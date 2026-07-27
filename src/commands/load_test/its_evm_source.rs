//! Shared building blocks for ITS load tests whose source chain is EVM.
//!
//! Every `its_evm_to_<dest>.rs` module follows the same source-side recipe:
//!
//! 1. Validate Axelar prerequisites for the destination chain.
//! 2. Parse the EVM private key into a signer + main-key bytes.
//! 3. Resolve the ITS factory + service addresses for the source chain.
//! 4. Pick a gas-value default (`estimateGasFee` quote × 1.5, fallback constant).
//! 5. Deploy or reuse an `InterchainToken` on the source EVM + its remote
//!    counterpart on the destination chain.
//! 6. Derive a pool of ephemeral signers and fund them with native + gas-value.
//! 7. Distribute the ITS token across the derived signers.
//! 8. Run the per-tx `interchainTransfer` loop (burst or sustained).
//!
//! Steps 1-4 and 6-8 are destination-agnostic and live here. Step 5 is mostly
//! destination-agnostic (the factory call is identical; only the cache schema
//! and the wait-for-remote-deploy hook differ) — the helper here covers the
//! identical bits; per-destination modules supply their own wait-for-deploy.
//!
//! Per-destination modules keep:
//!   * Destination-receiver-bytes encoding (varies: 20B EVM, 32B Solana, etc.).
//!   * Pre-flight checks for the destination chain (RPC validation, etc.).
//!   * Wait-for-remote-deploy on the destination chain.
//!   * Final on-chain verification (Solana / EVM / Stellar / etc.).

use std::time::{Duration, Instant};

use alloy::{
    consensus::Transaction as _,
    primitives::{Address, Bytes, FixedBytes, TxHash, U256},
    providers::{Provider, ProviderBuilder},
    rpc::types::TransactionReceipt,
    signers::local::PrivateKeySigner,
};
use eyre::eyre;

use super::keypairs;
use super::metrics::TxMetrics;
use super::{LoadTestArgs, check_evm_balance, save_its_cache};
use crate::commands::test_its::{
    extract_contract_call_event, extract_token_deployed_event, generate_salt,
};
use crate::config::ChainsConfig;
use crate::evm::{ERC20, InterchainTokenFactory, InterchainTokenService};
use crate::ui;

/// How long to wait for an EVM `interchainTransfer` receipt before giving up.
/// Fast chains confirm in ~8s; other chains typically <20s. The binding
/// constraint is Hyperliquid, whose gas-heavy txs (like ITS transfers) only
/// land in **big blocks**, produced on a ~60s cadence (see `crate::hyperliquid`).
/// A 60s budget times out right as the block is produced, recording an
/// already-landed transfer as a source failure — the recurring mainnet
/// `Hyperliquid -> *` false failure. 120s covers ~2 big-block cycles and
/// matches the deploy-tx receipt waits in this file, while still catching
/// silently-dropped txs on other chains.
const EVM_RECEIPT_TIMEOUT: Duration = Duration::from_secs(120);

const TOKEN_NAME: &str = "AXE";
const TOKEN_SYMBOL: &str = "AXE";
const TOKEN_DECIMALS: u8 = 18;

/// Source-chain EVM signer state: the user's signer plus its address and
/// raw private-key bytes (used for deriving ephemeral signers).
pub(super) struct EvmSource {
    pub signer: PrivateKeySigner,
    pub deployer_address: Address,
    pub main_key: [u8; 32],
}

/// ITS-side addresses resolved from config for the source chain.
pub(super) struct ItsContracts {
    pub its_factory_addr: Address,
    pub its_proxy_addr: Address,
}

/// Default gas value for ITS cross-chain transfers.
///
/// Tries the Axelarscan `estimateGasFee` quote for this route (× 1.5);
/// when the API can't be reached, falls back to `fallback_wei` — caller
/// supplies the per-(source-chain, destination-class) constant.
pub(super) async fn default_gas_value_wei(args: &LoadTestArgs, fallback_wei: u128) -> u128 {
    if let Some(quoted) = quote_route_gas(args).await {
        return quoted;
    }
    fallback_wei
}

