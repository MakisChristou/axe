//! ITS remote deploy waiters. Not part of message flow
//! verification — these block until a one-shot ITS remote-token-deploy
//! propagates through the Axelar hub to the destination chain, so that
//! subsequent ITS transfers find the token already registered.

use alloy::providers::ProviderBuilder;
use solana_client::rpc_client::RpcClient;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::task;
use tokio::time;

use alloy::primitives::{Address, keccak256};
use alloy::providers::Provider;
use eyre::{Result, WrapErr};

use super::POLL_INTERVAL;
use super::checks::{
    check_evm_command_executed, check_evm_is_message_approved, check_solana_incoming_message,
};
use super::legacy;
use super::pipeline::{check_cosmos_routed, check_hub_approved, parse_payload_hash};
use crate::config::AxelarChainContract;
use crate::config::AxelarGlobalContract;
use crate::config::ChainsConfig;
use crate::cosmos::{SecondLegInfo, discover_second_leg, read_axelar_rpc};
use crate::evm::AxelarAmplifierGateway;
use crate::stellar::{MessageApprovalQuery, StellarClient};
use crate::types::Network;
use crate::ui;

pub struct StellarRemoteDeployWait {
    pub config: PathBuf,
    pub source_chain: String,
    pub destination_chain: String,
    pub deploy_message_id: String,
    pub stellar_rpc: String,
    pub network_type: String,
    pub gateway_contract: String,
    pub its_contract: String,
    pub signer_pk: [u8; 32],
    pub token_id: [u8; 32],
}

enum StellarRemoteApproval {
    Pending,
    Approved,
    Executed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoteDeployPhase {
    Voted,
    HubApproved,
    DiscoverSecondLeg,
    Routed,
    Approved,
    Executed,
    Registered,
    Done,
}

struct HubDeployContext<'a> {
    lcd: &'a str,
    rpc: &'a str,
    axelarnet_gateway: &'a str,
    destination_gateway: &'a str,
    source_chain: &'a str,
    deploy_message_id: &'a str,
    routed_source: RoutedSource,
    routed_label: &'a str,
    skip_routing: bool,
}

#[derive(Clone, Copy)]
enum RoutedSource {
    Axelar,
    Discovered,
}

struct RemoteDeployState {
    phase: RemoteDeployPhase,
    second_leg: Option<SecondLegInfo>,
    command_id: Option<[u8; 32]>,
}

async fn advance_hub_deploy(
    context: &HubDeployContext<'_>,
    state: &mut RemoteDeployState,
    spinner: &indicatif::ProgressBar,
) -> Result<bool> {
    match state.phase {
        RemoteDeployPhase::Voted | RemoteDeployPhase::HubApproved => {
            if check_hub_approved(
                context.lcd,
                context.axelarnet_gateway,
                context.source_chain,
                context.deploy_message_id,
            )
            .await
            .wrap_err("remote deploy hub approval check failed")?
            {
                spinner.set_message("remote deploy: hub approved");
                state.phase = RemoteDeployPhase::DiscoverSecondLeg;
                return Ok(true);
            }
            let message = if state.phase == RemoteDeployPhase::Voted {
                "remote deploy: waiting for voting..."
            } else {
                "remote deploy: waiting for hub approval..."
            };
            spinner.set_message(message);
            Ok(true)
        }
        RemoteDeployPhase::DiscoverSecondLeg => {
            match discover_second_leg(context.rpc, context.deploy_message_id).await {
                Ok(Some(info)) => {
                    spinner.set_message(format!(
                        "remote deploy: second leg discovered ({})",
                        info.message_id
                    ));
                    state.second_leg = Some(info);
                    state.phase = if context.skip_routing {
                        RemoteDeployPhase::Approved
                    } else {
                        RemoteDeployPhase::Routed
                    };
                }
                Ok(None) => spinner.set_message("remote deploy: discovering second leg..."),
                Err(error) => {
                    return Err(error.wrap_err("remote deploy second-leg discovery failed"));
                }
            }
            Ok(true)
        }
        RemoteDeployPhase::Routed => {
            let info = require_second_leg(&state.second_leg)?;
            let source_chain = match context.routed_source {
                RoutedSource::Axelar => "axelar",
                RoutedSource::Discovered => info.source_chain.as_str(),
            };
            if check_cosmos_routed(
                context.lcd,
                context.destination_gateway,
                source_chain,
                &info.message_id,
            )
            .await
            .wrap_err("remote deploy routing check failed")?
            {
                spinner.set_message(format!("remote deploy: routed to {}", context.routed_label));
                state.phase = RemoteDeployPhase::Approved;
            } else {
                spinner.set_message("remote deploy: waiting for routing...");
            }
            Ok(true)
        }
        _ => Ok(false),
    }
}

