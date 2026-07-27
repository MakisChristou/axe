//! Sui -> EVM ITS load test (source-side).
//!
//! Unlike Stellar/Solana ITS source runners, we don't auto-deploy a fresh
//! AXE token on Sui — publishing a Move package requires the Sui CLI's
//! build pipeline (Move source -> bytecode -> `sui_publishPackage`), which
//! is not feasible to do from Rust without bundling the toolchain. Instead,
//! the runner takes:
//!
//!   * `--token-id`  — 32B hex, an already-registered ITS token id on Sui.
//!   * `--coin-type` — optional Move type tag string (e.g.
//!     `0x96b4…::token::TOKEN`). If omitted, we resolve it via
//!     `interchain_token_service::registered_coin_type` dev-inspect.
//!
//! Pre-deploy / register the AXE token using
//! `axelar-contract-deployments/sui/its.js register-coin-from-info` (or the
//! sibling `register-custom-coin` flow), then pass the resulting token id
//! into the runner.
//!
//! The PTB calls `example::its::send_interchain_transfer_call<T>` per tx,
//! which is the user-friendly wrapper that bundles
//! `prepare_interchain_transfer` + `send_interchain_transfer` + `pay_gas` +
//! `send_message` into a single Move call.

use std::time::Instant;

use eyre::{Result, eyre};

use super::gmp::{SUI_DEFAULT_GAS_BUDGET_MIST, SUI_DEFAULT_GAS_VALUE_MIST};
use super::metrics::{ComputeUnitSummary, LoadTestReport, ReportInput, TxMetrics};
use super::{
    LoadTestArgs, finish_report, load_sui_main_wallet, resolve_sui_axe_token, validate_evm_rpc,
};
use crate::config::ChainsConfig;
use crate::ui;

const AMOUNT_PER_TX: u64 = 1; // ITS amounts are in token sub-units; 1 is fine for a load test.