async fn quote_route_gas(args: &LoadTestArgs) -> Option<u128> {
    let cfg = ChainsConfig::load(&args.config).ok()?;
    let symbol = cfg
        .chains
        .get(&args.source_chain)?
        .token_symbol
        .as_deref()?;
    super::gas_estimate::estimate_route_gas(
        args.network,
        &args.source_axelar_id,
        &args.destination_axelar_id,
        symbol,
        super::gas_estimate::DEFAULT_DEST_GAS_LIMIT,
    )
    .await
}

/// Verify Axelar-side prerequisites: an Amplifier destination needs a Cosmos
/// Gateway (the `routed` phase); a legacy (consensus) destination has none and
/// is verified on its on-chain gateway instead. The global AxelarnetGateway
/// (the ITS Hub) is always required. `dest_axelar_id` is the destination's
/// axelarId (not the config key — they differ for consensus chains).
pub(super) fn verify_axelar_prerequisites(
    cfg: &ChainsConfig,
    dest_axelar_id: &str,
) -> eyre::Result<()> {
    let dest_amplifier = cfg
        .axelar
        .contract_address("VotingVerifier", dest_axelar_id)
        .is_ok();
    if dest_amplifier
        && cfg
            .axelar
            .contract_address("Gateway", dest_axelar_id)
            .is_err()
    {
        eyre::bail!(
            "destination chain '{dest_axelar_id}' has no Cosmos Gateway in the config — verification would fail."
        );
    }
    if cfg
        .axelar
        .global_contract_address("AxelarnetGateway")
        .is_err()
    {
        eyre::bail!("no AxelarnetGateway address in config — required for ITS load test");
    }
    Ok(())
}

/// Parse the EVM private key, log the wallet balance, and return the signer
/// state used by every downstream phase.
pub(super) async fn init_evm_source(
    args: &LoadTestArgs,
    evm_rpc_url: &str,
) -> eyre::Result<EvmSource> {
    let private_key = args.private_key.as_ref().ok_or_else(|| {
        eyre!("EVM private key required. Set EVM_PRIVATE_KEY env var or use --private-key")
    })?;
    let signer: PrivateKeySigner = private_key.parse()?;
    let deployer_address = signer.address();
    let read_provider = ProviderBuilder::new().connect_http(evm_rpc_url.parse()?);
    check_evm_balance(&read_provider, deployer_address).await?;

    let main_key: [u8; 32] = signer.to_bytes().into();
    {
        let balance: u128 = read_provider.get_balance(deployer_address).await?.to();
        let eth = balance as f64 / 1e18;
        ui::kv("wallet", &format!("{deployer_address} ({eth:.6} ETH)"));
    }

    Ok(EvmSource {
        signer,
        deployer_address,
        main_key,
    })
}

/// Resolve the ITS factory + service addresses for the source chain and emit
/// the matching UI lines.
pub(super) fn resolve_its_contracts(cfg: &ChainsConfig, src: &str) -> eyre::Result<ItsContracts> {
    let src_cfg = cfg
        .chains
        .get(src)
        .ok_or_else(|| eyre!("source chain '{src}' not found in config"))?;
    let its_factory_addr: Address = src_cfg
        .contract_address("InterchainTokenFactory", src)?
        .parse()?;
    let its_proxy_addr: Address = src_cfg
        .contract_address("InterchainTokenService", src)?
        .parse()?;

    ui::address("ITS factory", &format!("{its_factory_addr}"));
    ui::address("ITS service", &format!("{its_proxy_addr}"));

    Ok(ItsContracts {
        its_factory_addr,
        its_proxy_addr,
    })
}

