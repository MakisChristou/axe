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
use solana_sdk::signer::Signer;
use tokio::sync::{Mutex, Semaphore};

use super::its_evm_source::{
    self, EvmSource, ItsContracts, deploy_its_token, derive_and_fund_keys, distribute_tokens,
    execute_interchain_transfer, init_evm_source, resolve_its_contracts,
    verify_axelar_prerequisites,
};
use super::metrics::{ComputeUnitSummary, LoadTestReport, ReportInput, TxMetrics};
use super::{LoadTestArgs, finish_report, read_its_cache, validate_evm_rpc, validate_solana_rpc};
use crate::config::ChainsConfig;
use crate::evm::{ERC20, InterchainTokenService};
use crate::ui;

fn fallback_gas_value_wei(network: crate::types::Network, _source_chain: &str) -> u128 {
    match network {
        crate::types::Network::DevnetAmplifier => 0,
        _ => 10_000_000_000_000_000, // 0.01 ETH
    }
}
const MAX_CONCURRENT_SENDS: usize = 100;
const MAX_RETRIES: u32 = 5;

pub async fn run(args: LoadTestArgs, _run_start: Instant) -> eyre::Result<()> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;

    let evm_rpc_url = args.source_rpc.clone();
    validate_evm_rpc(&evm_rpc_url).await?;
    validate_solana_rpc(&args.destination_rpc).await?;

    let cfg = ChainsConfig::load(&args.config)?;
    verify_axelar_prerequisites(&cfg, dest)?;

    ui::kv("source", src);
    ui::kv("destination", dest);
    ui::kv("protocol", "ITS (interchainTransfer via hub)");

    let evm_source = init_evm_source(&args, &evm_rpc_url).await?;
    let its = resolve_its_contracts(&cfg, src)?;
    let gas_value_wei = parse_gas_value_wei(&args).await?;
    let gas_value = U256::from(gas_value_wei);
    let mut sizing = compute_run_sizing(&args);

    let token =
        resolve_or_deploy_token(&args, &evm_source, &its, &evm_rpc_url, &sizing, gas_value).await?;

    // compute_run_sizing assumes EVM-18 source decimals; rescale to the
    // actual on-chain decimals (Hedera HTS-fork AXE = 6 dec).
    {
        let read_provider = ProviderBuilder::new().connect_http(evm_rpc_url.parse()?);
        super::its_evm_source::rescale_sizing_for_decimals(
            &mut sizing.amount_per_tx,
            &mut sizing.amount_per_key,
            &mut sizing.total_supply,
            &read_provider,
            token.token_addr,
        )
        .await?;
    }

    if let Some(ref deploy_msg_id) = token.deploy_message_id {
        super::verify::wait_for_its_remote_deploy_to_solana(
            &args.config,
            src,
            dest,
            deploy_msg_id,
            &args.destination_rpc,
            args.network,
        )
        .await?;
    }

    let derived = derive_and_fund_keys(
        &evm_source.signer,
        &evm_source.main_key,
        &evm_rpc_url,
        sizing.num_keys,
        hub_gas_extra_per_key(&args, &sizing, gas_value_wei),
        &args.source_axelar_id,
    )
    .await?;

    let token_provider = ProviderBuilder::new()
        .wallet(evm_source.signer.clone())
        .connect_http(evm_rpc_url.parse()?);
    distribute_tokens(
        &token_provider,
        token.token_addr,
        &derived,
        sizing.amount_per_key,
    )
    .await?;

    let sol_keypair = crate::solana::load_keypair(args.keypair.as_deref())?;
    let receiver_bytes = Bytes::from(sol_keypair.pubkey().to_bytes().to_vec());

    let gas_arg_scaling_factor =
        super::its_evm_source::read_gas_arg_scaling_factor(&args.config, &args.source_axelar_id);

    let targets = TransferTargets {
        its_proxy_addr: its.its_proxy_addr,
        token_id: token.token_id,
        gas_value,
        gas_arg_scaling_factor,
        receiver_bytes,
    };

    if !sizing.burst_mode {
        run_sustained_pipeline(&args, &cfg, &evm_rpc_url, &derived, &sizing, &targets).await
    } else {
        run_burst_pipeline(&args, &evm_rpc_url, &derived, &sizing, &targets).await
    }
}

/// Sizing parameters derived from CLI flags: chooses burst vs sustained,
/// number of ephemeral wallets, per-tx amount, and supply parameters.
struct RunSizing {
    burst_mode: bool,
    sustained_params: Option<(u64, u64)>,
    num_keys: usize,
    num_txs: usize,
    total_expected: u64,
    amount_per_tx: U256,
    amount_per_key: U256,
    total_supply: U256,
}