struct EvmDeployContext<'a, P: Provider> {
    gateway: &'a AxelarAmplifierGateway::AxelarAmplifierGatewayInstance<&'a P>,
    legacy: bool,
    from_block: u64,
}

async fn check_legacy_evm_deploy<P: Provider>(
    context: &EvmDeployContext<'_, P>,
    state: &mut RemoteDeployState,
    spinner: &indicatif::ProgressBar,
) -> Result<()> {
    match state.phase {
        RemoteDeployPhase::Approved => {
            let info = require_second_leg(&state.second_leg)?;
            let payload_hash = parse_payload_hash(&info.payload_hash)
                .wrap_err("remote deploy second-leg payload_hash is invalid")?;
            let destination: Address = info.destination_address.parse().wrap_err_with(|| {
                format!(
                    "invalid second-leg destination address {}",
                    info.destination_address
                )
            })?;
            if let Some(command_id) = legacy::find_contract_call_approved_by_payload(
                context.gateway.provider(),
                *context.gateway.address(),
                destination,
                payload_hash.into_fixed_bytes(),
                context.from_block,
            )
            .await?
            {
                state.command_id = Some(command_id);
                spinner.set_message("remote deploy: approved on legacy gateway");
                state.phase = RemoteDeployPhase::Executed;
            } else {
                spinner.set_message("remote deploy: waiting for legacy approval...");
            }
        }
        RemoteDeployPhase::Executed => {
            let command_id = state
                .command_id
                .ok_or_else(|| eyre::eyre!("remote deploy missing legacy commandId"))?;
            if check_evm_command_executed(context.gateway, command_id.into()).await? {
                state.phase = RemoteDeployPhase::Done;
            } else {
                spinner.set_message("remote deploy: waiting for legacy execution...");
            }
        }
        _ => {}
    }
    Ok(())
}

async fn check_amplifier_evm_deploy<P: Provider>(
    context: &EvmDeployContext<'_, P>,
    state: &mut RemoteDeployState,
    spinner: &indicatif::ProgressBar,
) -> Result<()> {
    let info = require_second_leg(&state.second_leg)?;
    let payload_hash = parse_payload_hash(&info.payload_hash)
        .wrap_err("remote deploy second-leg payload_hash is invalid")?;
    let approved = check_evm_is_message_approved(
        context.gateway,
        "axelar",
        &info.message_id,
        "",
        Address::ZERO,
        payload_hash.into_fixed_bytes(),
    )
    .await;
    match (state.phase, approved) {
        (RemoteDeployPhase::Approved, Ok(true)) => {
            spinner.set_message("remote deploy: approved on EVM");
            state.phase = RemoteDeployPhase::Executed;
        }
        (RemoteDeployPhase::Approved, Ok(false)) => {
            state.phase = RemoteDeployPhase::Executed;
        }
        (RemoteDeployPhase::Executed, Ok(false)) => {
            state.phase = RemoteDeployPhase::Done;
        }
        (RemoteDeployPhase::Executed, Ok(true)) => {
            spinner.set_message("remote deploy: waiting for EVM execution...");
        }
        // A destination-RPC blip on one poll cycle must not abort the wait —
        // warn, keep the phase, and let the next cycle (or the wait's own
        // timeout) resolve it.
        (RemoteDeployPhase::Approved, Err(error)) => {
            ui::warn(&format!(
                "remote deploy EVM approval check failed (retrying next poll): {error}"
            ));
        }
        (RemoteDeployPhase::Executed, Err(error)) => {
            ui::warn(&format!(
                "remote deploy EVM execution check failed (retrying next poll): {error}"
            ));
        }
        _ => {}
    }
    Ok(())
}

async fn advance_evm_destination<P: Provider>(
    context: &EvmDeployContext<'_, P>,
    state: &mut RemoteDeployState,
    spinner: &indicatif::ProgressBar,
) -> Result<()> {
    if context.legacy {
        check_legacy_evm_deploy(context, state, spinner).await
    } else {
        check_amplifier_evm_deploy(context, state, spinner).await
    }
}

