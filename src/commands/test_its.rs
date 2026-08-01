//! ITS smoke tests. Two entry points live here:
//!
//! - [`run`]: the legacy EVM-direct flow keyed on a single `axelar_id` from
//!   on-disk state. Deploys an interchain token + remote, then sends a
//!   transfer, manually relaying both legs through the Amplifier pipeline.
//! - [`run_config`]: the modern config-driven flow that takes any
//!   `(source, destination)` pair from `mainnet.json`-style config. Phase A
//!   deploys (with cache reuse) and Phase B transfers, again with manual
//!   relay through both legs.
//!
//! The shared helpers — encoding, cache, per-phase drivers, and the
//! amplifier relay sequence — live in submodules:
//! - [`encoding`]: borsh + ABI payload encoders.
//! - [`cache`]: phase-A token-deploy cache so reruns can skip the deploy.
//! - [`phase_a`]: Phase-A driver + the destination-token poll.
//! - [`phase_b`]: Phase-B preflight + the receiver-balance poll.
//! - [`relay`]: source→hub and hub→destination relay sequences.

mod cache;
mod encoding;
mod phase_a;
mod phase_b;
mod relay;
mod remediation;

use std::path::PathBuf;
use std::time::Instant;

use alloy::{
    primitives::{Bytes, FixedBytes, U256},
    providers::ProviderBuilder,
    signers::local::PrivateKeySigner,
};
use eyre::Result;
use serde_json::json;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Keypair;
use solana_sdk::signer::Signer as SolSigner;
use tokio::task::spawn_blocking;

use cache::{cache_path, try_load_cached_phase_a};
use encoding::{encode_inner_transfer, encode_receive_from_hub};
use phase_a::{PhaseADeployRequest, poll_for_remote_token_deploy, run_phase_a_deploy};
use phase_b::{
    check_destination_trusts_source, poll_for_balance_on_destination, resolve_hub_address_evm_view,
};
use relay::{DestinationRelayRequest, HubRelayRequest, relay_to_destination, relay_to_hub};
use remediation::print_untrusted_chain_remediation;

use crate::cli::resolve_axelar_id;
pub use crate::commands::event_extractors::{
    extract_contract_call_event, extract_token_deployed_event, generate_salt,
};
use crate::commands::test_helpers::{
    CosmosTxContext, end_poll_with_retry, execute_on_axelarnet_gateway, route_messages_with_retry,
    submit_verify_messages_amplifier, wait_for_poll_votes,
};
use crate::config::{AxelarChainContract, AxelarGlobalContract, ChainContract, ChainsConfig};
use crate::cosmos::derive_axelar_wallet;
use crate::evm::{ERC20, InterchainToken, InterchainTokenFactory, InterchainTokenService};
use crate::preflight;
use crate::state::read_state;
use crate::types::{ChainAxelarId, ChainType, Network};
use crate::ui;

const TOTAL_STEPS: usize = 10;

// Destination chain (Amplifier chain with an active relayer)
const DEST_CHAIN: &str = "hedera";

const PHASE_B_STEPS: usize = 9;

/// Default cross-chain gas budget (in source-chain native units, lamports for
/// SVM senders) for an ITS `deployRemoteInterchainToken` / link-token
/// proposal. 0.01 SOL covers the relay round-trip with comfortable headroom
/// at testnet rates.
const DEFAULT_ITS_GAS_VALUE_LAMPORTS: u64 = 10_000_000;

pub struct ConfigArgs {
    pub config: PathBuf,
    pub network: crate::types::Network,
    pub source_chain: Option<String>,
    pub destination_chain: Option<String>,
    pub mnemonic_override: Option<String>,
    pub evm_private_key_override: Option<String>,
    pub amount: Option<u64>,
    pub gas_value: Option<u64>,
    pub fresh_token: bool,
}

/// Default ITS interchain-transfer amount in token base units. The SVM-side
/// test token mints with 9 decimals, so 1_000_000 base units = 0.001 token —
/// large enough for a balance-poll signal, small enough not to dust mints.
const DEFAULT_ITS_TRANSFER_AMOUNT_BASE_UNITS: u64 = 1_000_000;

// Token parameters live in `crate::types::EVM_LEGACY_SPEC` (legacy EVM-direct
// `run`) and `ITS_CONFIG_SPEC` (config-mode `run_config`).
//
// ITS message-type discriminators are the `ItsMessageType` enum in `types.rs`.
//
// Cache files are namespaced by the resolved `Network` so a `mainnet` run
// doesn't read a `testnet` deploy from disk.

struct LegacyMessage {
    message_id: String,
    source_address: String,
    payload: Vec<u8>,
    payload_hash: FixedBytes<32>,
    destination_chain: String,
    destination_address: String,
}

struct LegacyDeployment {
    token_id: FixedBytes<32>,
    local_token: alloy::primitives::Address,
    message: LegacyMessage,
}

