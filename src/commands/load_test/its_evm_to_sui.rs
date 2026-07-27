//! EVM -> Sui ITS load test.
//!
//! Pre-conditions handled outside axe (one-time per network):
//!   1. A Sui-side AXE coin is registered on Sui ITS
//!      (`axelar-contract-deployments/sui/its.js register-coin-from-info`).
//!      The resulting tokenId is stored under
//!      `chains.sui.contracts.AXE.objects.TokenId`.
//!   2. The same tokenId is linked on the EVM source-chain ITS via the
//!      `axelar-contract-deployments` link-token flow. After linking, the
//!      source signer must hold a balance of the resulting ERC-20.
//!
//! axe verifies (1) by reading the chain config and (2) by calling
//! `InterchainTokenService.interchainTokenAddress(tokenId)` on the source
//! chain; either missing precondition triggers a clear bail.
//!
//! Destination-side execution is driven by the cgp-sui relayer auto-calling
//! `example::its::receive_interchain_transfer<T>` on `MessageApproved`. We
//! reuse `verify::verify_onchain_sui_gmp` against the `ItsChannelId` since
//! the gateway emits the same `MessageExecuted` event regardless of who
//! called `gateway::execute`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use alloy::{
    primitives::{Address, Bytes, FixedBytes, U256},
    providers::{Provider, ProviderBuilder},
    signers::local::PrivateKeySigner,
};
use eyre::eyre;
use futures::future::join_all;
use tokio::sync::{Mutex, Semaphore};

use super::keypairs;
use super::metrics::{ComputeUnitSummary, LoadTestReport, ReportInput, TxMetrics};
use super::{
    LoadTestArgs, check_evm_balance, finalize_sui_dest_run_its, load_sui_main_wallet,
    read_sui_axe_token_id, sui_its_dest_lookup, validate_evm_rpc,
};
use crate::config::ChainsConfig;
use crate::evm::{ERC20, InterchainTokenService};
use crate::ui;

const MAX_CONCURRENT_SENDS: usize = 100;
const MAX_RETRIES: u32 = 5;

/// Default gas value: tries the Axelarscan `estimateGasFee` quote for the
/// route (× 1.5); falls back to a route-agnostic constant when the API
/// can't be reached.
async fn default_gas_value_wei(args: &LoadTestArgs) -> u128 {
    if let Some(quoted) = quote_route_gas(args).await {
        return quoted;
    }
    fallback_gas_value_wei(args.network, &args.source_chain)
}

async fn quote_route_gas(args: &LoadTestArgs) -> Option<u128> {
    let cfg = crate::config::ChainsConfig::load(&args.config).ok()?;
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

fn fallback_gas_value_wei(network: crate::types::Network, _source_chain: &str) -> u128 {
    match network {
        crate::types::Network::DevnetAmplifier => 0,
        _ => 10_000_000_000_000_000,
    }
}

/// Sizing parameters derived from CLI flags: burst vs sustained, key counts,
/// expected transfer total.
struct RunSizing {
    burst_mode: bool,
    sustained_params: Option<(u64, u64)>,
    num_keys: usize,
    num_txs: usize,
    total_expected: u64,
}

/// Resolved EVM source-chain context: signer, ITS proxy + linked token, RPC.
struct EvmContext {
    main_signer: PrivateKeySigner,
    rpc_url: String,
    its_proxy_addr: Address,
    token_id: FixedBytes<32>,
    linked_token_addr: Address,
    gas_value_wei: u128,
}

/// Resolved Sui destination context: recipient bytes (for interchainTransfer
/// payload), Sui RPC, display address. (The ITS channel is logged for
/// operators but the hub verifier keys off gateway events, not the channel.)
struct SuiContext {
    recipient_bytes: Bytes,
    recipient_display: String,
    rpc: String,
}

pub async fn run(args: LoadTestArgs, _run_start: Instant) -> eyre::Result<()> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;

    let evm_rpc_url = args.source_rpc.clone();
    validate_evm_rpc(&evm_rpc_url).await?;

    let cfg = ChainsConfig::load(&args.config)?;
    verify_axelar_prerequisites(&cfg, dest)?;

    ui::kv("source", src);
    ui::kv("destination", dest);
    ui::kv(
        "protocol",
        "ITS (interchainTransfer via hub, Sui destination)",
    );

    let evm = resolve_evm_context(&args, &cfg, evm_rpc_url.clone()).await?;
    let sui = resolve_sui_context(&args)?;
    let sizing = compute_run_sizing(&args);

    let derived = derive_and_fund_signers(&evm, &sizing).await?;
    let amount_per_tx = U256::from(1u64);

    if sizing.burst_mode {
        run_burst_pipeline(&args, &evm, &sui, &derived, &sizing, amount_per_tx).await
    } else {
        run_sustained_pipeline(&args, &evm, &sui, derived, &sizing, amount_per_tx).await
    }
}