/// Derive ephemeral signers and ensure each one is funded for the planned
/// number of transfers (native gas + per-key gas-value buffer).
///
/// `hub_gas_extra_per_key` is the wei amount each derived signer needs *on
/// top of* the baseline native-gas funding — i.e., the `gas_value × 2`
/// (hub round-trip) × the number of rounds the key will fire.
pub(super) async fn derive_and_fund_keys(
    main_signer: &PrivateKeySigner,
    main_key: &[u8; 32],
    evm_rpc_url: &str,
    num_keys: usize,
    hub_gas_extra_per_key: u128,
    source_axelar_id: &str,
) -> eyre::Result<Vec<PrivateKeySigner>> {
    // Hedera quirk: a freshly funded EVM address auto-creates a Hedera
    // account but the mirror node lags before the JSON-RPC relay sees it,
    // so a tx FROM a just-funded derived key reverts with "Sender account
    // not found" during simulation. The deployments-repo scripts avoid
    // this by using a single pre-existing wallet — mirror that here.
    // (Loses parallelism for num_txs > 1 on Hedera; acceptable for the
    // smoke fleet which uses num_txs = 1.)
    if source_axelar_id == "hedera" {
        ui::info("Hedera source: using main wallet directly (no key derivation)");
        return Ok(vec![main_signer.clone(); num_keys]);
    }

    let derived = keypairs::derive_evm_signers(main_key, num_keys)?;
    ui::info(&format!("derived {} EVM signing keys", derived.len()));

    let funding_provider = ProviderBuilder::new()
        .wallet(main_signer.clone())
        .connect_http(evm_rpc_url.parse()?);
    keypairs::ensure_funded_evm_with_extra(
        &funding_provider,
        main_signer,
        &derived,
        hub_gas_extra_per_key,
    )
    .await?;

    Ok(derived)
}

/// Deploy a new interchain token and its remote counterpart.
/// Returns `(tokenId, localTokenAddress, deployMessageId)`.
///
/// The cache schema is shared across destinations — `tokenId`, `tokenAddress`,
/// `salt` — so a token deployed for, say, an EVM→EVM run is reusable on an
/// EVM→Solana run with the same source/destination pair if the destination
/// chain string matches.
pub(super) async fn deploy_its_token<P: Provider>(
    provider: &P,
    factory_addr: Address,
    deployer: Address,
    dest_chain: &str,
    total_supply: U256,
    source_chain: &str,
    gas_value: U256,
) -> eyre::Result<(FixedBytes<32>, Address, Option<String>)> {
    let salt = generate_salt();

    ui::info("deploying new ITS token...");
    ui::kv("name", TOKEN_NAME);
    ui::kv("symbol", TOKEN_SYMBOL);
    ui::kv("decimals", &TOKEN_DECIMALS.to_string());
    ui::kv("supply", &format!("{total_supply}"));

    let factory = InterchainTokenFactory::new(factory_addr, provider);

    let deploy_call = factory
        .deployInterchainToken(
            salt,
            TOKEN_NAME.to_string(),
            TOKEN_SYMBOL.to_string(),
            TOKEN_DECIMALS,
            total_supply,
            deployer,
        )
        .value(U256::ZERO);

    let pending = deploy_call.send().await?;
    let tx_hash = *pending.tx_hash();
    ui::tx_hash("deploy tx", &format!("{tx_hash}"));

    let receipt = tokio::time::timeout(Duration::from_secs(120), pending.get_receipt())
        .await
        .map_err(|_| eyre!("deploy tx timed out after 120s"))??;

    let (token_id, token_addr) = extract_token_deployed_event(&receipt)?;
    ui::kv("token ID", &format!("{token_id}"));
    ui::address("token address", &format!("{token_addr}"));

    ui::info(&format!("deploying remote token to {dest_chain}..."));

    // A remote deploy creates a fresh token contract on the destination, which
    // costs far more destination-side gas than a transfer — and a consensus
    // source has no Axelarscan quote, so `gas_value` falls back to a low
    // constant. Pay 10× to cover the destination deploy execution (e.g. Stellar
    // token creation needed ~10×); the relayer refunds the unused remainder.
    // Mirrors `DEPLOY_GAS_MULTIPLIER` in the Solana-source ITS module.
    let hub_gas = gas_value * U256::from(10);
    let remote_call = factory
        .deployRemoteInterchainToken(salt, dest_chain.to_string(), hub_gas)
        .value(hub_gas);

    let pending = remote_call.send().await?;
    let tx_hash = *pending.tx_hash();
    ui::tx_hash("remote deploy tx", &format!("{tx_hash}"));

    let receipt = tokio::time::timeout(Duration::from_secs(120), pending.get_receipt())
        .await
        .map_err(|_| eyre!("remote deploy tx timed out after 120s"))??;

    ui::success(&format!(
        "remote deploy confirmed in block {}",
        receipt.block_number.unwrap_or(0)
    ));

    let deploy_message_id = match extract_contract_call_event(&receipt) {
        Ok((event_index, _, _, _, _)) => {
            let msg_id = format!("{tx_hash:#x}-{event_index}");
            ui::kv("remote deploy message ID", &msg_id);
            Some(msg_id)
        }
        Err(_) => None,
    };

    let cache = serde_json::json!({
        "tokenId": format!("{token_id}"),
        "tokenAddress": format!("{token_addr}"),
        "salt": format!("{salt}"),
    });
    save_its_cache(source_chain, dest_chain, &cache)?;

    super::helpers::hint_persist_axe_token(source_chain, &token_id);

    Ok((token_id, token_addr, deploy_message_id))
}