async fn deploy_legacy_token<P: alloy::providers::Provider>(
    provider: &P,
    factory_address: alloy::primitives::Address,
    its_address: alloy::primitives::Address,
    deployer: alloy::primitives::Address,
) -> Result<Option<LegacyDeployment>> {
    ui::step_header(1, TOTAL_STEPS, "Deploy interchain token");
    let salt = generate_salt();
    let spec = crate::types::EVM_LEGACY_SPEC;
    let initial_supply = crate::types::whole_tokens(1000, spec.decimals);
    ui::kv("name", spec.name);
    ui::kv("symbol", spec.symbol);
    ui::kv("decimals", &spec.decimals.to_string());
    ui::kv("initial supply", &format!("{initial_supply}"));
    ui::kv("salt", &format!("{salt}"));
    let factory = InterchainTokenFactory::new(factory_address, provider);
    let receipt = crate::evm::broadcast_and_log(
        factory
            .deployInterchainToken(
                salt,
                spec.name.to_string(),
                spec.symbol.to_string(),
                spec.decimals,
                initial_supply,
                deployer,
            )
            .value(U256::ZERO)
            .send()
            .await?,
        "tx",
    )
    .await?;
    let (token_id, local_token) = extract_token_deployed_event(&receipt)?;
    ui::kv("tokenId", &format!("{token_id}"));
    ui::address("local token", &format!("{local_token}"));
    let on_chain_id = InterchainToken::new(local_token, provider)
        .interchainTokenId()
        .call()
        .await?;
    if on_chain_id != token_id {
        return Err(eyre::eyre!(
            "tokenId mismatch: event={token_id} on-chain={on_chain_id}"
        ));
    }
    ui::success("tokenId verified on-chain");

    ui::step_header(2, TOTAL_STEPS, "Deploy remote interchain token to flow");
    let gas_value = crate::types::eth(2);
    ui::kv("destination", DEST_CHAIN);
    ui::kv("gas value", &format!("{gas_value} wei"));
    let pending = match factory
        .deployRemoteInterchainToken(salt, DEST_CHAIN.to_string(), gas_value)
        .value(gas_value)
        .send()
        .await
    {
        Ok(pending) => pending,
        Err(error) => {
            let debug = format!("{error:?}");
            if debug.contains("f9188a68") || debug.contains("UntrustedChain") {
                ui::error("UntrustedChain() — the destination chain is not trusted by this ITS");
                ui::info("Run `test its` again for detailed remediation steps.");
                return Ok(None);
            }
            return Err(error.into());
        }
    };
    let tx_hash = *pending.tx_hash();
    let receipt = crate::evm::broadcast_and_log(pending, "tx").await?;
    let (index, payload, payload_hash, destination_chain, destination_address) =
        extract_contract_call_event(&receipt)?;
    let message = LegacyMessage {
        message_id: format!("{tx_hash:#x}-{index}"),
        source_address: format!("{its_address}"),
        payload,
        payload_hash,
        destination_chain,
        destination_address,
    };
    ui::kv("message_id", &message.message_id);
    ui::kv("payload_hash", &format!("{}", message.payload_hash));
    ui::kv("destination_chain", &message.destination_chain);
    ui::kv("destination_address", &message.destination_address);
    ui::kv("source_address", &message.source_address);
    Ok(Some(LegacyDeployment {
        token_id,
        local_token,
        message,
    }))
}

struct LegacyRelayContext {
    signing_key: cosmrs::crypto::secp256k1::SigningKey,
    axelar_address: String,
    lcd: String,
    chain_id: String,
    fee_denom: String,
    gas_price: f64,
    cosm_gateway: String,
    voting_verifier: String,
    axelarnet_gateway: String,
}

impl LegacyRelayContext {
    fn tx(&self) -> CosmosTxContext<'_> {
        CosmosTxContext {
            signing_key: &self.signing_key,
            axelar_address: &self.axelar_address,
            lcd: &self.lcd,
            chain_id: &self.chain_id,
            fee_denom: &self.fee_denom,
            gas_price: self.gas_price,
        }
    }
}

fn prepare_legacy_relay(
    cfg: &ChainsConfig,
    mnemonic: &str,
    axelar_id: &str,
) -> Result<LegacyRelayContext> {
    let (signing_key, axelar_address) = derive_axelar_wallet(mnemonic)?;
    let (lcd, chain_id, fee_denom, gas_price) = cfg.axelar.cosmos_tx_params()?;
    let cosm_gateway = cfg
        .axelar
        .contract_address(AxelarChainContract::Gateway, axelar_id)?
        .to_string();
    let voting_verifier = cfg
        .axelar
        .contract_address(AxelarChainContract::VotingVerifier, axelar_id)?
        .to_string();
    let axelarnet_gateway = cfg
        .axelar
        .global_contract_address(AxelarGlobalContract::AxelarnetGateway)?
        .to_string();
    ui::address("cosmos gateway", &cosm_gateway);
    ui::address("voting verifier", &voting_verifier);
    ui::address("axelar address", &axelar_address);
    Ok(LegacyRelayContext {
        signing_key,
        axelar_address,
        lcd,
        chain_id,
        fee_denom,
        gas_price,
        cosm_gateway,
        voting_verifier,
        axelarnet_gateway,
    })
}

