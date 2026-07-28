//! Sui -> Solana ITS load test (source-side).
//!
//! Mirrors `its_sui_to_evm.rs`: the Sui PTB (`example::its::send_interchain_transfer_call<T>`)
//! is identical — only the destination differs. Same `--token-id` / `--coin-type`
//! resolution path (defaults from `chains.<sui>.contracts.AXE`), same gas
//! handling. The destination side swaps EVM-gateway polling for
//! `verify_onchain_solana_its`, which already handles SVM ITS receives from
//! any source.
//!
//! Prerequisites (one-time, off-axe):
//!   1. `chains.sui.contracts.AXE` populated (run
//!      `node sui/its.js register-coin-from-info AXE AXE 9 -e testnet -n sui`).
//!   2. `solana` MUST appear in Sui ITS's `trusted_chains` — added via
//!      `node sui/its.js add-trusted-chains solana`, which is admin-gated and
//!      needs the Sui ITS owner-cap holder to run it. Without this, Sui-side
//!      `prepare_hub_message` aborts before the message ever leaves Sui.
//!   3. `node sui/its.js deploy-remote-coin <tokenId> solana` to publish the
//!      linked SPL mint on Solana ITS (requires step 2 already done).
//!   4. Mint some AXE on Solana to the deployer's ATA so the destination
//!      side has supply for the inbound interchainTransfer to mint into.
//!
//! Until upstream (Axelar) flips the trust list to include `solana`, this
//! route bails at step 2. The code below is correct and will exercise
//! end-to-end as soon as that flips.

use std::time::Instant;

use eyre::{Result, eyre};

use super::gmp::{SUI_DEFAULT_GAS_BUDGET_MIST, SUI_DEFAULT_GAS_VALUE_MIST};
use super::metrics::{ComputeUnitSummary, LoadTestReport, ReportInput, TxMetrics};
use super::{
    LoadTestArgs, finish_report, load_sui_main_wallet, resolve_sui_axe_token, validate_solana_rpc,
};
use crate::config::ChainsConfig;
use crate::ui;

const AMOUNT_PER_TX: u64 = 1;

pub async fn run(args: LoadTestArgs, _run_start: Instant) -> Result<()> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;
    let cfg = ChainsConfig::load(&args.config)?;

    let sol_rpc_url = args.destination_rpc.clone();
    validate_solana_rpc(&sol_rpc_url).await?;

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
    let sui_amount = bal as f64 / 1e9;
    ui::kv("Sui balance", &format!("{bal} mist ({sui_amount:.4} SUI)"));

    // --- Token id + coin type ---
    // Same resolver as Sui→EVM: defaults to `chains.<sui>.contracts.AXE`,
    // CLI flags override.
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

    let (_, coin_balance) = sui_client
        .pick_coin_of_type(&main_wallet.address, &coin_type)
        .await?;
    ui::kv("Coin<T> balance", &coin_balance.to_string());

    // --- Solana destination address (recipient pubkey) ---
    // The destination_address bytes in the ITS message are the recipient on
    // Solana. We use the deployer's own Solana pubkey (from the keypair
    // axe loads); the destination ITS will route mint into this account's
    // ATA. The Solana ITS creates the ATA on demand if it doesn't exist yet.
    let sol_keypair = crate::solana::load_keypair(args.keypair.as_deref())?;
    use solana_sdk::signer::Signer;
    let sol_pubkey = sol_keypair.pubkey();
    let dest_address_bytes: Vec<u8> = sol_pubkey.to_bytes().to_vec();
    ui::address("destination Solana account", &sol_pubkey.to_string());

    // --- Gas (mist) ---
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

    // The ITS-via-hub destination on Axelar is the ITS-hub CosmWasm contract,
    // NOT AxelarnetGateway. The Amplifier voting verifier matches
    // `messages_status` against the exact destination_address recorded in the
    // source-side ContractCall, so anything else (AxelarnetGateway, etc.)
    // makes the vote lookup miss even when the message executes end-to-end.
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
        let result =
            crate::sui::send_its_interchain_transfer(crate::sui::InterchainTransferRequest {
                client: &sui_client,
                wallet: &main_wallet,
                contracts: &its_contracts,
                coin_type_tag: &coin_type,
                token_id,
                destination_chain: &args.destination_axelar_id,
                destination_address_bytes: &dest_address_bytes,
                transfer_amount: AMOUNT_PER_TX,
                gas_value_mist,
                gas_budget_mist: SUI_DEFAULT_GAS_BUDGET_MIST,
            })
            .await;

        match result {
            Ok(r) if r.success => {
                let latency_ms = send_start
                    .elapsed()
                    .as_millis()
                    .try_into()
                    .unwrap_or(u64::MAX);
                let message_id = format!("{}-{}", r.digest, r.event_index);
                metrics.push(TxMetrics {
                    signature: message_id,
                    submit_time_ms: latency_ms,
                    confirm_time_ms: Some(latency_ms),
                    latency_ms: Some(latency_ms),
                    compute_units: None,
                    slot: None,
                    outcome: TxMetrics::succeeded_outcome(),
                    payload: Vec::new(),
                    payload_hash: r.payload_hash_hex.clone(),
                    source_address: format!("0x{}", r.source_address_hex),
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
                    outcome: TxMetrics::external_outcome(false, r.error, "Sui ITS tx failed"),
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
                    outcome: TxMetrics::failed_outcome(e.to_string()),
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
    let total_confirmed = metrics.iter().filter(|m| m.is_success()).count() as u64;
    ui::success(&format!(
        "sent {total_confirmed}/{total_submitted} confirmed"
    ));

    let test_duration = test_start.elapsed().as_secs_f64();

    let destination_address = sol_pubkey.to_string();

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

    let verification =
        super::verify::verify_onchain_solana_its(super::verify::ItsBatchVerification {
            route: super::verify::VerificationRoute {
                config: &args.config,
                source_chain: &args.source_axelar_id,
                destination_chain: &args.destination_axelar_id,
                network: args.network,
            },
            destination: super::verify::SolanaItsDestination {
                rpc_url: &sol_rpc_url,
            },
            metrics: &mut report.transactions,
        })
        .await?;
    report.verification = Some(verification);

    finish_report(&args, &mut report, test_start)
}