/// Remote-deploy an *already-deployed* interchain token (identified by its
/// `salt`) to a new destination chain, without minting a fresh token. Lets an
/// existing AXE be registered on a chain it isn't on yet (e.g. a legacy chain),
/// reusing the same `tokenId`. Returns the remote-deploy message id for the
/// hub-propagation wait. `dest_chain` is the destination axelarId.
pub(super) async fn remote_deploy_existing_token<P: Provider>(
    provider: &P,
    factory_addr: Address,
    salt: FixedBytes<32>,
    dest_chain: &str,
    gas_value: U256,
) -> eyre::Result<String> {
    let factory = InterchainTokenFactory::new(factory_addr, provider);
    // Same 10× headroom as a fresh remote deploy — the destination still
    // CREATE2s the token contract; the relayer refunds the remainder.
    let hub_gas = gas_value * U256::from(10);
    ui::info(&format!("registering existing token on {dest_chain}..."));
    let remote_call = factory
        .deployRemoteInterchainToken(salt, dest_chain.to_string(), hub_gas)
        .value(hub_gas);
    let pending = remote_call.send().await?;
    let tx_hash = *pending.tx_hash();
    ui::tx_hash("remote deploy tx", &format!("{tx_hash}"));
    let receipt = tokio::time::timeout(Duration::from_secs(120), pending.get_receipt())
        .await
        .map_err(|_| eyre!("remote deploy tx timed out after 120s"))??;
    let (event_index, _, _, _, _) = extract_contract_call_event(&receipt)
        .map_err(|e| eyre!("remote deploy emitted no ContractCall event: {e}"))?;
    let msg_id = format!("{tx_hash:#x}-{event_index}");
    ui::kv("remote deploy message ID", &msg_id);
    Ok(msg_id)
}