pub async fn run(args: LoadTestArgs, _run_start: Instant) -> Result<()> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;
    let cfg = ChainsConfig::load(&args.config)?;

    let evm_rpc_url = args.destination_rpc.clone();
    validate_evm_rpc(&evm_rpc_url).await?;

    // A legacy (consensus) destination has no Cosmos Gateway and is verified on
    // its on-chain gateway instead; only amplifier dests need the Cosmos Gateway.
    let dest_amplifier = cfg.axelar.contract_address("VotingVerifier", dest).is_ok();
    if dest_amplifier && cfg.axelar.contract_address("Gateway", dest).is_err() {
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

    ui::kv("source", src);
    ui::kv("destination", dest);
    ui::kv("protocol", "ITS (interchainTransfer via hub)");

    // --- Sui config + main wallet ---
    let (sui_rpc_default, _gmp_contracts) = crate::sui::read_sui_chain_config(&args.config, src)?;
    let sui_rpc = if args.source_rpc.is_empty() {
        sui_rpc_default
    } else {
        args.source_rpc.clone()
    };
    let sui_client = crate::sui::SuiClient::new(&sui_rpc);

    let its_contracts = crate::sui::read_sui_its_config(&args.config, src)?;
    ui::address(
        "Example::its::Singleton",
        &format!("0x{}", hex::encode(its_contracts.its_singleton.as_bytes())),
    );
    ui::address(
        "InterchainTokenService",
        &format!("0x{}", hex::encode(its_contracts.its_object.as_bytes())),
    );

    let main_wallet = load_sui_main_wallet()?;
    ui::kv("Sui wallet", &main_wallet.address_hex());
    let bal = sui_client.get_balance(&main_wallet.address).await?;
    #[allow(clippy::cast_precision_loss, clippy::float_arithmetic)]
    let sui_amount = bal as f64 / 1e9;
    ui::kv("Sui balance", &format!("{bal} mist ({sui_amount:.4} SUI)"));

    // --- Token id + coin type ---
    // Defaults seamlessly from `chains.sui.contracts.AXE` in the chain config
    // (populated by `axelar-contract-deployments/sui/its.js
    // register-coin-from-info AXE AXE 9`). CLI flags override.
    let (token_id, coin_type) = resolve_sui_axe_token(
        &args.config,
        src,
        args.token_id.as_deref(),
        args.coin_type.as_deref(),
    )?;
    let coin_type = match coin_type {
        Some(t) => t,
        None => {
            ui::info("resolving Sui coin type via dev-inspect...");
            sui_client
                .dev_inspect_registered_coin_type(
                    &main_wallet.address,
                    its_contracts.its_pkg,
                    its_contracts.its_object,
                    token_id,
                )
                .await?
        }
    };
    ui::kv("token id", &format!("0x{}", hex::encode(token_id)));
    ui::kv("coin type", &coin_type);

    // Sanity-check the wallet has at least one Coin<T> object before we
    // start submitting (so we fail fast with a clearer error).
    let (_, coin_balance) = sui_client
        .pick_coin_of_type(&main_wallet.address, &coin_type)
        .await?;
    ui::kv("Coin<T> balance", &coin_balance.to_string());

    // --- EVM ITS proxy + gateway (for verification + dest_address) ---
    let dest_cfg = cfg
        .chains
        .get(dest)
        .ok_or_else(|| eyre!("destination chain '{dest}' not found in config"))?;
    let evm_its_addr: alloy::primitives::Address = dest_cfg
        .contract_address("InterchainTokenService", dest)?
        .parse()?;
    let evm_gateway_addr: alloy::primitives::Address =
        dest_cfg.contract_address("AxelarGateway", dest)?.parse()?;
    ui::address("destination ITS", &format!("{evm_its_addr}"));
    ui::address("EVM gateway", &format!("{evm_gateway_addr}"));
    let dest_address_bytes = evm_its_addr.as_slice().to_vec();
    let destination_address = format!("{evm_its_addr}");

    // --- Gas (mist) ---
    // ITS routes via the hub (two commands: source→hub, hub→destination),
    // so we pay 2× the per-command gas value.
    let gas_value_mist: u64 = match &args.gas_value {
        Some(v) => v
            .parse::<u64>()
            .map_err(|e| eyre!("invalid --gas-value: {e}"))?,
        None => SUI_DEFAULT_GAS_VALUE_MIST,
    }
    .saturating_mul(2);
    ui::kv(
        "cross-chain gas",
        &format!("{gas_value_mist} mist (paid via Sui GasService)"),
    );
    if bal < gas_value_mist + SUI_DEFAULT_GAS_BUDGET_MIST {
        eyre::bail!(
            "Sui wallet has insufficient SUI: {bal} mist; need ≥ {} mist (gas budget + cross-chain gas).",
            gas_value_mist + SUI_DEFAULT_GAS_BUDGET_MIST
        );
    }

    // --- Cosmos hub address (for `gmp_destination_*` book-keeping) ---
    // The ITS-via-hub destination on Axelar is the ITS-hub CosmWasm contract,
    // NOT AxelarnetGateway. The Amplifier voting verifier matches
    // `messages_status` against the exact destination_address recorded in the
    // source-side ContractCall, so anything else makes vote lookup miss.
    let its_hub_addr = cfg
        .axelar
        .global_contract_address("InterchainTokenService")?
        .to_string();

    // --- Sequential burst loop ---
    let num_txs = args.num_txs.max(1) as usize;
    let test_start = Instant::now();
    let spinner = ui::wait_spinner(&format!("sending (0/{num_txs} confirmed)..."));
    let mut metrics: Vec<TxMetrics> = Vec::with_capacity(num_txs);

    for i in 0..num_txs {
        let send_start = Instant::now();
        let result = crate::sui::send_its_interchain_transfer(
            &sui_client,
            &main_wallet,
            &its_contracts,
            &coin_type,
            token_id,
            &args.destination_axelar_id,
            &dest_address_bytes,
            AMOUNT_PER_TX,
            gas_value_mist,
            SUI_DEFAULT_GAS_BUDGET_MIST,
        )
        .await;

        match result {
            Ok(r) if r.success => {
                #[allow(clippy::cast_possible_truncation)]
                let latency_ms = send_start.elapsed().as_millis() as u64;
                let message_id = format!("{}-{}", r.digest, r.event_index);
                metrics.push(TxMetrics {
                    signature: message_id,
                    submit_time_ms: latency_ms,
                    confirm_time_ms: Some(latency_ms),
                    latency_ms: Some(latency_ms),
                    compute_units: None,
                    slot: None,
                    success: true,
                    error: None,
                    payload: Vec::new(),
                    payload_hash: r.payload_hash_hex.clone(),
                    source_address: format!("0x{}", r.source_address_hex),
                    // ITS routes via the Cosmos hub on its first leg, so
                    // book the second-leg destination as `axelar` so the
                    // verifier can pick up the hub-forwarded message id.
                    gmp_destination_chain: "axelar".to_string(),
                    gmp_destination_address: its_hub_addr.clone(),
                    send_instant: Some(send_start),
                    amplifier_timing: None,
                });
                spinner.set_message(format!("sending ({}/{num_txs} confirmed)...", i + 1));
            }
            Ok(r) => {
                metrics.push(TxMetrics {
                    signature: String::new(),
                    submit_time_ms: 0,
                    confirm_time_ms: None,
                    latency_ms: None,
                    compute_units: None,
                    slot: None,
                    success: false,
                    error: r.error.or_else(|| Some("Sui ITS tx failed".to_string())),
                    payload: Vec::new(),
                    payload_hash: String::new(),
                    source_address: String::new(),
                    gmp_destination_chain: String::new(),
                    gmp_destination_address: String::new(),
                    send_instant: None,
                    amplifier_timing: None,
                });
            }
            Err(e) => {
                metrics.push(TxMetrics {
                    signature: String::new(),
                    submit_time_ms: 0,
                    confirm_time_ms: None,
                    latency_ms: None,
                    compute_units: None,
                    slot: None,
                    success: false,
                    error: Some(e.to_string()),
                    payload: Vec::new(),
                    payload_hash: String::new(),
                    source_address: String::new(),
                    gmp_destination_chain: String::new(),
                    gmp_destination_address: String::new(),
                    send_instant: None,
                    amplifier_timing: None,
                });
            }
        }
    }

    spinner.finish_and_clear();
    let total_submitted = metrics.len() as u64;
    let total_confirmed = metrics.iter().filter(|m| m.success).count() as u64;
    ui::success(&format!(
        "sent {total_confirmed}/{total_submitted} confirmed"
    ));

    let test_duration = test_start.elapsed().as_secs_f64();
    let mut report = LoadTestReport::from_transactions(
        ReportInput {
            source_chain: src.to_string(),
            destination_chain: dest.to_string(),
            destination_address: destination_address.clone(),
            num_txs: total_submitted,
            num_keys: 1,
            total_submitted,
            test_duration_secs: test_duration,
            compute_unit_summary: ComputeUnitSummary::Omit,
        },
        metrics,
    );

    let verification = super::verify::verify_onchain_evm_its(
        &args.config,
        &args.source_axelar_id,
        &args.destination_axelar_id,
        args.network,
        &destination_address,
        evm_gateway_addr,
        &evm_rpc_url,
        &mut report.transactions,
    )
    .await?;
    report.verification = Some(verification);

    finish_report(&args, &mut report, test_start)
}