fn ensure_not_timed_out(
    start: Instant,
    timeout: Duration,
    phase: RemoteDeployPhase,
    spinner: &indicatif::ProgressBar,
) -> Result<()> {
    if start.elapsed() < timeout {
        return Ok(());
    }
    spinner.finish_and_clear();
    eyre::bail!(
        "remote deploy timed out after {}s at phase {phase:?}",
        timeout.as_secs()
    )
}

/// Wait for an ITS remote deploy message to propagate through the hub pipeline
/// and execute on the EVM destination. The deploy message ID is `{sig}-1.3`.
///
/// Polls: Voted → HubApproved → DiscoverSecondLeg → Routed → Executed(EVM)
pub async fn wait_for_its_remote_deploy(
    config: &Path,
    source_chain: &str,
    destination_chain: &str,
    deploy_message_id: &str,
    evm_gateway_addr: Address,
    evm_rpc_url: &str,
) -> Result<()> {
    let cfg = ChainsConfig::load(config).await?;
    let (lcd, _, _, _) = cfg.axelar.cosmos_tx_params()?;
    let rpc = read_axelar_rpc(config).await?;

    let axelarnet_gateway = cfg
        .axelar
        .global_contract_address(AxelarGlobalContract::AxelarnetGateway)?
        .to_string();

    let voting_verifier = cfg
        .axelar
        .contract_address(AxelarChainContract::VotingVerifier, source_chain)
        .ok()
        .map(String::from);

    // A legacy (consensus) destination has no Cosmos Gateway: skip the `routed`
    // phase and verify the deploy's second leg on the legacy gateway via events.
    let dest_legacy = cfg
        .axelar
        .contract_address(AxelarChainContract::VotingVerifier, destination_chain)
        .is_err();
    let cosm_gateway_dest = if dest_legacy {
        String::new()
    } else {
        cfg.axelar
            .contract_address(AxelarChainContract::Gateway, destination_chain)?
            .to_string()
    };

    let provider = ProviderBuilder::new().connect_http(evm_rpc_url.parse()?);
    let gw_contract = AxelarAmplifierGateway::new(evm_gateway_addr, &provider);
    let from_block = if dest_legacy {
        provider
            .get_block_number()
            .await?
            .saturating_sub(super::LEGACY_LOG_LOOKBACK_BLOCKS)
    } else {
        0
    };

    ui::kv("deploy message ID", deploy_message_id);
    let spinner = ui::wait_spinner("waiting for remote deploy to propagate through hub...");
    let start = Instant::now();
    let timeout = Duration::from_secs(500);
    let hub_context = HubDeployContext {
        lcd: &lcd,
        rpc: &rpc,
        axelarnet_gateway: &axelarnet_gateway,
        destination_gateway: &cosm_gateway_dest,
        source_chain,
        deploy_message_id,
        routed_source: RoutedSource::Axelar,
        routed_label: "destination",
        skip_routing: dest_legacy,
    };
    let evm_context = EvmDeployContext {
        gateway: &gw_contract,
        legacy: dest_legacy,
        from_block,
    };
    let mut state = RemoteDeployState {
        phase: if voting_verifier.is_some() {
            RemoteDeployPhase::Voted
        } else {
            RemoteDeployPhase::HubApproved
        },
        second_leg: None,
        command_id: None,
    };

    while state.phase != RemoteDeployPhase::Done {
        ensure_not_timed_out(start, timeout, state.phase, &spinner)?;
        if advance_hub_deploy(&hub_context, &mut state, &spinner).await? {
            time::sleep(POLL_INTERVAL).await;
            continue;
        }
        advance_evm_destination(&evm_context, &mut state, &spinner).await?;
        if state.phase != RemoteDeployPhase::Done {
            time::sleep(POLL_INTERVAL).await;
        }
    }

    spinner.finish_and_clear();
    ui::success("remote token deployed on destination chain");
    Ok(())
}