fn verify_axelar_prerequisites(cfg: &ChainsConfig, dest: &str) -> eyre::Result<()> {
    if cfg.axelar.contract_address("Gateway", dest).is_err() {
        eyre::bail!(
            "destination chain '{dest}' has no Cosmos Gateway in the config — verification would fail."
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

async fn resolve_evm_context(
    args: &LoadTestArgs,
    cfg: &ChainsConfig,
    rpc_url: String,
) -> eyre::Result<EvmContext> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;
    let main_signer = parse_main_signer(args.private_key.as_deref())?;
    let main_addr = main_signer.address();
    let read_provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);
    check_evm_balance(&read_provider, main_addr).await?;
    let balance: u128 = read_provider.get_balance(main_addr).await?.to();
    let eth = balance as f64 / 1e18;
    ui::kv("wallet", &format!("{main_addr} ({eth:.6} ETH)"));

    let evm_chain_cfg = cfg
        .chains
        .get(src)
        .ok_or_else(|| eyre!("source chain '{src}' not found in config"))?;
    let its_proxy_addr: Address = evm_chain_cfg
        .contract_address("InterchainTokenService", src)?
        .parse()?;
    ui::address("ITS service", &format!("{its_proxy_addr}"));

    let token_id_bytes = read_sui_axe_token_id(&args.config, dest, args.token_id.as_deref())?;
    let token_id = FixedBytes::<32>::from(token_id_bytes);
    ui::kv(
        "Sui token id",
        &format!("0x{}", hex::encode(token_id_bytes)),
    );

    let its = InterchainTokenService::new(its_proxy_addr, &read_provider);
    let linked_token_addr = its
        .interchainTokenAddress(token_id)
        .call()
        .await
        .map_err(|e| eyre!("ITS.interchainTokenAddress({token_id}) failed: {e}"))?;
    if linked_token_addr == Address::ZERO {
        eyre::bail!(
            "EVM source ITS at {its_proxy_addr} has no token linked to Sui AXE tokenId 0x{}. \
             Run the one-time off-axe link-token step from axelar-contract-deployments, then \
             ensure the source signer {main_addr} holds a balance of the resulting ERC-20.",
            hex::encode(token_id_bytes),
        );
    }
    ui::address("EVM linked ERC-20", &format!("{linked_token_addr}"));

    let token = ERC20::new(linked_token_addr, &read_provider);
    let main_token_balance = token.balanceOf(main_addr).call().await.unwrap_or_default();
    if main_token_balance == U256::ZERO {
        eyre::bail!(
            "source signer {main_addr} has zero balance of linked ERC-20 {linked_token_addr}. \
             Mint or transfer some AXE to it before re-running."
        );
    }
    ui::kv(
        "linked ERC-20 balance (main)",
        &main_token_balance.to_string(),
    );

    let gas_value_wei = parse_gas_value_wei(args).await?;
    ui::kv(
        "gas value",
        &format!("{gas_value_wei} wei (per-leg, x2 for hub)"),
    );

    Ok(EvmContext {
        main_signer,
        rpc_url,
        its_proxy_addr,
        token_id,
        linked_token_addr,
        gas_value_wei,
    })
}

fn resolve_sui_context(args: &LoadTestArgs) -> eyre::Result<SuiContext> {
    let dest = &args.destination_chain;
    let sui_wallet = load_sui_main_wallet()?;
    let recipient_bytes = Bytes::from(sui_wallet.address.as_bytes().to_vec());
    let recipient_display = sui_wallet.address_hex();
    ui::address("destination Sui address", &recipient_display);

    let (its_channel, rpc) = sui_its_dest_lookup(&args.config, dest, Some(&args.destination_rpc))?;
    ui::address("Sui ITS channel (destination)", &its_channel);

    Ok(SuiContext {
        recipient_bytes,
        recipient_display,
        rpc,
    })
}

fn compute_run_sizing(args: &LoadTestArgs) -> RunSizing {
    let sustained_params = args.tps.zip(args.duration_secs);
    let burst_mode = sustained_params.is_none();
    let (num_keys, num_txs, total_expected) = if burst_mode {
        let n = args.num_txs.max(1) as usize;
        (n, n, n as u64)
    } else {
        let (tps, dur) = sustained_params.expect("burst_mode is false");
        let tps_usize = tps as usize;
        let total = tps * dur;
        (
            tps_usize * args.key_cycle as usize,
            tps_usize * args.key_cycle as usize,
            total,
        )
    };
    RunSizing {
        burst_mode,
        sustained_params,
        num_keys,
        num_txs,
        total_expected,
    }
}

async fn derive_and_fund_signers(
    evm: &EvmContext,
    sizing: &RunSizing,
) -> eyre::Result<Vec<PrivateKeySigner>> {
    let main_key: [u8; 32] = evm.main_signer.to_bytes().into();
    let derived = keypairs::derive_evm_signers(&main_key, sizing.num_keys)?;

    // Each tx forwards 2 * gas_value as msg.value (one hub leg, one
    // destination leg). Fund derived keys with 4x that headroom so each
    // can cover its share of sends plus tx fees.
    let read_provider = ProviderBuilder::new().connect_http(evm.rpc_url.parse()?);
    let per_call_msg_value_wei = evm.gas_value_wei.saturating_mul(2);
    keypairs::ensure_funded_evm_with_extra(
        &read_provider,
        &evm.main_signer,
        &derived,
        per_call_msg_value_wei.saturating_mul(2),
    )
    .await?;

    // Distribute linked AXE: each derived signer receives enough for its
    // share of the run. Burst: 1 per tx. Sustained: `key_cycle` per key,
    // since each key serves `key_cycle` rotations.
    let txs_per_key = if sizing.burst_mode {
        1u64
    } else {
        let (_, dur) = sizing.sustained_params.expect("burst_mode is false");
        // total = tps * dur. tps * key_cycle = num_keys. So txs_per_key = dur / key_cycle.
        let key_cycle = (sizing.num_keys as u64).max(1) / sizing.sustained_params.unwrap().0;
        // Round up to ensure derived keys never run dry mid-cycle.
        dur.div_ceil(key_cycle)
    };
    let amount_per_key = U256::from(txs_per_key);

    let write_provider = ProviderBuilder::new()
        .wallet(evm.main_signer.clone())
        .connect_http(evm.rpc_url.parse()?);
    super::its_evm_source::distribute_tokens(
        &write_provider,
        evm.linked_token_addr,
        &derived,
        amount_per_key,
    )
    .await?;

    Ok(derived)
}

async fn run_burst_pipeline(
    args: &LoadTestArgs,
    evm: &EvmContext,
    sui: &SuiContext,
    derived: &[PrivateKeySigner],
    sizing: &RunSizing,
    amount_per_tx: U256,
) -> eyre::Result<()> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;
    let num_txs = sizing.num_txs;
    let test_start = Instant::now();
    let metrics_list: Arc<Mutex<Vec<TxMetrics>>> = Arc::new(Mutex::new(Vec::new()));
    let confirmed_counter = Arc::new(AtomicU64::new(0));
    let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_SENDS));
    let spinner = ui::wait_spinner(&format!("sending (0/{num_txs} confirmed)..."));

    let dest_chain_id = args.destination_axelar_id.clone();
    let gas_value = U256::from(evm.gas_value_wei);
    let gas_arg_scaling_factor =
        super::its_evm_source::read_gas_arg_scaling_factor(&args.config, &args.source_axelar_id);
    let mut tasks = Vec::with_capacity(num_txs);
    for derived_signer in derived {
        let metrics_clone = Arc::clone(&metrics_list);
        let counter = Arc::clone(&confirmed_counter);
        let sem = Arc::clone(&semaphore);
        let sp = spinner.clone();
        let total = num_txs;
        let dc = dest_chain_id.clone();
        let rb = sui.recipient_bytes.clone();
        let its_proxy = evm.its_proxy_addr;
        let tid = evm.token_id;
        let provider = ProviderBuilder::new()
            .wallet(derived_signer.clone())
            .connect_http(evm.rpc_url.parse()?);

        let handle = tokio::spawn(async move {
            let _permit = sem.acquire().await.unwrap();
            let mut last = None;
            for attempt in 0..=MAX_RETRIES {
                let result = super::its_evm_source::execute_interchain_transfer(
                    super::its_evm_source::InterchainTransferRequest {
                        provider: &provider,
                        its_proxy,
                        token_id: tid,
                        destination_chain: &dc,
                        receiver: &rb,
                        amount: amount_per_tx,
                        gas_value,
                        gas_arg_scaling_factor,
                        explicit_nonce: None,
                    },
                )
                .await;
                if result.success || attempt == MAX_RETRIES {
                    last = Some(result);
                    break;
                }
                let rate_limited = result.error.as_deref().is_some_and(|e| e.contains("429"));
                if !rate_limited {
                    last = Some(result);
                    break;
                }
                tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
            }
            let m = last.expect("exited retry loop with no result");
            if m.success {
                let done = counter.fetch_add(1, Ordering::Relaxed) + 1;
                sp.set_message(format!("sending ({done}/{total} confirmed)..."));
            }
            metrics_clone.lock().await.push(m);
        });
        tasks.push(handle);
    }
    let total_submitted = tasks.len() as u64;
    join_all(tasks).await;
    spinner.finish_and_clear();
    let confirmed_count = confirmed_counter.load(Ordering::Relaxed);
    ui::success(&format!(
        "sent {confirmed_count}/{total_submitted} confirmed"
    ));

    let test_duration = test_start.elapsed().as_secs_f64();
    let metrics = metrics_list.lock().await.clone();
    let mut report = LoadTestReport::from_transactions(
        ReportInput {
            source_chain: src.to_string(),
            destination_chain: dest.to_string(),
            destination_address: sui.recipient_display.clone(),
            num_txs: args.num_txs,
            num_keys: num_txs,
            total_submitted,
            test_duration_secs: test_duration,
            compute_unit_summary: ComputeUnitSummary::Omit,
        },
        metrics,
    );

    finalize_sui_dest_run_its(args, &mut report, &sui.rpc, test_start).await
}

