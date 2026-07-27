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
use solana_sdk::signer::Signer as SolSigner;

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
use crate::config::ChainsConfig;
use crate::cosmos::derive_axelar_wallet;
use crate::evm::{ERC20, InterchainToken, InterchainTokenFactory, InterchainTokenService};
use crate::preflight;
use crate::state::read_state;
use crate::types::{ChainAxelarId, ChainType};
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

pub async fn run(axelar_id: Option<String>) -> Result<()> {
    let axelar_id = resolve_axelar_id(axelar_id)?;
    let state = read_state(&axelar_id)?;
    let start = Instant::now();

    let rpc_url = state.rpc_url.clone();
    let target_json = state.target_json.clone();
    let cfg = ChainsConfig::load(&target_json)?;

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
        .contract_address("InterchainTokenFactory", &axelar_id)?
        .parse()?;
    let its_proxy_addr: alloy::primitives::Address = src_cfg
        .contract_address("InterchainTokenService", &axelar_id)?
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
        .contract_address("InterchainTokenService", DEST_CHAIN)?
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

    // ── Step 1: Deploy interchain token locally ─────────────────────────
    ui::step_header(1, TOTAL_STEPS, "Deploy interchain token");

    let salt = crate::commands::event_extractors::generate_salt();
    let spec = crate::types::EVM_LEGACY_SPEC;
    let initial_supply = crate::types::whole_tokens(1000, spec.decimals);

    ui::kv("name", spec.name);
    ui::kv("symbol", spec.symbol);
    ui::kv("decimals", &spec.decimals.to_string());
    ui::kv("initial supply", &format!("{initial_supply}"));
    ui::kv("salt", &format!("{salt}"));

    let factory = InterchainTokenFactory::new(its_factory_addr, &provider);
    let deploy_call = factory
        .deployInterchainToken(
            salt,
            spec.name.to_string(),
            spec.symbol.to_string(),
            spec.decimals,
            initial_supply,
            deployer_address,
        )
        .value(U256::ZERO);

    let pending = deploy_call.send().await?;
    let receipt = crate::evm::broadcast_and_log(pending, "tx").await?;

    // Extract tokenId from InterchainTokenDeployed event logs
    let (token_id, local_token_addr) = extract_token_deployed_event(&receipt)?;
    ui::kv("tokenId", &format!("{token_id}"));
    ui::address("local token", &format!("{local_token_addr}"));

    // Verify tokenId by calling interchainTokenId() on the deployed token
    let on_chain_id = InterchainToken::new(local_token_addr, &provider)
        .interchainTokenId()
        .call()
        .await?;
    if on_chain_id != token_id {
        return Err(eyre::eyre!(
            "tokenId mismatch: event={token_id} on-chain={on_chain_id}"
        ));
    }
    ui::success("tokenId verified on-chain");

    // ── Step 2: Deploy remote interchain token to flow ──────────────────
    ui::step_header(2, TOTAL_STEPS, "Deploy remote interchain token to flow");

    let gas_value = crate::types::eth(2); // cross-chain deploy budget
    ui::kv("destination", DEST_CHAIN);
    ui::kv("gas value", &format!("{gas_value} wei"));

    let remote_call = factory
        .deployRemoteInterchainToken(salt, DEST_CHAIN.to_string(), gas_value)
        .value(gas_value);

    let pending = match remote_call.send().await {
        Ok(p) => p,
        Err(e) => {
            let err_debug = format!("{e:?}");
            if err_debug.contains("f9188a68") || err_debug.contains("UntrustedChain") {
                ui::error("UntrustedChain() — the destination chain is not trusted by this ITS");
                ui::info("Run `test its` again for detailed remediation steps.");
                return Ok(());
            }
            return Err(e.into());
        }
    };
    let tx_hash = *pending.tx_hash();
    let receipt = crate::evm::broadcast_and_log(pending, "tx").await?;

    // Extract ContractCall event to get message details
    let (event_index, payload, payload_hash, destination_chain, destination_address) =
        extract_contract_call_event(&receipt)?;

    let message_id = format!("{tx_hash:#x}-{event_index}");
    let source_address = format!("{its_proxy_addr}");

    ui::kv("message_id", &message_id);
    ui::kv("payload_hash", &format!("{payload_hash}"));
    ui::kv("destination_chain", &destination_chain);
    ui::kv("destination_address", &destination_address);
    ui::kv("source_address", &source_address);

    // ── Amplifier routing: source → hub ─────────────────────────────────
    ui::section("Amplifier Routing (source → hub)");

    let (signing_key, axelar_address) = derive_axelar_wallet(&state.mnemonic)?;
    let (lcd, chain_id, fee_denom, gas_price) = cfg.axelar.cosmos_tx_params()?;
    let tx = CosmosTxContext {
        signing_key: &signing_key,
        axelar_address: &axelar_address,
        lcd: &lcd,
        chain_id: &chain_id,
        fee_denom: &fee_denom,
        gas_price,
    };

    let cosm_gateway = cfg
        .axelar
        .contract_address("Gateway", &axelar_id)?
        .to_string();
    let voting_verifier = cfg
        .axelar
        .contract_address("VotingVerifier", &axelar_id)?
        .to_string();

    ui::address("cosmos gateway", &cosm_gateway);
    ui::address("voting verifier", &voting_verifier);
    ui::address("axelar address", &axelar_address);

    let its_msg = json!({
        "cc_id": {
            "message_id": message_id,
            "source_chain": axelar_id,
        },
        "destination_chain": destination_chain,
        "destination_address": destination_address,
        "source_address": source_address,
        "payload_hash": alloy::hex::encode(payload_hash.as_slice()),
    });

    // ── Step 3: verify_messages ─────────────────────────────────────────
    ui::step_header(3, TOTAL_STEPS, "verify_messages");
    let poll_id = submit_verify_messages_amplifier(&its_msg, tx, &cosm_gateway).await?;

    if let Some(poll_id) = poll_id {
        ui::kv("poll_id", &poll_id);

        // ── Step 4: Wait for poll votes + end poll ──────────────────────────
        ui::step_header(4, TOTAL_STEPS, "Wait for poll votes + end poll");
        wait_for_poll_votes(&lcd, &voting_verifier, &poll_id).await?;
        end_poll_with_retry(&poll_id, tx, &voting_verifier).await?;
    } else {
        ui::info("no new poll created — message already being verified by active verifiers");
        ui::step_header(4, TOTAL_STEPS, "Wait for poll votes + end poll");
        ui::info("skipped (existing poll)");
    }

    // ── Step 5: route_messages ──────────────────────────────────────────
    ui::step_header(5, TOTAL_STEPS, "route_messages");
    route_messages_with_retry(&its_msg, tx, &cosm_gateway).await?;

    // ── Step 6: Execute on AxelarnetGateway (hub) ───────────────────────
    ui::step_header(6, TOTAL_STEPS, "Execute on AxelarnetGateway (hub)");

    let axelarnet_gateway = cfg
        .axelar
        .global_contract_address("AxelarnetGateway")?
        .to_string();
    ui::address("AxelarnetGateway", &axelarnet_gateway);

    execute_on_axelarnet_gateway(
        &message_id,
        &axelar_id,
        DEST_CHAIN,
        &payload,
        tx,
        &axelarnet_gateway,
    )
    .await?;

    // ── Step 7: Poll destination chain to confirm token deployed ─────────
    let dest_provider = ProviderBuilder::new().connect_http(dest_rpc.parse()?);
    let predicted_addr =
        poll_for_remote_token_deploy(&dest_provider, dest_its_addr, token_id).await?;

    // ── Step 8: Send interchain transfer ────────────────────────────────
    ui::step_header(8, TOTAL_STEPS, "Send interchain transfer");

    let transfer_amount = crate::types::whole_tokens(100, crate::types::EVM_LEGACY_SPEC.decimals);
    let receiver = crate::types::DEAD_ADDRESS;
    let receiver_bytes = Bytes::copy_from_slice(receiver.as_slice());
    let transfer_gas = crate::types::eth_milli(200); // cross-chain transfer budget

    ui::kv("amount", &format!("{transfer_amount}"));
    ui::address("receiver", &format!("{receiver}"));
    ui::kv("gas value", &format!("{transfer_gas} wei"));

    let local_token = InterchainToken::new(local_token_addr, &provider);
    let transfer_call = local_token
        .interchainTransfer(
            DEST_CHAIN.to_string(),
            receiver_bytes,
            transfer_amount,
            Bytes::new(), // empty metadata
        )
        .value(transfer_gas);

    let pending = transfer_call.send().await?;
    let tx_hash = *pending.tx_hash();
    let receipt = crate::evm::broadcast_and_log(pending, "tx").await?;

    // Extract ContractCall event for the transfer
    let (xfer_event_index, xfer_payload, xfer_payload_hash, xfer_dest_chain, xfer_dest_addr) =
        extract_contract_call_event(&receipt)?;

    let xfer_message_id = format!("{tx_hash:#x}-{xfer_event_index}");
    ui::kv("message_id", &xfer_message_id);
    ui::kv("destination_chain", &xfer_dest_chain);

    // ── Step 9: Relay transfer to hub ────────────────────────────────────
    ui::step_header(9, TOTAL_STEPS, "Relay transfer to hub");

    relay_to_hub(HubRelayRequest {
        axelar_id: &axelar_id,
        message_id: &xfer_message_id,
        source_address: &source_address,
        destination_chain: &xfer_dest_chain,
        destination_address: &xfer_dest_addr,
        payload_hash: &xfer_payload_hash,
        payload: &xfer_payload,
        tx,
        cosm_gateway: &cosm_gateway,
        voting_verifier: &voting_verifier,
        axelarnet_gateway: &axelarnet_gateway,
    })
    .await?;

    // ── Step 10: Verify transfer on destination ──────────────────────────
    poll_for_balance_on_destination(&dest_provider, predicted_addr, receiver).await;

    // ── Complete ────────────────────────────────────────────────────────
    ui::section("Complete");
    ui::success(&format!(
        "ITS flow complete ({})",
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

    let cfg = ChainsConfig::load(&config)?;

    let src = source_chain.ok_or_else(|| eyre::eyre!("--source-chain required"))?;
    let dst = destination_chain.ok_or_else(|| eyre::eyre!("--destination-chain required"))?;

    let src_cfg = cfg
        .chains
        .get(&src)
        .ok_or_else(|| eyre::eyre!("source chain '{src}' not found in config"))?;
    let dst_cfg = cfg
        .chains
        .get(&dst)
        .ok_or_else(|| eyre::eyre!("destination chain '{dst}' not found in config"))?;

    let src_type: ChainType = src_cfg
        .chain_type
        .as_deref()
        .ok_or_else(|| eyre::eyre!("source chain '{src}' has no chainType"))?
        .parse()?;
    let dst_type: ChainType = dst_cfg
        .chain_type
        .as_deref()
        .ok_or_else(|| eyre::eyre!("destination chain '{dst}' has no chainType"))?
        .parse()?;

    if src_type != ChainType::Svm || dst_type != ChainType::Evm {
        return Err(eyre::eyre!(
            "ITS config-mode currently supports svm → evm only (got {src_type} → {dst_type})"
        ));
    }

    let src_rpc = src_cfg
        .rpc
        .as_deref()
        .ok_or_else(|| eyre::eyre!("no RPC for source chain '{src}'"))?
        .to_string();
    let dst_rpc = dst_cfg
        .rpc
        .as_deref()
        .ok_or_else(|| eyre::eyre!("no RPC for destination chain '{dst}'"))?
        .to_string();

    // Cosmos-side identifiers for the source/destination chains. Consensus
    // chains use a capitalised axelarId distinct from the JSON key — keep
    // them as separate types so the compiler refuses to confuse them.
    let src_axelar_id: ChainAxelarId = src_cfg.axelar_id_or(&src).into();
    let dst_axelar_id: ChainAxelarId = dst_cfg.axelar_id_or(&dst).into();

    ui::section(&format!("ITS Test: {src} → {dst}"));
    ui::kv("source", &format!("{src} ({src_axelar_id}, {src_type})"));
    ui::kv(
        "destination",
        &format!("{dst} ({dst_axelar_id}, {dst_type})"),
    );

    // --- Preflight: derive Axelar wallet, fund checks ---
    let mnemonic = mnemonic_override
        .or_else(|| std::env::var("MNEMONIC").ok())
        .ok_or_else(|| eyre::eyre!("MNEMONIC env var or --mnemonic required for relay"))?;
    let (signing_key, axelar_address) = derive_axelar_wallet(&mnemonic)?;
    let (lcd, chain_id, fee_denom, gas_price) = cfg.axelar.cosmos_tx_params()?;
    let axelar_rpc = cfg
        .axelar
        .rpc
        .as_deref()
        .ok_or_else(|| eyre::eyre!("no axelar.rpc in target json"))?
        .to_string();

    ui::section("Preflight");
    ui::address("axelar address", &axelar_address);
    // ITS does ~7-8 cosmos txs per phase across 2 phases; bump min from GMP's 100k.
    const MIN_RELAY_BALANCE_UAXL: u128 = 200_000;
    crate::cosmos::check_axelar_balance(
        &lcd,
        &chain_id,
        &axelar_address,
        &fee_denom,
        MIN_RELAY_BALANCE_UAXL,
    )
    .await?;

    let sol_keypair = crate::solana::load_keypair(None)?;
    let sol_pubkey = sol_keypair.pubkey();
    crate::solana::check_solana_balance(
        &src_rpc,
        "source",
        &sol_pubkey,
        crate::solana::MIN_SOL_ITS_LAMPORTS,
    )?;

    // EVM signer is only used to derive the receiver address (we never send EVM
    // txs from this key — that's done with deployer / cosm-mnemonic-derived flow).
    let evm_pk = evm_private_key_override
        .or_else(|| std::env::var("EVM_PRIVATE_KEY").ok())
        .ok_or_else(|| {
            eyre::eyre!("EVM_PRIVATE_KEY env var or --evm-private-key required (used to sign destination EVM txs)")
        })?;
    let evm_signer: PrivateKeySigner = evm_pk.parse()?;
    let evm_signer_address = evm_signer.address();
    ui::address("evm signer / receiver", &format!("{evm_signer_address}"));

    let token_symbol = dst_cfg
        .token_symbol
        .as_deref()
        .ok_or_else(|| eyre::eyre!("no tokenSymbol for destination chain '{dst}'"))?;
    preflight::check_evm_balances(
        &dst_rpc,
        &[("dest evm signer", evm_signer_address)],
        token_symbol,
    )
    .await?;

    let dst_provider = ProviderBuilder::new()
        .wallet(evm_signer.clone())
        .connect_http(dst_rpc.parse()?);

    // --- Resolve contract addresses ---
    let dst_its_proxy: alloy::primitives::Address = dst_cfg
        .contract_address("InterchainTokenService", &dst)?
        .parse()?;
    let dst_evm_gateway: alloy::primitives::Address =
        dst_cfg.contract_address("AxelarGateway", &dst)?.parse()?;
    let src_cosm_gateway = cfg
        .axelar
        .contract_address("Gateway", src_axelar_id.as_str())?
        .to_string();
    let voting_verifier = cfg
        .axelar
        .contract_address("VotingVerifier", src_axelar_id.as_str())?
        .to_string();
    let dst_cosm_gateway = cfg
        .axelar
        .contract_address("Gateway", dst_axelar_id.as_str())?
        .to_string();
    let dst_multisig_prover = cfg
        .axelar
        .contract_address("MultisigProver", dst_axelar_id.as_str())?
        .to_string();
    let axelarnet_gateway = cfg
        .axelar
        .global_contract_address("AxelarnetGateway")?
        .to_string();
    let its_hub_address = cfg
        .axelar
        .global_contract_address("InterchainTokenService")?
        .to_string();

    ui::address("dest ITS proxy", &format!("{dst_its_proxy}"));
    ui::address("dest EVM gateway", &format!("{dst_evm_gateway}"));
    ui::address("source cosm gateway", &src_cosm_gateway);
    ui::address("dest cosm gateway", &dst_cosm_gateway);
    ui::address("multisig prover (dst)", &dst_multisig_prover);
    ui::address("AxelarnetGateway", &axelarnet_gateway);
    ui::address("ITS hub (cosm)", &its_hub_address);

    // --- Trust-chain check: dest ITS must trust the source chain ---
    let its = InterchainTokenService::new(dst_its_proxy, &dst_provider);
    if !check_destination_trusts_source(&its, &src_axelar_id, dst_its_proxy, &dst, &dst_rpc).await?
    {
        return Ok(());
    }
    ui::success(&format!("destination ITS trusts '{src_axelar_id}'"));

    let hub_address_evm_view = resolve_hub_address_evm_view(&its, &its_hub_address).await;
    ui::kv("hub address (EVM view)", &hub_address_evm_view);

    // ─────────────────────────────────────────────────────────────────────
    // Phase A: deploy interchain token (local + remote with manual relay)
    //
    // Idempotent: if a previous Phase A run for the same
    // (network, src, dst, deployer) is cached on disk and the destination
    // token still responds to `name()`, skip the deploy entirely and reuse
    // the cached tokenId. Pass `--fresh-token` to force a redeploy.
    // ─────────────────────────────────────────────────────────────────────
    let cache_file = cache_path(network, &src, &dst, &sol_pubkey.to_string());
    let cached =
        try_load_cached_phase_a(&cache_file, fresh_token, &sol_pubkey, &dst_provider).await;

    let (token_id, dest_token_addr) = if let Some((name, tid, addr)) = cached {
        ui::section("Phase A: skipped (cached deploy still valid)");
        ui::kv("cache file", &cache_file.display().to_string());
        ui::kv("tokenId", &format!("0x{}", alloy::hex::encode(tid)));
        ui::address("dest token address", &format!("{addr}"));
        ui::success(&format!("dest token responds to name() → \"{name}\""));
        (tid, addr)
    } else {
        run_phase_a_deploy(PhaseADeployRequest {
            network,
            src_axelar_id: &src_axelar_id,
            dst_axelar_id: &dst_axelar_id,
            src_rpc: &src_rpc,
            sol_keypair: &sol_keypair,
            sol_pubkey,
            tx: CosmosTxContext {
                signing_key: &signing_key,
                axelar_address: &axelar_address,
                lcd: &lcd,
                chain_id: &chain_id,
                fee_denom: &fee_denom,
                gas_price,
            },
            src_cosm_gateway: &src_cosm_gateway,
            voting_verifier: &voting_verifier,
            axelarnet_gateway: &axelarnet_gateway,
            dst_cosm_gateway: &dst_cosm_gateway,
            dst_multisig_prover: &dst_multisig_prover,
            axelar_rpc: &axelar_rpc,
            its_hub_address: &its_hub_address,
            dst_its_proxy,
            dst_evm_gateway,
            dst_provider: &dst_provider,
            its: &its,
            gas_value,
            cache_file: &cache_file,
            phase_start: start,
        })
        .await?
    };

    // ─────────────────────────────────────────────────────────────────────
    // Phase B: interchain transfer (manual relay)
    // ─────────────────────────────────────────────────────────────────────
    let phase_b_start = Instant::now();
    let amount = amount.unwrap_or(DEFAULT_ITS_TRANSFER_AMOUNT_BASE_UNITS);
    let receiver_bytes = evm_signer_address.as_slice().to_vec();
    let token_program_2022 =
        solana_sdk::pubkey::Pubkey::from_str_const("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
    let ata_program =
        solana_sdk::pubkey::Pubkey::from_str_const("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
    let (its_root_pda, _) = crate::solana::find_its_root_pda(network);
    let (mint, _) = crate::solana::find_interchain_token_pda(network, &its_root_pda, &token_id);
    let source_ata = solana_sdk::pubkey::Pubkey::find_program_address(
        &[
            sol_pubkey.as_ref(),
            token_program_2022.as_ref(),
            mint.as_ref(),
        ],
        &ata_program,
    )
    .0;

    ui::section("Phase B: interchain transfer (manual relay)");
    ui::address("mint", &mint.to_string());
    ui::address("source ATA", &source_ata.to_string());
    ui::kv("amount (base units)", &format!("{amount}"));
    ui::address("receiver (EVM)", &format!("{evm_signer_address}"));

    // Capture the destination ERC20 balance BEFORE the transfer so we can
    // verify a strict delta later.
    let erc20 = ERC20::new(dest_token_addr, &dst_provider);
    let pre_balance = erc20
        .balanceOf(evm_signer_address)
        .call()
        .await
        .unwrap_or(U256::ZERO);
    ui::kv("pre-transfer balance", &format!("{pre_balance}"));

    // Step B1: Solana — fire the InterchainTransfer
    ui::step_header(1, PHASE_B_STEPS, "Send InterchainTransfer (Solana → hub)");
    let (xfer_sig, _metrics) =
        crate::solana::send_its_interchain_transfer(crate::solana::InterchainTransferRequest {
            rpc_url: &src_rpc,
            keypair: &sol_keypair,
            network,
            token_id: &token_id,
            source_account: &source_ata,
            mint: &mint,
            destination_chain: dst_axelar_id.as_str(),
            destination_address: &receiver_bytes,
            amount,
            gas_value,
        })?;
    ui::tx_hash("solana tx", &xfer_sig);

    let xfer_first_leg_id = crate::solana::extract_its_message_id(&src_rpc, network, &xfer_sig)?;
    ui::kv("first-leg message_id", &xfer_first_leg_id);

    let xfer_gw = crate::solana::extract_gateway_call_contract_payload(&src_rpc, &xfer_sig)?;
    ui::kv("gateway sender", &xfer_gw.sender);
    ui::kv("gateway destination_chain", &xfer_gw.destination_chain);
    ui::kv("gateway destination_address", &xfer_gw.destination_address);
    ui::kv(
        "gateway payload_hash",
        &format!("0x{}", alloy::hex::encode(xfer_gw.payload_hash)),
    );

    // Step B2: drive source → hub
    ui::step_header(
        2,
        PHASE_B_STEPS,
        "Source → hub (verify, route, hub-execute)",
    );
    let xfer_payload_hash = FixedBytes::<32>::from(xfer_gw.payload_hash);
    let tx = CosmosTxContext {
        signing_key: &signing_key,
        axelar_address: &axelar_address,
        lcd: &lcd,
        chain_id: &chain_id,
        fee_denom: &fee_denom,
        gas_price,
    };
    relay_to_hub(HubRelayRequest {
        axelar_id: src_axelar_id.as_str(),
        message_id: &xfer_first_leg_id,
        source_address: &xfer_gw.sender,
        destination_chain: crate::types::HubChain::NAME,
        destination_address: &its_hub_address,
        payload_hash: &xfer_payload_hash,
        payload: &xfer_gw.payload,
        tx,
        cosm_gateway: &src_cosm_gateway,
        voting_verifier: &voting_verifier,
        axelarnet_gateway: &axelarnet_gateway,
    })
    .await?;

    // Step B3+: hub → destination (reconstruct RECEIVE_FROM_HUB envelope and drive proof + execute)
    let xfer_inner = encode_inner_transfer(
        &token_id,
        sol_pubkey.to_bytes().as_slice(),
        &receiver_bytes,
        amount,
        &[],
    );
    let xfer_dest_payload = encode_receive_from_hub(&src_axelar_id, &xfer_inner);

    relay_to_destination(DestinationRelayRequest {
        first_leg_message_id: &xfer_first_leg_id,
        src_axelar_id: &src_axelar_id,
        dest_payload: &xfer_dest_payload,
        dst_its_proxy,
        dst_evm_gateway,
        dst_provider: &dst_provider,
        tx,
        dst_cosm_gateway: &dst_cosm_gateway,
        dst_multisig_prover: &dst_multisig_prover,
        axelarnet_gateway: &axelarnet_gateway,
        axelar_rpc: &axelar_rpc,
        step_base: 3,
        step_total: PHASE_B_STEPS,
    })
    .await?;

    // Step B-final: verify ERC20 balance went up by exactly `amount`.
    ui::step_header(
        PHASE_B_STEPS,
        PHASE_B_STEPS,
        "Verify ERC20 balance on destination",
    );
    let post_balance = erc20.balanceOf(evm_signer_address).call().await?;
    let delta = post_balance.saturating_sub(pre_balance);
    ui::kv("post-transfer balance", &format!("{post_balance}"));
    ui::kv("delta", &format!("{delta}"));
    if delta == U256::from(amount) {
        ui::success(&format!(
            "receiver balance increased by exactly {amount} base units"
        ));
    } else {
        return Err(eyre::eyre!(
            "balance delta {delta} does not match expected {amount} (post={post_balance}, pre={pre_balance})"
        ));
    }

    ui::section("Phase B complete");
    ui::success(&format!(
        "transfer + manual relay finished ({})",
        ui::format_elapsed(phase_b_start)
    ));

    ui::section("All phases complete");
    ui::success(&format!("total elapsed: {}", ui::format_elapsed(start)));

    Ok(())
}
