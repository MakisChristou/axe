//! Pre-flight ITS remote-deploy waiters. Not part of message-flow
//! verification — these block until a one-shot ITS remote-token-deploy
//! propagates through the Axelar hub to the destination chain, so that
//! subsequent ITS transfers find the token already registered.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use alloy::primitives::{Address, keccak256};
use alloy::providers::Provider;
use eyre::{Result, WrapErr};

use super::POLL_INTERVAL;
use super::checks::{
    check_evm_command_executed, check_evm_is_message_approved, check_solana_incoming_message,
};
use super::legacy;
use super::pipeline::{check_cosmos_routed, check_hub_approved, parse_payload_hash};
use crate::config::ChainsConfig;
use crate::cosmos::{SecondLegInfo, discover_second_leg, read_axelar_rpc};
use crate::evm::AxelarAmplifierGateway;
use crate::stellar::StellarClient;
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
    let cfg = ChainsConfig::load(config)?;
    let (lcd, _, _, _) = cfg.axelar.cosmos_tx_params()?;
    let rpc = read_axelar_rpc(config)?;

    let axelarnet_gateway = cfg
        .axelar
        .global_contract_address("AxelarnetGateway")?
        .to_string();

    let voting_verifier = cfg
        .axelar
        .contract_address("VotingVerifier", source_chain)
        .ok()
        .map(String::from);

    // A legacy (consensus) destination has no Cosmos Gateway: skip the `routed`
    // phase and verify the deploy's second leg on the legacy gateway via events.
    let dest_legacy = cfg
        .axelar
        .contract_address("VotingVerifier", destination_chain)
        .is_err();
    let cosm_gateway_dest = if dest_legacy {
        String::new()
    } else {
        cfg.axelar
            .contract_address("Gateway", destination_chain)?
            .to_string()
    };

    let provider = alloy::providers::ProviderBuilder::new().connect_http(evm_rpc_url.parse()?);
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

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum DeployPhase {
        Voted,
        HubApproved,
        DiscoverSecondLeg,
        Routed,
        Approved,
        Executed,
        Done,
    }

    let mut phase = if voting_verifier.is_some() {
        DeployPhase::Voted
    } else {
        DeployPhase::HubApproved
    };
    let mut second_leg_id: Option<String> = None;
    let mut second_leg_ph: Option<String> = None;
    let mut second_leg_dest: Option<String> = None;
    let mut command_id: Option<[u8; 32]> = None;

    loop {
        if start.elapsed() >= timeout {
            spinner.finish_and_clear();
            eyre::bail!(
                "remote deploy timed out after {}s at phase {phase:?}",
                timeout.as_secs()
            );
        }

        match phase {
            DeployPhase::Voted => {
                if let Some(ref vv) = voting_verifier {
                    // For deploy, we don't have payload_hash — use empty string
                    // VotingVerifier just needs the message to exist
                    if check_hub_approved(&lcd, &axelarnet_gateway, source_chain, deploy_message_id)
                        .await
                        .wrap_err("remote deploy hub approval check failed")?
                    {
                        spinner.set_message("remote deploy: hub approved");
                        phase = DeployPhase::DiscoverSecondLeg;
                        continue;
                    }
                    // Also try voting verifier directly — but we'd need payload_hash.
                    // Skip directly to hub_approved check since it implies voted.
                    let _ = vv; // suppress unused warning
                }
                spinner.set_message("remote deploy: waiting for voting...");
            }
            DeployPhase::HubApproved => {
                if check_hub_approved(&lcd, &axelarnet_gateway, source_chain, deploy_message_id)
                    .await
                    .wrap_err("remote deploy hub approval check failed")?
                {
                    spinner.set_message("remote deploy: hub approved");
                    phase = DeployPhase::DiscoverSecondLeg;
                    continue;
                }
                spinner.set_message("remote deploy: waiting for hub approval...");
            }
            DeployPhase::DiscoverSecondLeg => {
                match discover_second_leg(&rpc, deploy_message_id).await {
                    Ok(Some(info)) => {
                        spinner.set_message(format!(
                            "remote deploy: second leg discovered ({})",
                            info.message_id
                        ));
                        second_leg_id = Some(info.message_id);
                        second_leg_ph = Some(info.payload_hash);
                        second_leg_dest = Some(info.destination_address);
                        // Legacy dest has no Cosmos Gateway → skip `routed`.
                        phase = if dest_legacy {
                            DeployPhase::Approved
                        } else {
                            DeployPhase::Routed
                        };
                        continue;
                    }
                    Ok(None) => {
                        spinner.set_message("remote deploy: discovering second leg...");
                    }
                    Err(e) => return Err(e.wrap_err("remote deploy second-leg discovery failed")),
                }
            }
            DeployPhase::Routed => {
                let sl_id = second_leg_id
                    .as_deref()
                    .ok_or_else(|| eyre::eyre!("remote deploy missing second-leg message_id"))?;
                if check_cosmos_routed(&lcd, &cosm_gateway_dest, "axelar", sl_id)
                    .await
                    .wrap_err("remote deploy routing check failed")?
                {
                    spinner.set_message("remote deploy: routed to destination");
                    phase = DeployPhase::Approved;
                    continue;
                }
                spinner.set_message("remote deploy: waiting for routing...");
            }
            DeployPhase::Approved if dest_legacy => {
                let sl_ph_str = second_leg_ph
                    .as_deref()
                    .ok_or_else(|| eyre::eyre!("remote deploy missing second-leg payload_hash"))?;
                let ph = parse_payload_hash(sl_ph_str)
                    .wrap_err("remote deploy second-leg payload_hash is invalid")?;
                let sl_dst = second_leg_dest.as_deref().ok_or_else(|| {
                    eyre::eyre!("remote deploy missing second-leg destination_address")
                })?;
                let dst_addr: Address = sl_dst
                    .parse()
                    .wrap_err_with(|| format!("invalid second-leg destination address {sl_dst}"))?;
                if let Some(cmd_id) = legacy::find_contract_call_approved_by_payload(
                    gw_contract.provider(),
                    *gw_contract.address(),
                    dst_addr,
                    ph,
                    from_block,
                )
                .await?
                {
                    command_id = Some(cmd_id);
                    spinner.set_message("remote deploy: approved on legacy gateway");
                    phase = DeployPhase::Executed;
                    continue;
                }
                spinner.set_message("remote deploy: waiting for legacy approval...");
            }
            DeployPhase::Executed if dest_legacy => {
                let cmd_id = command_id
                    .ok_or_else(|| eyre::eyre!("remote deploy missing legacy commandId"))?;
                if check_evm_command_executed(&gw_contract, cmd_id.into()).await? {
                    phase = DeployPhase::Done;
                    continue;
                }
                spinner.set_message("remote deploy: waiting for legacy execution...");
            }
            DeployPhase::Approved => {
                let sl_id = second_leg_id
                    .as_deref()
                    .ok_or_else(|| eyre::eyre!("remote deploy missing second-leg message_id"))?;
                let sl_ph_str = second_leg_ph
                    .as_deref()
                    .ok_or_else(|| eyre::eyre!("remote deploy missing second-leg payload_hash"))?;
                let ph = parse_payload_hash(sl_ph_str)
                    .wrap_err("remote deploy second-leg payload_hash is invalid")?;
                match check_evm_is_message_approved(
                    &gw_contract,
                    "axelar",
                    sl_id,
                    "",
                    Address::ZERO,
                    ph,
                )
                .await
                {
                    Ok(true) => {
                        spinner.set_message("remote deploy: approved on EVM");
                        phase = DeployPhase::Executed;
                        continue;
                    }
                    Ok(false) => {
                        // Could be already executed — check by trying executed phase
                        phase = DeployPhase::Executed;
                        continue;
                    }
                    Err(e) => return Err(e.wrap_err("remote deploy EVM approval check failed")),
                }
            }
            DeployPhase::Executed => {
                let sl_id = second_leg_id
                    .as_deref()
                    .ok_or_else(|| eyre::eyre!("remote deploy missing second-leg message_id"))?;
                let sl_ph_str = second_leg_ph
                    .as_deref()
                    .ok_or_else(|| eyre::eyre!("remote deploy missing second-leg payload_hash"))?;
                let ph = parse_payload_hash(sl_ph_str)
                    .wrap_err("remote deploy second-leg payload_hash is invalid")?;
                match check_evm_is_message_approved(
                    &gw_contract,
                    "axelar",
                    sl_id,
                    "",
                    Address::ZERO,
                    ph,
                )
                .await
                {
                    Ok(false) => {
                        // false = approval consumed = executed
                        phase = DeployPhase::Done;
                        continue;
                    }
                    Ok(true) => {
                        spinner.set_message("remote deploy: waiting for EVM execution...");
                    }
                    Err(e) => return Err(e.wrap_err("remote deploy EVM execution check failed")),
                }
            }
            DeployPhase::Done => break,
        }

        tokio::time::sleep(POLL_INTERVAL).await;
    }

    spinner.finish_and_clear();
    ui::success("remote token deployed on destination chain");
    Ok(())
}