async fn relay_legacy_deployment(
    axelar_id: &str,
    message: &LegacyMessage,
    relay: &LegacyRelayContext,
) -> Result<()> {
    ui::section("Amplifier Routing (source → hub)");
    let its_message = json!({
        "cc_id": {
            "message_id": message.message_id,
            "source_chain": axelar_id,
        },
        "destination_chain": message.destination_chain,
        "destination_address": message.destination_address,
        "source_address": message.source_address,
        "payload_hash": alloy::hex::encode(message.payload_hash.as_slice()),
    });
    ui::step_header(3, TOTAL_STEPS, "verify_messages");
    let poll_id =
        submit_verify_messages_amplifier(&its_message, relay.tx(), &relay.cosm_gateway).await?;
    if let Some(poll_id) = poll_id {
        ui::kv("poll_id", &poll_id);
        ui::step_header(4, TOTAL_STEPS, "Wait for poll votes + end poll");
        wait_for_poll_votes(&relay.lcd, &relay.voting_verifier, &poll_id).await?;
        end_poll_with_retry(&poll_id, relay.tx(), &relay.voting_verifier).await?;
    } else {
        ui::info("no new poll created — message already being verified by active verifiers");
        ui::step_header(4, TOTAL_STEPS, "Wait for poll votes + end poll");
        ui::info("skipped (existing poll)");
    }
    ui::step_header(5, TOTAL_STEPS, "route_messages");
    route_messages_with_retry(&its_message, relay.tx(), &relay.cosm_gateway).await?;
    ui::step_header(6, TOTAL_STEPS, "Execute on AxelarnetGateway (hub)");
    ui::address("AxelarnetGateway", &relay.axelarnet_gateway);
    execute_on_axelarnet_gateway(
        &message.message_id,
        axelar_id,
        DEST_CHAIN,
        &message.payload,
        relay.tx(),
        &relay.axelarnet_gateway,
    )
    .await
}

struct LegacyTransferRequest<'a, P, D> {
    axelar_id: &'a str,
    local_token: alloy::primitives::Address,
    source_its: alloy::primitives::Address,
    destination_token: alloy::primitives::Address,
    source_provider: &'a P,
    destination_provider: &'a D,
    relay: &'a LegacyRelayContext,
}