async fn run_sustained_pipeline(
    args: &LoadTestArgs,
    evm: &EvmContext,
    sui: &SuiContext,
    derived: Vec<PrivateKeySigner>,
    sizing: &RunSizing,
    amount_per_tx: U256,
) -> eyre::Result<()> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;
    let (tps, duration_secs) = sizing.sustained_params.expect("burst_mode is false");
    let tps_usize = tps as usize;
    let key_cycle = args.key_cycle as usize;

    // Pre-fetch each derived signer's nonce so the rate-limited loop can
    // bump them locally per dispatch (avoids RPC round-trips on each tx).
    let read_provider = ProviderBuilder::new().connect_http(evm.rpc_url.parse()?);
    let mut nonces: Vec<u64> = Vec::with_capacity(sizing.num_keys);
    for s in &derived {
        let n = read_provider.get_transaction_count(s.address()).await?;
        nonces.push(n);
    }

    let spinner = ui::wait_spinner(&format!(
        "[0/{duration_secs}s] starting sustained ITS send..."
    ));
    let test_start = Instant::now();
    let dest_chain_id = args.destination_axelar_id.clone();
    let rpc_url = evm.rpc_url.clone();
    let its_proxy = evm.its_proxy_addr;
    let token_id = evm.token_id;
    let recipient_bytes = sui.recipient_bytes.clone();
    let gas_value = U256::from(evm.gas_value_wei);
    let gas_arg_scaling_factor =
        super::its_evm_source::read_gas_arg_scaling_factor(&args.config, &args.source_axelar_id);

    let make_task: super::sustained::MakeTask =
        Box::new(move |key_idx: usize, nonce: Option<u64>| {
            let dc = dest_chain_id.clone();
            let rb = recipient_bytes.clone();
            let amt = amount_per_tx;
            let provider = ProviderBuilder::new()
                .wallet(derived[key_idx].clone())
                .connect_http(rpc_url.parse().expect("invalid RPC URL"));

            Box::pin(async move {
                super::its_evm_source::execute_interchain_transfer(
                    super::its_evm_source::InterchainTransferRequest {
                        provider: &provider,
                        its_proxy,
                        token_id,
                        destination_chain: &dc,
                        receiver: &rb,
                        amount: amt,
                        gas_value,
                        gas_arg_scaling_factor,
                        explicit_nonce: nonce,
                    },
                )
                .await
            })
        });

    let result = super::sustained::run_sustained_loop(
        tps_usize,
        duration_secs,
        key_cycle,
        Some(nonces),
        make_task,
        None,
        spinner,
    )
    .await;

    let mut report = super::sustained::build_sustained_report(
        result,
        src,
        dest,
        &sui.recipient_display,
        sizing.total_expected,
        sizing.num_keys,
    );
    report.tps = Some(tps);
    report.duration_secs = Some(duration_secs);

    finalize_sui_dest_run_its(args, &mut report, &sui.rpc, test_start).await
}

fn parse_main_signer(private_key: Option<&str>) -> eyre::Result<PrivateKeySigner> {
    let key = private_key.ok_or_else(|| {
        eyre!("EVM private key required. Set EVM_PRIVATE_KEY env var or use --private-key")
    })?;
    key.parse::<PrivateKeySigner>()
        .map_err(|e| eyre!("invalid EVM private key: {e}"))
}

async fn parse_gas_value_wei(args: &LoadTestArgs) -> eyre::Result<u128> {
    match args.gas_value.as_deref() {
        Some(s) if !s.is_empty() => s
            .parse::<u128>()
            .map_err(|e| eyre!("invalid --gas-value: {e}")),
        _ => Ok(default_gas_value_wei(args).await),
    }
}