/// Pre-approve the ITS token manager on each derived key's balance so
/// `interchainTransfer` doesn't revert with `TakeTokenFailed` when the
/// underlying token manager is lock/unlock (e.g. canonical XRP wrapped on
/// EVM).
///
/// The spender that pulls the tokens is the **token manager** for the given
/// `token_id`, not the ITS proxy itself: ITS dispatches into
/// `tokenManager.takeToken(from, amount)`, which then does
/// `IERC20.safeTransferFrom(from, address(this), amount)` — `address(this)`
/// is the token manager. So the user's allowance is checked against the
/// token manager's address, which we look up via `ITS.tokenManagerAddress`.
///
/// Mint/burn-managed tokens (the AXE we deploy via `deployInterchainToken`)
/// don't need this — ITS is the minter and the InterchainToken's `burn(from,
/// amount)` skips the allowance check. But canonical tokens registered against
/// the ITS hub use `transferFrom(sender, token_manager, amount)` which
/// strictly requires `allowance >= amount`.
///
/// Calls are issued sequentially (cheap relative to the test itself) and
/// skipped per-key when the existing allowance already exceeds
/// `amount_per_key * 2`, so re-runs against the same derived keys reuse the
/// prior approval.
pub(super) async fn approve_its_for_keys(
    rpc_url: &str,
    token_addr: Address,
    its_proxy: Address,
    token_id: FixedBytes<32>,
    derived: &[PrivateKeySigner],
    amount_per_key: U256,
) -> eyre::Result<()> {
    let read_provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);
    let its = InterchainTokenService::new(its_proxy, &read_provider);
    let token_manager: Address = its
        .tokenManagerAddress(token_id)
        .call()
        .await
        .map_err(|e| {
            eyre!(
                "ITS.tokenManagerAddress({}) failed — token may not be registered yet: {e}",
                hex::encode(token_id)
            )
        })?;

    let read_token = ERC20::new(token_addr, &read_provider);
    let approve_threshold = amount_per_key.saturating_mul(U256::from(2));

    let spinner = ui::wait_spinner(&format!(
        "approving token manager {token_manager} for {} keys (lock/unlock token)...",
        derived.len()
    ));

    let mut approved = 0usize;
    for (i, signer) in derived.iter().enumerate() {
        let allowance = read_token
            .allowance(signer.address(), token_manager)
            .call()
            .await
            .unwrap_or_default();
        if allowance >= approve_threshold {
            continue;
        }
        let write_provider = ProviderBuilder::new()
            .wallet(signer.clone())
            .connect_http(rpc_url.parse()?);
        let token = ERC20::new(token_addr, &write_provider);
        let pending = token
            .approve(token_manager, U256::MAX)
            .send()
            .await
            .map_err(|e| eyre!("failed to approve token manager for key {i}: {e}"))?;
        pending
            .get_receipt()
            .await
            .map_err(|e| eyre!("approve receipt for key {i} failed: {e}"))?;
        approved += 1;
        spinner.set_message(format!(
            "approving token manager ({}/{} new approvals)...",
            approved,
            derived.len()
        ));
    }

    spinner.finish_and_clear();
    if approved == 0 {
        ui::info(&format!(
            "token manager already approved for all {} keys (reused from prior run)",
            derived.len()
        ));
    } else {
        ui::success(&format!(
            "approved token manager for {approved}/{} keys",
            derived.len()
        ));
    }
    Ok(())
}

/// Distribute ITS tokens from the deployer wallet to every derived signer.
/// Per-key sends are skipped when the existing balance already meets
/// `amount_per_key`, so re-runs against the same derived keys reuse prior
/// distribution.
pub(super) async fn distribute_tokens<P: Provider>(
    provider: &P,
    token_addr: Address,
    derived: &[PrivateKeySigner],
    amount_per_key: U256,
) -> eyre::Result<()> {
    let token = ERC20::new(token_addr, provider);

    let spinner = ui::wait_spinner(&format!("distributing tokens to {} keys...", derived.len()));

    for (i, signer) in derived.iter().enumerate() {
        let balance = token
            .balanceOf(signer.address())
            .call()
            .await
            .unwrap_or_default();
        if balance >= amount_per_key {
            continue;
        }

        let call = token.transfer(signer.address(), amount_per_key);
        let pending = call
            .send()
            .await
            .map_err(|e| eyre!("failed to transfer tokens to key {i}: {e}"))?;
        pending
            .get_receipt()
            .await
            .map_err(|e| eyre!("token transfer to key {i} failed: {e}"))?;

        spinner.set_message(format!(
            "distributing tokens ({}/{} done)...",
            i + 1,
            derived.len()
        ));
    }

    spinner.finish_and_clear();
    ui::success(&format!("distributed tokens to {} keys", derived.len()));
    Ok(())
}