async fn run_legacy_transfer<P, D>(request: LegacyTransferRequest<'_, P, D>) -> Result<()>
where
    P: alloy::providers::Provider,
    D: alloy::providers::Provider,
{
    ui::step_header(8, TOTAL_STEPS, "Send interchain transfer");
    let amount = crate::types::whole_tokens(100, crate::types::EVM_LEGACY_SPEC.decimals);
    let receiver = crate::types::DEAD_ADDRESS;
    let gas_value = crate::types::eth_milli(200);
    ui::kv("amount", &format!("{amount}"));
    ui::address("receiver", &format!("{receiver}"));
    ui::kv("gas value", &format!("{gas_value} wei"));
    let pending = InterchainToken::new(request.local_token, request.source_provider)
        .interchainTransfer(
            DEST_CHAIN.to_string(),
            Bytes::copy_from_slice(receiver.as_slice()),
            amount,
            Bytes::new(),
        )
        .value(gas_value)
        .send()
        .await?;
    let tx_hash = *pending.tx_hash();
    let receipt = crate::evm::broadcast_and_log(pending, "tx").await?;
    let (index, payload, payload_hash, destination_chain, destination_address) =
        extract_contract_call_event(&receipt)?;
    let message_id = format!("{tx_hash:#x}-{index}");
    ui::kv("message_id", &message_id);
    ui::kv("destination_chain", &destination_chain);

    ui::step_header(9, TOTAL_STEPS, "Relay transfer to hub");
    let source_address = format!("{}", request.source_its);
    relay_to_hub(HubRelayRequest {
        axelar_id: request.axelar_id,
        message_id: &message_id,
        source_address: &source_address,
        destination_chain: &destination_chain,
        destination_address: &destination_address,
        payload_hash: &payload_hash,
        payload: &payload,
        tx: request.relay.tx(),
        cosm_gateway: &request.relay.cosm_gateway,
        voting_verifier: &request.relay.voting_verifier,
        axelarnet_gateway: &request.relay.axelarnet_gateway,
    })
    .await?;
    poll_for_balance_on_destination(
        request.destination_provider,
        request.destination_token,
        receiver,
    )
    .await;
    Ok(())
}

pub async fn run(axelar_id: Option<String>) -> Result<()> {
    let axelar_id = resolve_axelar_id(axelar_id)?;
    let state = read_state(&axelar_id).await?;
    let start = Instant::now();

    let rpc_url = state.rpc_url.clone();
    let target_json = state.target_json.clone();
    let cfg = ChainsConfig::load(&target_json).await?;

    let private_key = state
        .deployer_private_key
        .clone()
        .ok_or_else(|| eyre::eyre!("no deployerPrivateKey in state"))?;

    let signer: PrivateKeySigner = private_key.parse()?;
    let deployer_address = signer.address();
    let provider = ProviderBuilder::new()
        .wallet(signer)
        .connect_http(rpc_url.parse()?);

    preflight::check_deployer_balance(&rpc_url, deployer_address, &target_json, &axelar_id).await?;

    let src_cfg = cfg
        .chains
        .get(&axelar_id)
        .ok_or_else(|| eyre::eyre!("chain '{axelar_id}' not found in config"))?;
    let its_factory_addr: alloy::primitives::Address = src_cfg
        .contract_address(ChainContract::InterchainTokenFactory, &axelar_id)?
        .parse()?;
    let its_proxy_addr: alloy::primitives::Address = src_cfg
        .contract_address(ChainContract::InterchainTokenService, &axelar_id)?
        .parse()?;

    let dest_cfg = cfg
        .chains
        .get(DEST_CHAIN)
        .ok_or_else(|| eyre::eyre!("destination chain '{DEST_CHAIN}' not found in config"))?;
    let dest_rpc = dest_cfg
        .rpc
        .as_deref()
        .ok_or_else(|| eyre::eyre!("no RPC for destination chain '{DEST_CHAIN}' in target json"))?
        .to_string();
    let dest_its_addr: alloy::primitives::Address = dest_cfg
        .contract_address(ChainContract::InterchainTokenService, DEST_CHAIN)?
        .parse()?;

    ui::section(&format!("ITS Test: {axelar_id} → {DEST_CHAIN}"));
    ui::address("deployer", &format!("{deployer_address}"));
    ui::address("ITS factory", &format!("{its_factory_addr}"));
    ui::address("ITS proxy", &format!("{its_proxy_addr}"));

    // ── Pre-flight: check chain trust ────────────────────────────────────
    let its_service = InterchainTokenService::new(its_proxy_addr, &provider);
    let trusted = its_service
        .isTrustedChain(DEST_CHAIN.to_string())
        .call()
        .await
        .unwrap_or_default();

    if !trusted {
        print_untrusted_chain_remediation(
            &axelar_id,
            its_proxy_addr,
            dest_its_addr,
            &rpc_url,
            &dest_rpc,
            DEST_CHAIN,
            &provider,
        )
        .await?;
        return Ok(());
    }
    ui::success(&format!("\"{DEST_CHAIN}\" is trusted on {axelar_id} ITS"));

    let Some(deployment) = deploy_legacy_token(
        &provider,
        its_factory_addr,
        its_proxy_addr,
        deployer_address,
    )
    .await?
    else {
        return Ok(());
    };

    let relay = prepare_legacy_relay(&cfg, &state.mnemonic, &axelar_id)?;
    relay_legacy_deployment(&axelar_id, &deployment.message, &relay).await?;
    let dest_provider = ProviderBuilder::new().connect_http(dest_rpc.parse()?);
    let destination_token =
        poll_for_remote_token_deploy(&dest_provider, dest_its_addr, deployment.token_id).await?;
    run_legacy_transfer(LegacyTransferRequest {
        axelar_id: &axelar_id,
        local_token: deployment.local_token,
        source_its: its_proxy_addr,
        destination_token,
        source_provider: &provider,
        destination_provider: &dest_provider,
        relay: &relay,
    })
    .await?;

    // ── Complete ────────────────────────────────────────────────────────
    ui::section("Complete");
    ui::success(&format!(
        "ITS flow complete ({})",
        ui::format_elapsed(start)
    ));

    Ok(())
}

struct ConfigRoute {
    source: String,
    destination: String,
    source_rpc: String,
    destination_rpc: String,
    source_axelar_id: ChainAxelarId,
    destination_axelar_id: ChainAxelarId,
    destination_token_symbol: String,
    destination_its: alloy::primitives::Address,
    destination_gateway: alloy::primitives::Address,
    source_cosm_gateway: String,
    voting_verifier: String,
    destination_cosm_gateway: String,
    destination_multisig_prover: String,
    axelarnet_gateway: String,
    its_hub: String,
}

fn resolve_config_route(
    cfg: &ChainsConfig,
    source: Option<String>,
    destination: Option<String>,
) -> Result<ConfigRoute> {
    let source = source.ok_or_else(|| eyre::eyre!("--source-chain required"))?;
    let destination = destination.ok_or_else(|| eyre::eyre!("--destination-chain required"))?;
    let source_config = cfg
        .chains
        .get(&source)
        .ok_or_else(|| eyre::eyre!("source chain '{source}' not found in config"))?;
    let destination_config = cfg
        .chains
        .get(&destination)
        .ok_or_else(|| eyre::eyre!("destination chain '{destination}' not found in config"))?;
    let source_type: ChainType = source_config
        .chain_type
        .as_deref()
        .ok_or_else(|| eyre::eyre!("source chain '{source}' has no chainType"))?
        .parse()?;
    let destination_type: ChainType = destination_config
        .chain_type
        .as_deref()
        .ok_or_else(|| eyre::eyre!("destination chain '{destination}' has no chainType"))?
        .parse()?;
    if source_type != ChainType::Svm || destination_type != ChainType::Evm {
        return Err(eyre::eyre!(
            "ITS config-mode currently supports svm → evm only (got {source_type} → {destination_type})"
        ));
    }
    let source_axelar_id: ChainAxelarId = source_config.axelar_id_or(&source).into();
    let destination_axelar_id: ChainAxelarId = destination_config.axelar_id_or(&destination).into();
    ui::section(&format!("ITS Test: {source} → {destination}"));
    ui::kv(
        "source",
        &format!("{source} ({source_axelar_id}, {source_type})"),
    );
    ui::kv(
        "destination",
        &format!("{destination} ({destination_axelar_id}, {destination_type})"),
    );
    Ok(ConfigRoute {
        source_rpc: source_config
            .rpc
            .clone()
            .ok_or_else(|| eyre::eyre!("no RPC for source chain '{source}'"))?,
        destination_rpc: destination_config
            .rpc
            .clone()
            .ok_or_else(|| eyre::eyre!("no RPC for destination chain '{destination}'"))?,
        destination_token_symbol: destination_config
            .token_symbol
            .clone()
            .ok_or_else(|| eyre::eyre!("no tokenSymbol for destination chain '{destination}'"))?,
        destination_its: destination_config
            .contract_address(ChainContract::InterchainTokenService, &destination)?
            .parse()?,
        destination_gateway: destination_config
            .contract_address(ChainContract::AxelarGateway, &destination)?
            .parse()?,
        source_cosm_gateway: cfg
            .axelar
            .contract_address(AxelarChainContract::Gateway, source_axelar_id.as_str())?
            .to_string(),
        voting_verifier: cfg
            .axelar
            .contract_address(
                AxelarChainContract::VotingVerifier,
                source_axelar_id.as_str(),
            )?
            .to_string(),
        destination_cosm_gateway: cfg
            .axelar
            .contract_address(AxelarChainContract::Gateway, destination_axelar_id.as_str())?
            .to_string(),
        destination_multisig_prover: cfg
            .axelar
            .contract_address(
                AxelarChainContract::MultisigProver,
                destination_axelar_id.as_str(),
            )?
            .to_string(),
        axelarnet_gateway: cfg
            .axelar
            .global_contract_address(AxelarGlobalContract::AxelarnetGateway)?
            .to_string(),
        its_hub: cfg
            .axelar
            .global_contract_address(AxelarGlobalContract::InterchainTokenService)?
            .to_string(),
        source,
        destination,
        source_axelar_id,
        destination_axelar_id,
    })
}

fn print_config_route_contracts(route: &ConfigRoute) {
    ui::address("dest ITS proxy", &format!("{}", route.destination_its));
    ui::address(
        "dest EVM gateway",
        &format!("{}", route.destination_gateway),
    );
    ui::address("source cosm gateway", &route.source_cosm_gateway);
    ui::address("dest cosm gateway", &route.destination_cosm_gateway);
    ui::address("multisig prover (dst)", &route.destination_multisig_prover);
    ui::address("AxelarnetGateway", &route.axelarnet_gateway);
    ui::address("ITS hub (cosm)", &route.its_hub);
}

struct ConfigSigners {
    axelar_signing_key: cosmrs::crypto::secp256k1::SigningKey,
    axelar_address: String,
    lcd: String,
    chain_id: String,
    fee_denom: String,
    gas_price: f64,
    axelar_rpc: String,
    solana: solana_sdk::signature::Keypair,
    solana_address: solana_sdk::pubkey::Pubkey,
    evm: PrivateKeySigner,
    evm_address: alloy::primitives::Address,
}

async fn prepare_config_signers(
    cfg: &ChainsConfig,
    route: &ConfigRoute,
    mnemonic_override: Option<String>,
    evm_private_key_override: Option<String>,
) -> Result<ConfigSigners> {
    let mnemonic = mnemonic_override
        .or_else(|| std::env::var("MNEMONIC").ok())
        .ok_or_else(|| eyre::eyre!("MNEMONIC env var or --mnemonic required for relay"))?;
    let (axelar_signing_key, axelar_address) = derive_axelar_wallet(&mnemonic)?;
    let (lcd, chain_id, fee_denom, gas_price) = cfg.axelar.cosmos_tx_params()?;
    let axelar_rpc = cfg
        .axelar
        .rpc
        .clone()
        .ok_or_else(|| eyre::eyre!("no axelar.rpc in target json"))?;
    ui::section("Preflight");
    ui::address("axelar address", &axelar_address);
    crate::cosmos::check_axelar_balance(&lcd, &chain_id, &axelar_address, &fee_denom, 200_000)
        .await?;
    let solana = crate::solana::load_keypair(None).await?;
    let solana_address = solana.pubkey();
    let source_rpc = route.source_rpc.clone();
    spawn_blocking(move || {
        crate::solana::check_solana_balance(
            &source_rpc,
            "source",
            &solana_address,
            crate::solana::MIN_SOL_ITS_LAMPORTS,
        )
    })
    .await??;
    let evm_key = evm_private_key_override
        .or_else(|| std::env::var("EVM_PRIVATE_KEY").ok())
        .ok_or_else(|| {
            eyre::eyre!(
                "EVM_PRIVATE_KEY env var or --evm-private-key required (used to sign destination EVM txs)"
            )
        })?;
    let evm: PrivateKeySigner = evm_key.parse()?;
    let evm_address = evm.address();
    ui::address("evm signer / receiver", &format!("{evm_address}"));
    preflight::check_evm_balances(
        &route.destination_rpc,
        &[("dest evm signer", evm_address)],
        &route.destination_token_symbol,
    )
    .await?;
    Ok(ConfigSigners {
        axelar_signing_key,
        axelar_address,
        lcd,
        chain_id,
        fee_denom,
        gas_price,
        axelar_rpc,
        solana,
        solana_address,
        evm,
        evm_address,
    })
}

struct ConfigPhaseBRequest<'a, P> {
    network: crate::types::Network,
    route: &'a ConfigRoute,
    signers: &'a ConfigSigners,
    destination_provider: &'a P,
    token_id: [u8; 32],
    destination_token: alloy::primitives::Address,
    amount: u64,
    gas_value: u64,
}