/// Resolved interchain token: cached, user-supplied, or freshly deployed.
/// `deploy_message_id` is `Some` only when the helper performed a remote
/// deploy in this run.
struct TokenIdentity {
    token_id: FixedBytes<32>,
    token_addr: Address,
    deploy_message_id: Option<String>,
}

/// Per-tx send parameters consumed by both the sustained and burst pipelines:
/// the ITS service to call, the token to push through it, the gas attached to
/// each interchain transfer, and the Solana recipient.
struct TransferTargets {
    its_proxy_addr: Address,
    token_id: FixedBytes<32>,
    gas_value: U256,
    /// See `its_evm_source::read_gas_arg_scaling_factor` — Hedera = 10,
    /// other EVM chains = 0.
    gas_arg_scaling_factor: u32,
    receiver_bytes: Bytes,
}

/// Parse the user-supplied gas value (wei), defaulting via the relayer-aware
/// `estimateGasFee` quote for the route, and emit the matching UI line.
async fn parse_gas_value_wei(args: &LoadTestArgs) -> eyre::Result<u128> {
    let gas_value_wei: u128 = match args.gas_value.as_deref() {
        Some(v) => v.parse().map_err(|e| eyre!("invalid --gas-value: {e}"))?,
        None => {
            its_evm_source::default_gas_value_wei(
                args,
                fallback_gas_value_wei(args.network, &args.source_chain),
            )
            .await
        }
    };

    ui::kv(
        "gas value",
        &format!(
            "{gas_value_wei} wei ({:.6} ETH)",
            gas_value_wei as f64 / 1e18
        ),
    );
    Ok(gas_value_wei)
}

/// Decide burst vs sustained, ephemeral wallet count, expected tx count, and
/// per-tx / per-key / total-supply amounts.
fn compute_run_sizing(args: &LoadTestArgs) -> RunSizing {
    let sustained_params = args.tps.zip(args.duration_secs);
    let burst_mode = sustained_params.is_none();
    let (num_keys, total_expected) = if burst_mode {
        let n = args.num_txs.max(1) as usize;
        (n, args.num_txs.max(1))
    } else {
        let (tps, dur) = sustained_params.expect("burst_mode is false");
        let tps = tps as usize;
        (tps * args.key_cycle as usize, tps as u64 * dur)
    };
    // Keep num_txs as alias for burst compat (equals num_keys in burst mode)
    let num_txs = num_keys;
    // 0.01 token per tx (10^16 at 18 decimals). Still well above ITS hub
    // decimal truncation between EVM-18 and Solana-6 (truncated to 10_000).
    let amount_per_tx = U256::from(10_000_000_000_000_000u128);
    // Distribute 100x per key so cached tokens last across many runs.
    let amount_per_key = amount_per_tx * U256::from(100);
    // Mint a large fixed supply so the token can be reused across runs without redeploying.
    let total_supply = U256::from(1_000_000) * U256::from(1_000_000_000_000_000_000u128); // 1M tokens

    RunSizing {
        burst_mode,
        sustained_params,
        num_keys,
        num_txs,
        total_expected,
        amount_per_tx,
        amount_per_key,
        total_supply,
    }
}