/// Send a single `interchainTransfer` via the ITS Service and return metrics.
///
/// Calls `ITS.interchainTransfer(tokenId, destChain, destAddr, amount, metadata, gasValue)`,
/// which internally wraps the payload as `SEND_TO_HUB` and emits a `ContractCall`
/// to "axelar".
///
/// `explicit_nonce`: when `Some`, bypasses alloy's RPC-based nonce fetch to
/// avoid collisions when the same key fires again before the previous tx
/// confirms.
///
/// `gas_arg_scaling_factor`: per-source-chain divisor exponent applied to the
/// `gasValue` *argument* of `interchainTransfer` so it matches the
/// source-side gas service's expected unit. `msg.value` always stays in
/// 18-decimal EVM-wei (alloy's `.value()` takes wei). For Hedera the factor
/// is 10 — passing the 18-decimal value as the function arg yields a 10^10
/// mismatch and the call reverts with empty data (eth_estimateGas →
/// "CONTRACT_REVERT_EXECUTED, data: 0x" at submit time). Other EVM chains
/// in the matrix omit `gasScalingFactor` so callers pass 0 and behavior is
/// unchanged.
#[allow(clippy::too_many_arguments)]
pub(super) async fn execute_interchain_transfer<P: Provider>(
    provider: &P,
    its_proxy: Address,
    token_id: FixedBytes<32>,
    dest_chain: &str,
    receiver_bytes: &Bytes,
    amount: U256,
    gas_value: U256,
    gas_arg_scaling_factor: u32,
    explicit_nonce: Option<u64>,
) -> TxMetrics {
    let submit_start = Instant::now();

    let hub_gas = gas_value * U256::from(2);
    let gas_value_arg = if gas_arg_scaling_factor == 0 {
        hub_gas
    } else {
        hub_gas / U256::from(10).pow(U256::from(gas_arg_scaling_factor))
    };
    let its = InterchainTokenService::new(its_proxy, provider);

    // A receipt timeout on an EVM chain (all mine within seconds) almost always
    // means the tx was dropped from the mempool — not that it is slow — e.g. an
    // RPC that accepted `eth_sendRawTransaction` but never broadcast it
    // (observed on Avalanche via QuikNode). Resubmit, but only ever at the SAME
    // nonce (learned from the first send) so at most one tx can land: a dropped
    // tx's nonce is free and the resubmit fills it; a still-pending tx makes the
    // resubmit a same-nonce replacement, never a second transfer.
    const MAX_SUBMIT_ATTEMPTS: u32 = 3;
    let mut pinned_nonce = explicit_nonce;
    let mut last_hash: Option<TxHash> = None;
    for attempt in 0..MAX_SUBMIT_ATTEMPTS {
        let base_call = its
            .interchainTransfer(
                token_id,
                dest_chain.to_string(),
                receiver_bytes.clone(),
                amount,
                Bytes::new(),
                gas_value_arg,
            )
            .value(hub_gas);
        let call = match pinned_nonce {
            Some(n) => base_call.nonce(n),
            None => base_call,
        };

        let pending = match call.send().await {
            Ok(p) => p,
            Err(e) => {
                let es = e.to_string();
                // "nonce too low" / "already known" ⇒ a prior attempt actually
                // landed; return its receipt instead of a spurious failure.
                if let Some(h) = last_hash
                    && (es.contains("nonce too low")
                        || es.contains("already known")
                        || es.contains("replacement transaction underpriced"))
                    && let Ok(Some(receipt)) = provider.get_transaction_receipt(h).await
                {
                    return receipt_to_metrics(&receipt, h, its_proxy, submit_start);
                }
                return make_failure(submit_start, &es);
            }
        };

        let tx_hash = *pending.tx_hash();
        last_hash = Some(tx_hash);
        if pinned_nonce.is_none() {
            pinned_nonce = learn_nonce(provider, tx_hash).await;
        }

        match tokio::time::timeout(EVM_RECEIPT_TIMEOUT, pending.get_receipt()).await {
            Ok(Ok(receipt)) => {
                return receipt_to_metrics(&receipt, tx_hash, its_proxy, submit_start);
            }
            Ok(Err(e)) => {
                return make_failure_with_hash(submit_start, &e.to_string(), Some(tx_hash));
            }
            Err(_) => {
                // Deadline hit — did it land just after?
                if let Ok(Some(receipt)) = provider.get_transaction_receipt(tx_hash).await {
                    return receipt_to_metrics(&receipt, tx_hash, its_proxy, submit_start);
                }
                // Not mined. Resubmit only if the nonce is pinned (else a
                // resubmit could double-send) and attempts remain.
                if pinned_nonce.is_none() || attempt + 1 == MAX_SUBMIT_ATTEMPTS {
                    return make_failure_with_hash(
                        submit_start,
                        &format!(
                            "tx timed out (no receipt in {}s)",
                            EVM_RECEIPT_TIMEOUT.as_secs()
                        ),
                        Some(tx_hash),
                    );
                }
                ui::warn(&format!(
                    "source tx {tx_hash:#x} not mined in {}s — resubmitting at nonce {} \
                     (attempt {}/{MAX_SUBMIT_ATTEMPTS})",
                    EVM_RECEIPT_TIMEOUT.as_secs(),
                    pinned_nonce.unwrap_or_default(),
                    attempt + 2,
                ));
            }
        }
    }
    make_failure_with_hash(
        submit_start,
        &format!("tx timed out after {MAX_SUBMIT_ATTEMPTS} submit attempts"),
        last_hash,
    )
}