struct ConfigPhaseARequest<'a, P> {
    network: crate::types::Network,
    route: &'a ConfigRoute,
    signers: &'a ConfigSigners,
    destination_provider: &'a P,
    fresh_token: bool,
    gas_value: u64,
    start: Instant,
}

async fn run_config_phase_a<P: alloy::providers::Provider>(
    request: ConfigPhaseARequest<'_, P>,
) -> Result<Option<([u8; 32], alloy::primitives::Address)>> {
    let its =
        InterchainTokenService::new(request.route.destination_its, request.destination_provider);
    if !check_destination_trusts_source(
        &its,
        &request.route.source_axelar_id,
        request.route.destination_its,
        &request.route.destination,
        &request.route.destination_rpc,
    )
    .await?
    {
        return Ok(None);
    }
    ui::success(&format!(
        "destination ITS trusts '{}'",
        request.route.source_axelar_id
    ));
    let hub_address = resolve_hub_address_evm_view(&its, &request.route.its_hub).await;
    ui::kv("hub address (EVM view)", &hub_address);

    let cache_file = cache_path(
        request.network,
        &request.route.source,
        &request.route.destination,
        &request.signers.solana_address.to_string(),
    );
    let cached = try_load_cached_phase_a(
        &cache_file,
        request.fresh_token,
        &request.signers.solana_address,
        request.destination_provider,
    )
    .await;
    if let Some((name, token_id, address)) = cached {
        ui::section("Phase A: skipped (cached deploy still valid)");
        ui::kv("cache file", &cache_file.display().to_string());
        ui::kv("tokenId", &format!("0x{}", alloy::hex::encode(token_id)));
        ui::address("dest token address", &format!("{address}"));
        ui::success(&format!("dest token responds to name() → \"{name}\""));
        return Ok(Some((token_id, address)));
    }
    let tx = CosmosTxContext {
        signing_key: &request.signers.axelar_signing_key,
        axelar_address: &request.signers.axelar_address,
        lcd: &request.signers.lcd,
        chain_id: &request.signers.chain_id,
        fee_denom: &request.signers.fee_denom,
        gas_price: request.signers.gas_price,
    };
    let deployed = run_phase_a_deploy(PhaseADeployRequest {
        network: request.network,
        src_axelar_id: &request.route.source_axelar_id,
        dst_axelar_id: &request.route.destination_axelar_id,
        src_rpc: &request.route.source_rpc,
        sol_keypair: &request.signers.solana,
        sol_pubkey: request.signers.solana_address,
        tx,
        src_cosm_gateway: &request.route.source_cosm_gateway,
        voting_verifier: &request.route.voting_verifier,
        axelarnet_gateway: &request.route.axelarnet_gateway,
        dst_cosm_gateway: &request.route.destination_cosm_gateway,
        dst_multisig_prover: &request.route.destination_multisig_prover,
        axelar_rpc: &request.signers.axelar_rpc,
        its_hub_address: &request.route.its_hub,
        dst_its_proxy: request.route.destination_its,
        dst_evm_gateway: request.route.destination_gateway,
        dst_provider: request.destination_provider,
        its: &its,
        gas_value: request.gas_value,
        cache_file: &cache_file,
        phase_start: request.start,
    })
    .await?;
    Ok(Some(deployed))
}