/// Resolve the ITS token to use this run: honour `--token-id`, then fall back
/// to the source/dest cache (deploying fresh if the cached token has
/// insufficient supply or no longer exists), and finally deploy a brand-new
/// token if no cache hit.
async fn resolve_or_deploy_token(
    args: &LoadTestArgs,
    evm_source: &EvmSource,
    its: &ItsContracts,
    evm_rpc_url: &str,
    sizing: &RunSizing,
    gas_value: U256,
) -> eyre::Result<TokenIdentity> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;
    let write_provider = ProviderBuilder::new()
        .wallet(evm_source.signer.clone())
        .connect_http(evm_rpc_url.parse()?);

    let its_service = InterchainTokenService::new(its.its_proxy_addr, &write_provider);

    // Resolution order: --token-id (CLI override) → chains-config
    // `contracts.AXE.tokenId` (per-source pre-registration, lets CI skip the
    // deploy + hub-routed remote-deploy and collapse to a single
    // interchainTransfer — but only when the configured wallet actually holds
    // the AXE; a wallet with no balance falls through to a fresh deploy) →
    // local file cache (per src+dst) → fresh deploy.
    //
    // Hedera special-cases: auto-deploy reverts with
    // `InitialSupplyUnsupported` (and the broader path is currently broken
    // upstream — see TODOs in the workflow + script). For Hedera-source we
    // require an explicit pre-registered token; the error message points at
    // the deployments-repo Hedera setup.
    let needed = sizing.amount_per_tx * U256::from(sizing.num_txs);
    let config_axe = super::helpers::reusable_config_axe(
        &args.config,
        src,
        its.its_proxy_addr,
        &write_provider,
        evm_source.deployer_address,
        needed,
    )
    .await?;

    let (token_id, token_addr, deploy_message_id) = if args.source_axelar_id == "hedera" {
        // Hedera ITS uses `registeredTokenAddress` (HTS tokens lack
        // deterministic addresses, so `interchainTokenAddress` was removed
        // in the fork — see contracts/hedera/README.md in commonprefix's
        // interchain-token-service@hedera-its).
        let token_id =
            super::helpers::resolve_hedera_axe_token(&args.config, src, args.token_id.as_deref())?;
        let addr = its_service
            .registeredTokenAddress(token_id)
            .call()
            .await
            .map_err(|e| eyre!("failed to look up token address for {token_id}: {e}"))?;
        ui::kv("token ID (Hedera, pre-registered)", &format!("{token_id}"));
        ui::address("token address", &format!("{addr}"));
        (token_id, addr, None)
    } else if let Some(ref tid) = args.token_id {
        // User provided a token ID
        let token_id: FixedBytes<32> = tid.parse().map_err(|e| eyre!("invalid --token-id: {e}"))?;
        let addr = its_service
            .interchainTokenAddress(token_id)
            .call()
            .await
            .map_err(|e| eyre!("failed to look up token address for {token_id}: {e}"))?;
        ui::kv("token ID (provided)", &format!("{token_id}"));
        ui::address("token address", &format!("{addr}"));
        (token_id, addr, None)
    } else if let Some((tid, addr)) = config_axe {
        ui::kv("token ID (chains-config)", &format!("{tid}"));
        ui::address("token address", &format!("{addr}"));
        (tid, addr, None)
    } else {
        // Check cache
        let cache = read_its_cache(src, dest);
        let cached = cache
            .get("tokenId")
            .and_then(|v| v.as_str())
            .and_then(|tid| tid.parse::<FixedBytes<32>>().ok())
            .zip(
                cache
                    .get("tokenAddress")
                    .and_then(|v| v.as_str())
                    .and_then(|a| a.parse::<Address>().ok()),
            );

        if let Some((tid, addr)) = cached {
            // Verify token still exists and deployer has enough balance
            let token = ERC20::new(addr, &write_provider);
            let needed = sizing.amount_per_tx * U256::from(sizing.num_txs);
            let balance = token
                .balanceOf(evm_source.deployer_address)
                .call()
                .await
                .unwrap_or_default();
            if balance >= needed {
                ui::info(&format!("reusing cached ITS token: {addr}"));
                ui::kv("token ID (cached)", &format!("{tid}"));
                (tid, addr, None)
            } else if balance > U256::ZERO {
                ui::warn(&format!(
                    "cached token has insufficient supply ({balance} < {needed}), deploying fresh..."
                ));
                deploy_its_token(
                    &write_provider,
                    its.its_factory_addr,
                    evm_source.deployer_address,
                    dest,
                    sizing.total_supply,
                    src,
                    gas_value,
                )
                .await?
            } else {
                ui::warn("cached token no longer exists, deploying fresh...");
                deploy_its_token(
                    &write_provider,
                    its.its_factory_addr,
                    evm_source.deployer_address,
                    dest,
                    sizing.total_supply,
                    src,
                    gas_value,
                )
                .await?
            }
        } else {
            deploy_its_token(
                &write_provider,
                its.its_factory_addr,
                evm_source.deployer_address,
                dest,
                sizing.total_supply,
                src,
                gas_value,
            )
            .await?
        }
    };

    Ok(TokenIdentity {
        token_id,
        token_addr,
        deploy_message_id,
    })
}

/// Compute the per-key hub-gas funding amount for this run. Burst mode fires
/// each key once; sustained mode fires each key `ceil(duration / key_cycle)`
/// times with a 20% buffer.
fn hub_gas_extra_per_key(args: &LoadTestArgs, sizing: &RunSizing, gas_value_wei: u128) -> u128 {
    let hub_gas_value_wei = gas_value_wei.saturating_mul(2);
    if sizing.burst_mode {
        hub_gas_value_wei
    } else {
        let dur = sizing.sustained_params.expect("burst_mode is false").1;
        let rounds = dur.div_ceil(args.key_cycle);
        let buffered = rounds + rounds / 5 + 1;
        hub_gas_value_wei.saturating_mul(buffered as u128)
    }
}