/// Build success (or revert) metrics from a mined receipt. Shared by the
/// happy-path receipt and the post-timeout "did it land late?" recheck.
fn receipt_to_metrics(
    receipt: &TransactionReceipt,
    tx_hash: TxHash,
    its_proxy: Address,
    submit_start: Instant,
) -> TxMetrics {
    let latency_ms = submit_start.elapsed().as_millis() as u64;
    // EVM receipts come back even for reverted txs — `status: 0x0` means the tx
    // mined but failed; surface the revert (with hash) rather than a confusing
    // "ContractCall event not found".
    if !receipt.status() {
        return make_failure_with_hash(
            submit_start,
            &format!(
                "source-side interchainTransfer reverted (status 0x0, gas_used {})",
                receipt.gas_used
            ),
            Some(tx_hash),
        );
    }
    match extract_contract_call_event(receipt) {
        Ok((event_index, _payload, payload_hash_bytes, dest_chain, dest_address)) => TxMetrics {
            signature: format!("{tx_hash:#x}-{event_index}"),
            submit_time_ms: 0,
            confirm_time_ms: Some(latency_ms),
            latency_ms: Some(latency_ms),
            compute_units: Some(receipt.gas_used),
            slot: receipt.block_number,
            success: true,
            error: None,
            payload: Vec::new(),
            payload_hash: alloy::hex::encode(payload_hash_bytes.as_slice()),
            source_address: format!("{its_proxy}"),
            gmp_destination_chain: dest_chain,
            gmp_destination_address: dest_address,
            send_instant: Some(submit_start),
            amplifier_timing: None,
        },
        Err(e) => make_failure_with_hash(
            submit_start,
            &format!("no ContractCall event: {e}"),
            Some(tx_hash),
        ),
    }
}