struct ConfigPhaseBTransfer {
    first_leg_message_id: String,
    sender: String,
    payload: Vec<u8>,
    payload_hash: FixedBytes<32>,
    destination_payload: Vec<u8>,
    pre_balance: U256,
}

fn source_token_accounts(
    network: Network,
    owner: &Pubkey,
    token_id: &[u8; 32],
) -> (Pubkey, Pubkey) {
    let token_program = Pubkey::from_str_const("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
    let associated_token_program =
        Pubkey::from_str_const("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
    let (its_root, _) = crate::solana::find_its_root_pda(network);
    let (mint, _) = crate::solana::find_interchain_token_pda(network, &its_root, token_id);
    let source_account = Pubkey::find_program_address(
        &[owner.as_ref(), token_program.as_ref(), mint.as_ref()],
        &associated_token_program,
    )
    .0;
    (mint, source_account)
}

async fn send_config_phase_b<P: alloy::providers::Provider>(
    request: &ConfigPhaseBRequest<'_, P>,
) -> Result<ConfigPhaseBTransfer> {
    let (mint, source_account) = source_token_accounts(
        request.network,
        &request.signers.solana_address,
        &request.token_id,
    );
    ui::section("Phase B: interchain transfer (manual relay)");
    ui::address("mint", &mint.to_string());
    ui::address("source ATA", &source_account.to_string());
    ui::kv("amount (base units)", &format!("{}", request.amount));
    ui::address(
        "receiver (EVM)",
        &format!("{}", request.signers.evm_address),
    );
    let token = ERC20::new(request.destination_token, request.destination_provider);
    let pre_balance = token
        .balanceOf(request.signers.evm_address)
        .call()
        .await
        .unwrap_or(U256::ZERO);
    ui::kv("pre-transfer balance", &format!("{pre_balance}"));

    ui::step_header(1, PHASE_B_STEPS, "Send InterchainTransfer (Solana → hub)");
    let receiver = request.signers.evm_address.as_slice().to_vec();
    let keypair = crate::solana::clone_keypair(&request.signers.solana);
    let submission_request = ConfigPhaseBSubmissionRequest {
        source_rpc: request.route.source_rpc.clone(),
        keypair,
        network: request.network,
        token_id: request.token_id,
        source_account,
        mint,
        destination_chain: request.route.destination_axelar_id.to_string(),
        destination_address: receiver.clone(),
        amount: request.amount,
        gas_value: request.gas_value,
    };
    let submission = spawn_blocking(move || submit_config_phase_b(submission_request)).await??;

    ui::tx_hash("solana tx", &submission.signature);
    ui::kv("first-leg message_id", &submission.message_id);
    ui::kv("gateway sender", &submission.sender);
    ui::kv("gateway destination_chain", &submission.destination_chain);
    ui::kv(
        "gateway destination_address",
        &submission.destination_address,
    );
    ui::kv(
        "gateway payload_hash",
        &format!("0x{}", alloy::hex::encode(submission.payload_hash)),
    );
    let inner = encode_inner_transfer(
        &request.token_id,
        request.signers.solana_address.to_bytes().as_slice(),
        &receiver,
        request.amount,
        &[],
    );
    Ok(ConfigPhaseBTransfer {
        first_leg_message_id: submission.message_id,
        sender: submission.sender,
        payload: submission.payload,
        payload_hash: submission.payload_hash.into(),
        destination_payload: encode_receive_from_hub(&request.route.source_axelar_id, &inner),
        pre_balance,
    })
}

struct ConfigPhaseBSubmissionRequest {
    source_rpc: String,
    keypair: Keypair,
    network: Network,
    token_id: [u8; 32],
    source_account: Pubkey,
    mint: Pubkey,
    destination_chain: String,
    destination_address: Vec<u8>,
    amount: u64,
    gas_value: u64,
}

struct ConfigPhaseBSubmission {
    signature: String,
    message_id: String,
    sender: String,
    destination_chain: String,
    destination_address: String,
    payload: Vec<u8>,
    payload_hash: [u8; 32],
}

fn submit_config_phase_b(request: ConfigPhaseBSubmissionRequest) -> Result<ConfigPhaseBSubmission> {
    let ConfigPhaseBSubmissionRequest {
        source_rpc,
        keypair,
        network,
        token_id,
        source_account,
        mint,
        destination_chain,
        destination_address,
        amount,
        gas_value,
    } = request;

    let (signature, _) =
        crate::solana::send_its_interchain_transfer(crate::solana::InterchainTransferRequest {
            rpc_url: &source_rpc,
            keypair: &keypair,
            network,
            token_id: &token_id,
            source_account: &source_account,
            mint: &mint,
            destination_chain: &destination_chain,
            destination_address: &destination_address,
            amount,
            gas_value,
        })?;
    let message_id = crate::solana::extract_its_message_id(&source_rpc, network, &signature)?;
    let gateway = crate::solana::extract_gateway_call_contract_payload(&source_rpc, &signature)?;

    Ok(ConfigPhaseBSubmission {
        signature,
        message_id,
        sender: gateway.sender,
        destination_chain: gateway.destination_chain,
        destination_address: gateway.destination_address,
        payload: gateway.payload,
        payload_hash: gateway.payload_hash,
    })
}

async fn relay_config_phase_b<P: alloy::providers::Provider>(
    request: &ConfigPhaseBRequest<'_, P>,
    transfer: &ConfigPhaseBTransfer,
) -> Result<()> {
    let tx = CosmosTxContext {
        signing_key: &request.signers.axelar_signing_key,
        axelar_address: &request.signers.axelar_address,
        lcd: &request.signers.lcd,
        chain_id: &request.signers.chain_id,
        fee_denom: &request.signers.fee_denom,
        gas_price: request.signers.gas_price,
    };
    ui::step_header(
        2,
        PHASE_B_STEPS,
        "Source → hub (verify, route, hub-execute)",
    );
    relay_to_hub(HubRelayRequest {
        axelar_id: request.route.source_axelar_id.as_str(),
        message_id: &transfer.first_leg_message_id,
        source_address: &transfer.sender,
        destination_chain: crate::types::HubChain::NAME,
        destination_address: &request.route.its_hub,
        payload_hash: &transfer.payload_hash,
        payload: &transfer.payload,
        tx,
        cosm_gateway: &request.route.source_cosm_gateway,
        voting_verifier: &request.route.voting_verifier,
        axelarnet_gateway: &request.route.axelarnet_gateway,
    })
    .await?;
    relay_to_destination(DestinationRelayRequest {
        first_leg_message_id: &transfer.first_leg_message_id,
        src_axelar_id: &request.route.source_axelar_id,
        dest_payload: &transfer.destination_payload,
        dst_its_proxy: request.route.destination_its,
        dst_evm_gateway: request.route.destination_gateway,
        dst_provider: request.destination_provider,
        tx,
        dst_cosm_gateway: &request.route.destination_cosm_gateway,
        dst_multisig_prover: &request.route.destination_multisig_prover,
        axelarnet_gateway: &request.route.axelarnet_gateway,
        axelar_rpc: &request.signers.axelar_rpc,
        step_base: 3,
        step_total: PHASE_B_STEPS,
    })
    .await
}

async fn run_config_phase_b<P: alloy::providers::Provider>(
    request: ConfigPhaseBRequest<'_, P>,
) -> Result<()> {
    let start = Instant::now();
    let transfer = send_config_phase_b(&request).await?;
    relay_config_phase_b(&request, &transfer).await?;
    ui::step_header(
        PHASE_B_STEPS,
        PHASE_B_STEPS,
        "Verify ERC20 balance on destination",
    );
    let post_balance = ERC20::new(request.destination_token, request.destination_provider)
        .balanceOf(request.signers.evm_address)
        .call()
        .await?;
    let delta = post_balance.saturating_sub(transfer.pre_balance);
    ui::kv("post-transfer balance", &format!("{post_balance}"));
    ui::kv("delta", &format!("{delta}"));
    if delta != U256::from(request.amount) {
        return Err(eyre::eyre!(
            "balance delta {delta} does not match expected {} (post={post_balance}, pre={})",
            request.amount,
            transfer.pre_balance
        ));
    }
    ui::success(&format!(
        "receiver balance increased by exactly {} base units",
        request.amount
    ));
    ui::section("Phase B complete");
    ui::success(&format!(
        "transfer + manual relay finished ({})",
        ui::format_elapsed(start)
    ));
    Ok(())
}

/// Print the cast-send remediation block when DEST_CHAIN isn't trusted on the
/// source-chain ITS (or vice versa). The owner addresses are queried so the
/// user knows which key needs to sign the setTrustedChain calls.
pub async fn run_config(args: ConfigArgs) -> Result<()> {
    let ConfigArgs {
        config,
        network,
        source_chain,
        destination_chain,
        mnemonic_override,
        evm_private_key_override,
        amount,
        gas_value,
        fresh_token,
    } = args;
    let start = Instant::now();
    let gas_value = gas_value.unwrap_or(DEFAULT_ITS_GAS_VALUE_LAMPORTS);

    let cfg = ChainsConfig::load(&config).await?;
    let route = resolve_config_route(&cfg, source_chain, destination_chain)?;
    let signers =
        prepare_config_signers(&cfg, &route, mnemonic_override, evm_private_key_override).await?;
    let dst_provider = ProviderBuilder::new()
        .wallet(signers.evm.clone())
        .connect_http(route.destination_rpc.parse()?);
    print_config_route_contracts(&route);

    let Some((token_id, destination_token)) = run_config_phase_a(ConfigPhaseARequest {
        network,
        route: &route,
        signers: &signers,
        destination_provider: &dst_provider,
        fresh_token,
        gas_value,
        start,
    })
    .await?
    else {
        return Ok(());
    };

    let amount = amount.unwrap_or(DEFAULT_ITS_TRANSFER_AMOUNT_BASE_UNITS);
    run_config_phase_b(ConfigPhaseBRequest {
        network,
        route: &route,
        signers: &signers,
        destination_provider: &dst_provider,
        token_id,
        destination_token,
        amount,
        gas_value,
    })
    .await?;

    ui::section("All phases complete");
    ui::success(&format!("total elapsed: {}", ui::format_elapsed(start)));

    Ok(())
}