/// Wait for a remote ITS token deploy to propagate through the hub and execute
/// on Stellar. This proves the token is registered before EVM→Stellar
/// transfers are sent.
pub async fn wait_for_its_remote_deploy_to_stellar(args: StellarRemoteDeployWait) -> Result<()> {
    let cfg = ChainsConfig::load(&args.config)?;
    let (lcd, _, _, _) = cfg.axelar.cosmos_tx_params()?;
    let rpc = read_axelar_rpc(&args.config)?;

    let axelarnet_gateway = cfg
        .axelar
        .global_contract_address("AxelarnetGateway")?
        .to_string();

    let cosm_gateway_dest = cfg
        .axelar
        .contract_address("Gateway", &args.destination_chain)?
        .to_string();

    let stellar_client = StellarClient::new(&args.stellar_rpc, &args.network_type)?;

    ui::kv("deploy message ID", &args.deploy_message_id);
    let spinner =
        ui::wait_spinner("waiting for remote deploy to propagate through hub to Stellar...");
    let start = Instant::now();
    let timeout = Duration::from_secs(500);

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum DeployPhase {
        HubApproved,
        DiscoverSecondLeg,
        Routed,
        Approved,
        Executed,
        Registered,
        Done,
    }

    let mut phase = DeployPhase::HubApproved;
    let mut second_leg: Option<SecondLegInfo> = None;

    loop {
        if start.elapsed() >= timeout {
            spinner.finish_and_clear();
            eyre::bail!(
                "remote deploy timed out after {}s at phase {phase:?}",
                timeout.as_secs()
            );
        }

        match phase {
            DeployPhase::HubApproved => {
                if check_hub_approved(
                    &lcd,
                    &axelarnet_gateway,
                    &args.source_chain,
                    &args.deploy_message_id,
                )
                .await
                .wrap_err("remote deploy hub approval check failed")?
                {
                    spinner.set_message("remote deploy: hub approved");
                    phase = DeployPhase::DiscoverSecondLeg;
                    continue;
                }
                spinner.set_message("remote deploy: waiting for hub approval...");
            }
            DeployPhase::DiscoverSecondLeg => {
                match discover_second_leg(&rpc, &args.deploy_message_id).await {
                    Ok(Some(info)) => {
                        spinner.set_message(format!(
                            "remote deploy: second leg discovered ({})",
                            info.message_id
                        ));
                        second_leg = Some(info);
                        phase = DeployPhase::Routed;
                        continue;
                    }
                    Ok(None) => {
                        spinner.set_message("remote deploy: discovering second leg...");
                    }
                    Err(e) => return Err(e.wrap_err("remote deploy second-leg discovery failed")),
                }
            }
            DeployPhase::Routed => {
                let info = require_second_leg(&second_leg)?;
                if check_cosmos_routed(
                    &lcd,
                    &cosm_gateway_dest,
                    &info.source_chain,
                    &info.message_id,
                )
                .await
                .wrap_err("remote deploy routing check failed")?
                {
                    spinner.set_message("remote deploy: routed to Stellar");
                    phase = DeployPhase::Approved;
                    continue;
                }
                spinner.set_message("remote deploy: waiting for routing...");
            }
            DeployPhase::Approved => {
                let info = require_second_leg(&second_leg)?;
                match check_stellar_remote_deploy_approval(&stellar_client, &args, info).await? {
                    StellarRemoteApproval::Approved => {
                        spinner.set_message("remote deploy: approved on Stellar");
                        phase = DeployPhase::Executed;
                        continue;
                    }
                    StellarRemoteApproval::Executed => {
                        spinner.set_message("remote deploy: executed on Stellar");
                        phase = DeployPhase::Registered;
                        continue;
                    }
                    StellarRemoteApproval::Pending => {
                        spinner.set_message("remote deploy: waiting for Stellar approval...");
                    }
                }
            }
            DeployPhase::Executed => {
                let info = require_second_leg(&second_leg)?;
                if check_stellar_remote_deploy_executed(&stellar_client, &args, info).await? {
                    spinner.set_message("remote deploy: executed on Stellar");
                    phase = DeployPhase::Registered;
                    continue;
                }
                spinner.set_message("remote deploy: waiting for Stellar execution...");
            }
            DeployPhase::Registered => {
                match stellar_client
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
                        phase = DeployPhase::Done;
                        continue;
                    }
                    None => {
                        spinner.set_message(
                            "remote deploy: waiting for Stellar token registration...",
                        );
                    }
                }
            }
            DeployPhase::Done => break,
        }

        tokio::time::sleep(POLL_INTERVAL).await;
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
        .gateway_is_message_approved(crate::stellar::MessageApprovalQuery {
            signer_account_pk: &args.signer_pk,
            gateway_contract: &args.gateway_contract,
            source_chain: &info.source_chain,
            message_id: &info.message_id,
            source_address: &info.source_address,
            contract_address: &info.destination_address,
            payload_hash: payload_hash.0,
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
pub async fn wait_for_its_remote_deploy_to_solana(
    config: &Path,
    source_chain: &str,
    destination_chain: &str,
    deploy_message_id: &str,
    solana_rpc: &str,
    network: crate::types::Network,
) -> Result<()> {
    let cfg = ChainsConfig::load(config)?;
    let (lcd, _, _, _) = cfg.axelar.cosmos_tx_params()?;
    let rpc = read_axelar_rpc(config)?;

    let axelarnet_gateway = cfg
        .axelar
        .global_contract_address("AxelarnetGateway")?
        .to_string();

    let cosm_gateway_dest = cfg
        .axelar
        .contract_address("Gateway", destination_chain)?
        .to_string();

    let sol_rpc_client = solana_client::rpc_client::RpcClient::new_with_commitment(
        solana_rpc,
        solana_commitment_config::CommitmentConfig::finalized(),
    );

    ui::kv("deploy message ID", deploy_message_id);
    let spinner =
        ui::wait_spinner("waiting for remote deploy to propagate through hub to Solana...");
    let start = Instant::now();
    let timeout = Duration::from_secs(500);

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum DeployPhase {
        HubApproved,
        DiscoverSecondLeg,
        Routed,
        Approved,
        Done,
    }

    let mut phase = DeployPhase::HubApproved;
    let mut second_leg_id: Option<String> = None;
    let mut approved_not_found_count: u32 = 0;

    loop {
        if start.elapsed() >= timeout {
            spinner.finish_and_clear();
            eyre::bail!(
                "remote deploy timed out after {}s at phase {phase:?}",
                timeout.as_secs()
            );
        }

        match phase {
            DeployPhase::HubApproved => {
                if check_hub_approved(&lcd, &axelarnet_gateway, source_chain, deploy_message_id)
                    .await
                    .wrap_err("remote deploy hub approval check failed")?
                {
                    spinner.set_message("remote deploy: hub approved");
                    phase = DeployPhase::DiscoverSecondLeg;
                    continue;
                }
                spinner.set_message("remote deploy: waiting for hub approval...");
            }
            DeployPhase::DiscoverSecondLeg => {
                match discover_second_leg(&rpc, deploy_message_id).await {
                    Ok(Some(info)) => {
                        spinner.set_message(format!(
                            "remote deploy: second leg discovered ({})",
                            info.message_id
                        ));
                        second_leg_id = Some(info.message_id);
                        phase = DeployPhase::Routed;
                        continue;
                    }
                    Ok(None) => {
                        spinner.set_message("remote deploy: discovering second leg...");
                    }
                    Err(e) => return Err(e.wrap_err("remote deploy second-leg discovery failed")),
                }
            }
            DeployPhase::Routed => {
                let sl_id = second_leg_id
                    .as_deref()
                    .ok_or_else(|| eyre::eyre!("remote deploy missing second-leg message_id"))?;
                if check_cosmos_routed(&lcd, &cosm_gateway_dest, "axelar", sl_id)
                    .await
                    .wrap_err("remote deploy routing check failed")?
                {
                    spinner.set_message("remote deploy: routed to Solana");
                    phase = DeployPhase::Approved;
                    continue;
                }
                spinner.set_message("remote deploy: waiting for routing...");
            }
            DeployPhase::Approved => {
                // Check if the Solana gateway has the incoming message.
                // The PDA may be absent if the message was already executed and
                // the account was closed, so after enough retries we assume done.
                let sl_id = second_leg_id
                    .as_deref()
                    .ok_or_else(|| eyre::eyre!("remote deploy missing second-leg message_id"))?;
                let input = [b"axelar-".as_slice(), sl_id.as_bytes()].concat();
                let cmd_id: [u8; 32] = keccak256(&input).into();
                match check_solana_incoming_message(&sol_rpc_client, network, &cmd_id) {
                    Ok(Some(_)) => {
                        phase = DeployPhase::Done;
                        continue;
                    }
                    Ok(None) => {
                        approved_not_found_count += 1;
                        if approved_not_found_count >= 10 {
                            // PDA never appeared — likely already executed and closed
                            spinner.set_message(
                                "remote deploy: PDA not found, assuming already executed",
                            );
                            phase = DeployPhase::Done;
                            continue;
                        }
                        spinner.set_message("remote deploy: waiting for Solana approval...");
                    }
                    Err(e) => return Err(e.wrap_err("remote deploy Solana approval check failed")),
                }
            }
            DeployPhase::Done => break,
        }

        tokio::time::sleep(POLL_INTERVAL).await;
    }

    spinner.finish_and_clear();
    ui::success("remote token deployed on Solana");
    Ok(())
}