/// Best-effort read of a just-submitted tx's nonce so resubmits reuse it
/// (a replacement, never a duplicate). Returns `None` if the RPC can't return
/// the tx yet — the caller then declines to resubmit rather than risk a
/// double-send.
async fn learn_nonce<P: Provider>(provider: &P, tx_hash: TxHash) -> Option<u64> {
    match provider.get_transaction_by_hash(tx_hash).await {
        Ok(Some(tx)) => Some(tx.inner.nonce()),
        _ => None,
    }
}

fn make_failure(submit_start: Instant, error: &str) -> TxMetrics {
    make_failure_with_hash(submit_start, error, None)
}

/// Look up `gasScalingFactor` for a source EVM chain in chains-config. The
/// caller reads this once at run() entry and threads it through to
/// `execute_interchain_transfer`. Hedera = 10 (HTS native 8-dec ↔ EVM-wei
/// 18-dec ⇒ scale by 10^10); most chains omit the field ⇒ 0 (no scaling).
/// On any IO / parse failure returns 0 — over-paying the gasValue arg via
/// no-scaling is safe (gas service refunds the excess), whereas under-paying
/// causes the empty-data revert we're fixing.
pub(super) fn read_gas_arg_scaling_factor(
    config_path: &std::path::Path,
    source_chain_id: &str,
) -> u32 {
    let Ok(content) = std::fs::read_to_string(config_path) else {
        return 0;
    };
    let Ok(root) = serde_json::from_str::<serde_json::Value>(&content) else {
        return 0;
    };
    root.pointer(&format!("/chains/{source_chain_id}/gasScalingFactor"))
        .and_then(serde_json::Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())
        .unwrap_or(0)
}

/// Rescale 18-decimal-assumed sizing amounts to the source token's actual
/// `decimals()`. `compute_run_sizing` in every its_evm_to_<dest>.rs hardcodes
/// amounts in 18-decimal sub-units (e.g. `1e16 = 0.01 AXE`). That holds for
/// standard EVM-18 chains (Monad, HL, XRPL-EVM), but Hedera HTS-fork AXE is 6
/// decimals — 1e16 sub-units there is 1e10 AXE, exceeds wallet balance, and
/// the source burn reverts. Dividing by `10^(18 − decimals)` keeps the
/// intended 0.01 AXE meaning regardless of chain.
pub(super) async fn rescale_sizing_for_decimals<P: Provider>(
    amount_per_tx: &mut U256,
    amount_per_key: &mut U256,
    total_supply: &mut U256,
    provider: &P,
    token_addr: Address,
) -> eyre::Result<u8> {
    let decimals = ERC20::new(token_addr, provider)
        .decimals()
        .call()
        .await
        .map_err(|e| eyre!("failed to read source token decimals at {token_addr}: {e}"))?;
    if decimals < 18 {
        let divisor = U256::from(10).pow(U256::from(18 - u32::from(decimals)));
        *amount_per_tx /= divisor;
        *amount_per_key /= divisor;
        *total_supply /= divisor;
    }
    ui::kv("source token decimals", &decimals.to_string());
    Ok(decimals)
}

fn make_failure_with_hash(
    submit_start: Instant,
    error: &str,
    tx_hash: Option<TxHash>,
) -> TxMetrics {
    let elapsed_ms = submit_start.elapsed().as_millis() as u64;
    TxMetrics {
        signature: tx_hash.map_or_else(String::new, |h| format!("{h:#x}")),
        submit_time_ms: elapsed_ms,
        confirm_time_ms: None,
        latency_ms: None,
        compute_units: None,
        slot: None,
        success: false,
        error: Some(error.to_string()),
        payload: Vec::new(),
        payload_hash: String::new(),
        source_address: String::new(),
        gmp_destination_chain: String::new(),
        gmp_destination_address: String::new(),
        send_instant: None,
        amplifier_timing: None,
    }
}