/// Wait for a remote ITS token deploy to propagate through the hub and execute
/// on Stellar. This proves the token is registered before EVM→Stellar
/// transfers are sent.
async fn advance_stellar_destination(
    client: &StellarClient,
    args: &StellarRemoteDeployWait,
    state: &mut RemoteDeployState,
    spinner: &indicatif::ProgressBar,
) -> Result<()> {
    let info = require_second_leg(&state.second_leg)?;
    match state.phase {
        RemoteDeployPhase::Approved => {
            match check_stellar_remote_deploy_approval(client, args, info).await? {
                StellarRemoteApproval::Approved => {
                    spinner.set_message("remote deploy: approved on Stellar");
                    state.phase = RemoteDeployPhase::Executed;
                }
                StellarRemoteApproval::Executed => {
                    spinner.set_message("remote deploy: executed on Stellar");
                    state.phase = RemoteDeployPhase::Registered;
                }
                StellarRemoteApproval::Pending => {
                    spinner.set_message("remote deploy: waiting for Stellar approval...");
                }
            }
        }
        RemoteDeployPhase::Executed => {
            if check_stellar_remote_deploy_executed(client, args, info).await? {
                spinner.set_message("remote deploy: executed on Stellar");
                state.phase = RemoteDeployPhase::Registered;
            } else {
                spinner.set_message("remote deploy: waiting for Stellar execution...");
            }
        }
        RemoteDeployPhase::Registered => {
            match client
                .its_registered_token_address_view(
                    &args.signer_pk,
                    &args.its_contract,
                    args.token_id,
                )
                .await
                .wrap_err("remote deploy Stellar token registration check failed")?
            {
                Some(token_address) => {
                    ui::address("Stellar token", &token_address);
                    state.phase = RemoteDeployPhase::Done;
                }
                None => {
                    spinner.set_message("remote deploy: waiting for Stellar token registration...");
                }
            }
        }
        _ => {}
    }
    Ok(())
}

pub async fn wait_for_its_remote_deploy_to_stellar(args: StellarRemoteDeployWait) -> Result<()> {
    let cfg = ChainsConfig::load(&args.config).await?;
    let (lcd, _, _, _) = cfg.axelar.cosmos_tx_params()?;
    let rpc = read_axelar_rpc(&args.config).await?;

    let axelarnet_gateway = cfg
        .axelar
        .global_contract_address(AxelarGlobalContract::AxelarnetGateway)?
        .to_string();

    let cosm_gateway_dest = cfg
        .axelar
        .contract_address(AxelarChainContract::Gateway, &args.destination_chain)?
        .to_string();

    let stellar_client = StellarClient::new(&args.stellar_rpc, &args.network_type)?;

    ui::kv("deploy message ID", &args.deploy_message_id);
    let spinner =
        ui::wait_spinner("waiting for remote deploy to propagate through hub to Stellar...");
    let start = Instant::now();
    let timeout = Duration::from_secs(500);
    let hub_context = HubDeployContext {
        lcd: &lcd,
        rpc: &rpc,
        axelarnet_gateway: &axelarnet_gateway,
        destination_gateway: &cosm_gateway_dest,
        source_chain: &args.source_chain,
        deploy_message_id: &args.deploy_message_id,
        routed_source: RoutedSource::Discovered,
        routed_label: "Stellar",
        skip_routing: false,
    };
    let mut state = RemoteDeployState {
        phase: RemoteDeployPhase::HubApproved,
        second_leg: None,
        command_id: None,
    };

    while state.phase != RemoteDeployPhase::Done {
        ensure_not_timed_out(start, timeout, state.phase, &spinner)?;
        if !advance_hub_deploy(&hub_context, &mut state, &spinner).await? {
            advance_stellar_destination(&stellar_client, &args, &mut state, &spinner).await?;
        }
        if state.phase != RemoteDeployPhase::Done {
            time::sleep(POLL_INTERVAL).await;
        }
    }

    spinner.finish_and_clear();
    ui::success("remote token deployed on Stellar");
    Ok(())
}

fn require_second_leg(second_leg: &Option<SecondLegInfo>) -> Result<&SecondLegInfo> {
    second_leg
        .as_ref()
        .ok_or_else(|| eyre::eyre!("remote deploy missing second-leg metadata"))
}

async fn check_stellar_remote_deploy_approval(
    client: &StellarClient,
    args: &StellarRemoteDeployWait,
    info: &SecondLegInfo,
) -> Result<StellarRemoteApproval> {
    let payload_hash = parse_payload_hash(&info.payload_hash)
        .wrap_err("remote deploy second-leg payload_hash is invalid")?;
    let approved = client
        .gateway_is_message_approved(MessageApprovalQuery {
            signer_account_pk: &args.signer_pk,
            gateway_contract: &args.gateway_contract,
            source_chain: &info.source_chain,
            message_id: &info.message_id,
            source_address: &info.source_address,
            contract_address: &info.destination_address,
            payload_hash: payload_hash.into_fixed_bytes().into(),
        })
        .await?
        .ok_or_else(|| eyre::eyre!("Stellar gateway returned non-bool approval result"))?;

    if approved {
        return Ok(StellarRemoteApproval::Approved);
    }

    if check_stellar_remote_deploy_executed(client, args, info).await? {
        return Ok(StellarRemoteApproval::Executed);
    }

    Ok(StellarRemoteApproval::Pending)
}