/// Drive the sustained-mode pipeline: pre-fetch nonces, spawn the streaming
/// Solana ITS verifier, run the EVM sustained sender loop, stitch amplifier
/// timings back into the report, and hand off to `finish_report`.
async fn run_sustained_pipeline(
    args: &LoadTestArgs,
    cfg: &ChainsConfig,
    evm_rpc_url: &str,
    derived: &[PrivateKeySigner],
    sizing: &RunSizing,
    targets: &TransferTargets,
) -> eyre::Result<()> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;
    let tps = sizing.sustained_params.expect("burst_mode is false").0 as usize;
    let duration_secs = sizing.sustained_params.expect("burst_mode is false").1;
    let key_cycle = args.key_cycle as usize;
    let rpc_url_str = evm_rpc_url.to_string();

    // Pre-fetch nonces.
    let nonce_provider = ProviderBuilder::new().connect_http(evm_rpc_url.parse()?);
    let mut nonces: Vec<u64> = Vec::with_capacity(sizing.num_keys);
    for signer in derived {
        let n = nonce_provider
            .get_transaction_count(signer.address())
            .await?;
        nonces.push(n);
    }

    // Streaming verification: run concurrently with sends.
    let (verify_tx, verify_rx) = tokio::sync::mpsc::unbounded_channel();
    let send_done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (spinner_tx, spinner_rx) = tokio::sync::oneshot::channel::<indicatif::ProgressBar>();

    let has_voting_verifier = cfg
        .axelar
        .contract_address("VotingVerifier", &args.source_chain)
        .is_ok();

    let vconfig = args.config.clone();
    let vsource = args.source_axelar_id.clone();
    let vdest = args.destination_axelar_id.clone();
    let vdest_rpc = args.destination_rpc.clone();
    let vdone = std::sync::Arc::clone(&send_done);
    let vnetwork = args.network;
    let verify_handle = tokio::spawn(async move {
        let spinner = spinner_rx.await.expect("spinner channel dropped");
        super::verify::verify_onchain_solana_its_streaming(
            &vconfig, &vsource, &vdest, &vdest_rpc, vnetwork, verify_rx, vdone, spinner,
        )
        .await
    });

    let spinner = ui::wait_spinner(&format!(
        "[0/{duration_secs}s] starting sustained ITS send..."
    ));
    let _ = spinner_tx.send(spinner.clone());

    let test_start = Instant::now();
    let dest_chain_s = dest.to_string();
    let derived_owned: Vec<PrivateKeySigner> = derived.to_vec();
    let amount_per_tx = sizing.amount_per_tx;
    let its_proxy_addr = targets.its_proxy_addr;
    let token_id = targets.token_id;
    let gas_value = targets.gas_value;
    let gas_arg_scaling_factor = targets.gas_arg_scaling_factor;
    let receiver_bytes = targets.receiver_bytes.clone();

    let make_task: super::sustained::MakeTask =
        Box::new(move |key_idx: usize, nonce: Option<u64>| {
            let dc = dest_chain_s.clone();
            let gv = gas_value;
            let gsf = gas_arg_scaling_factor;
            let rb = receiver_bytes.clone();
            let amt = amount_per_tx;
            let its_proxy = its_proxy_addr;
            let tid = token_id;
            let url = rpc_url_str.clone();
            let vtx = verify_tx.clone();
            let has_vv = has_voting_verifier;

            let provider = ProviderBuilder::new()
                .wallet(derived_owned[key_idx].clone())
                .connect_http(url.parse().expect("invalid RPC URL"));

            Box::pin(async move {
                let mut result = execute_interchain_transfer(
                    &provider, its_proxy, tid, &dc, &rb, amt, gv, gsf, nonce,
                )
                .await;
                if result.success {
                    match super::verify::tx_to_pending_its(&result, has_vv) {
                        Ok(pending) => {
                            let _ = vtx.send(pending);
                        }
                        Err(e) => {
                            result.success = false;
                            result.error = Some(format!("failed to build verification state: {e}"));
                        }
                    }
                }
                result
            })
        });

    let result = super::sustained::run_sustained_loop(
        tps,
        duration_secs,
        key_cycle,
        Some(nonces),
        make_task,
        Some(send_done),
        spinner,
    )
    .await;

    let mut report = super::sustained::build_sustained_report(
        result,
        src,
        dest,
        &format!("{}", targets.its_proxy_addr),
        sizing.total_expected,
        sizing.num_keys,
    );

    let (verification, timings) = verify_handle.await??;
    for (msg_id, timing) in timings {
        if let Some(tx) = report
            .transactions
            .iter_mut()
            .find(|t| t.signature == msg_id)
        {
            tx.amplifier_timing = Some(timing);
        }
    }
    report.verification = Some(verification);

    finish_report(args, &mut report, test_start)
}