async fn check_stellar_remote_deploy_executed(
    client: &StellarClient,
    args: &StellarRemoteDeployWait,
    info: &SecondLegInfo,
) -> Result<bool> {
    client
        .gateway_is_message_executed(
            &args.signer_pk,
            &args.gateway_contract,
            &info.source_chain,
            &info.message_id,
        )
        .await?
        .ok_or_else(|| eyre::eyre!("Stellar gateway returned non-bool execution result"))
}

/// Wait for a remote ITS token deploy to propagate through the hub and reach Solana.
///
/// Similar to `wait_for_its_remote_deploy` but for EVM→Solana direction.
/// Polls: Voted → HubApproved → DiscoverSecondLeg → Routed → Done
/// (We don't check Solana approval/execution — once routed, the Solana relayer
/// handles it. We just need the token to exist before sending transfers.)
async fn check_solana_deploy_approval(
    client: Arc<RpcClient>,
    network: Network,
    state: &mut RemoteDeployState,
    not_found_count: &mut u32,
    spinner: &indicatif::ProgressBar,
) -> Result<()> {
    let info = require_second_leg(&state.second_leg)?;
    let input = [b"axelar-".as_slice(), info.message_id.as_bytes()].concat();
    let command_id: [u8; 32] = keccak256(&input).into();
    let status =
        task::spawn_blocking(move || check_solana_incoming_message(&client, network, &command_id))
            .await
            .wrap_err("Solana deploy approval task failed")?;

    match status {
        Ok(Some(_)) => state.phase = RemoteDeployPhase::Done,
        Ok(None) => {
            *not_found_count += 1;
            if *not_found_count >= 10 {
                spinner.set_message("remote deploy: PDA not found, assuming already executed");
                state.phase = RemoteDeployPhase::Done;
            } else {
                spinner.set_message("remote deploy: waiting for Solana approval...");
            }
        }
        Err(error) => {
            return Err(error.wrap_err("remote deploy Solana approval check failed"));
        }
    }
    Ok(())
}

pub async fn wait_for_its_remote_deploy_to_solana(
    config: &Path,
    source_chain: &str,
    destination_chain: &str,
    deploy_message_id: &str,
    solana_rpc: &str,
    network: Network,
) -> Result<()> {
    let cfg = ChainsConfig::load(config).await?;
    let (lcd, _, _, _) = cfg.axelar.cosmos_tx_params()?;
    let rpc = read_axelar_rpc(config).await?;

    let axelarnet_gateway = cfg
        .axelar
        .global_contract_address(AxelarGlobalContract::AxelarnetGateway)?
        .to_string();

    let cosm_gateway_dest = cfg
        .axelar
        .contract_address(AxelarChainContract::Gateway, destination_chain)?
        .to_string();

    let sol_rpc_client = Arc::new(RpcClient::new_with_commitment(
        solana_rpc,
        solana_commitment_config::CommitmentConfig::finalized(),
    ));

    ui::kv("deploy message ID", deploy_message_id);
    let spinner =
        ui::wait_spinner("waiting for remote deploy to propagate through hub to Solana...");
    let start = Instant::now();
    let timeout = Duration::from_secs(500);
    let hub_context = HubDeployContext {
        lcd: &lcd,
        rpc: &rpc,
        axelarnet_gateway: &axelarnet_gateway,
        destination_gateway: &cosm_gateway_dest,
        source_chain,
        deploy_message_id,
        routed_source: RoutedSource::Axelar,
        routed_label: "Solana",
        skip_routing: false,
    };
    let mut state = RemoteDeployState {
        phase: RemoteDeployPhase::HubApproved,
        second_leg: None,
        command_id: None,
    };
    let mut approved_not_found_count: u32 = 0;

    while state.phase != RemoteDeployPhase::Done {
        ensure_not_timed_out(start, timeout, state.phase, &spinner)?;
        if !advance_hub_deploy(&hub_context, &mut state, &spinner).await? {
            check_solana_deploy_approval(
                Arc::clone(&sol_rpc_client),
                network,
                &mut state,
                &mut approved_not_found_count,
                &spinner,
            )
            .await?;
        }
        if state.phase != RemoteDeployPhase::Done {
            time::sleep(POLL_INTERVAL).await;
        }
    }

    spinner.finish_and_clear();
    ui::success("remote token deployed on Solana");
    Ok(())
}