/// Drive the burst-mode pipeline: fan out parallel ITS interchain transfers
/// (with retry on rate limits), batch-verify on the Solana destination, and
/// hand off to `finish_report`.
async fn run_burst_pipeline(
    args: &LoadTestArgs,
    evm_rpc_url: &str,
    derived: &[PrivateKeySigner],
    sizing: &RunSizing,
    targets: &TransferTargets,
) -> eyre::Result<()> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;
    let num_txs = sizing.num_txs;

    // --- Parallel interchainTransfer sends via ITS Service ---
    // Each derived key calls ITS.interchainTransfer(tokenId, destChain, destAddr, amount, metadata, gasValue)
    // The ITS Service handles hub wrapping (SEND_TO_HUB) and emits ContractCall to "axelar".
    let metrics_list: Arc<Mutex<Vec<TxMetrics>>> = Arc::new(Mutex::new(Vec::new()));
    let confirmed_counter = Arc::new(AtomicU64::new(0));
    let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_SENDS));
    let spinner = ui::wait_spinner(&format!("sending (0/{num_txs} confirmed)..."));
    let test_start = Instant::now();

    let mut tasks = Vec::with_capacity(num_txs);
    let dest_chain = dest.to_string();

    for derived_signer in derived {
        let metrics_clone = Arc::clone(&metrics_list);
        let counter = Arc::clone(&confirmed_counter);
        let sem = Arc::clone(&semaphore);
        let sp = spinner.clone();
        let total = num_txs;
        let dc = dest_chain.clone();
        let gv = targets.gas_value;
        let gsf = targets.gas_arg_scaling_factor;
        let rb = targets.receiver_bytes.clone();
        let amt = sizing.amount_per_tx;
        let its_proxy = targets.its_proxy_addr;
        let tid = targets.token_id;

        let provider = ProviderBuilder::new()
            .wallet(derived_signer.clone())
            .connect_http(evm_rpc_url.parse()?);

        let handle = tokio::spawn(async move {
            let _permit = sem.acquire().await.unwrap();

            let mut m = None;
            for attempt in 0..=MAX_RETRIES {
                let result = execute_interchain_transfer(
                    &provider, its_proxy, tid, &dc, &rb, amt, gv, gsf, None,
                )
                .await;

                if result.success || attempt == MAX_RETRIES {
                    m = Some(result);
                    break;
                }

                let is_rate_limited = result.error.as_deref().is_some_and(|e| e.contains("429"));
                if !is_rate_limited {
                    m = Some(result);
                    break;
                }

                let backoff = Duration::from_secs(1 << attempt);
                tokio::time::sleep(backoff).await;
            }

            let m = m.unwrap();
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
    let test_duration = test_start.elapsed().as_secs_f64();

    let confirmed_count = confirmed_counter.load(Ordering::Relaxed);
    spinner.finish_and_clear();
    ui::success(&format!(
        "sent {confirmed_count}/{total_submitted} confirmed"
    ));

    let metrics = metrics_list.lock().await.clone();
    let total_failed = metrics.iter().filter(|m| !m.success).count() as u64;

    if total_failed > 0 {
        let mut error_counts: std::collections::HashMap<String, u64> =
            std::collections::HashMap::new();
        for m in metrics.iter().filter(|m| !m.success) {
            let reason = m
                .error
                .as_deref()
                .unwrap_or("unknown")
                .chars()
                .take(120)
                .collect::<String>();
            *error_counts.entry(reason).or_default() += 1;
        }
        for (reason, count) in &error_counts {
            ui::warn(&format!("{count} txs failed: {reason}"));
        }
    }

    let mut report = LoadTestReport::from_transactions(
        ReportInput {
            source_chain: src.to_string(),
            destination_chain: dest.to_string(),
            destination_address: format!("{}", targets.its_proxy_addr),
            num_txs: args.num_txs,
            num_keys: num_txs,
            total_submitted,
            test_duration_secs: test_duration,
            compute_unit_summary: ComputeUnitSummary::Omit,
        },
        metrics,
    );

    // --- Verify ---
    let verification = super::verify::verify_onchain_solana_its(
        &args.config,
        &args.source_axelar_id,
        &args.destination_axelar_id,
        &format!("{}", targets.its_proxy_addr),
        &args.destination_rpc,
        args.network,
        &mut report.transactions,
    )
    .await?;
    report.verification = Some(verification);

    finish_report(args, &mut report, test_start)
}
