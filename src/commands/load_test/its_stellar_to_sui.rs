//! Stellar -> Sui ITS load test.
//!
//! Pre-conditions handled outside axe (one-time per network):
//!   1. A Sui-side AXE coin is registered on Sui ITS, tokenId stored in
//!      `chains.sui.contracts.AXE.objects.TokenId`.
//!   2. The same tokenId is linked on the Stellar ITS via the
//!      `axelar-contract-deployments` link-token flow. After link, the
//!      Stellar token contract is queryable via
//!      `InterchainTokenService.interchain_token_address(tokenId)` and the
//!      main wallet holds a balance.
//!
//! Burst mode (`--num-txs N`) fires N txs from the main wallet sequentially.
//!
//! Sustained mode (`--tps T --duration-secs D`) sequences the same main
//! wallet at the requested rate. Because Stellar sequence numbers serialise
//! transactions per account, single-wallet sustained is bounded by the
//! chain's confirmation cadence (~5s per ledger close on mainnet) — request
//! `T * D` total but effective TPS will be `min(T, 1/confirm_time)`. For
//! higher source-side throughput, the same derived-wallet pattern from
//! `its_stellar_to_evm.rs` would need to land here (create_account per
//! derived key + linked-token distribution); not yet implemented.

use std::sync::Arc;
use std::time::{Duration, Instant};

use eyre::{Result, eyre};

use super::its_stellar_source::{ItsStellarSubmitJob, ItsStellarSubmitter};
use super::metrics::{ComputeUnitSummary, LoadTestReport, ReportInput};
use super::run_sizing::RunSizing;
use super::{
    LoadTestArgs, finalize_sui_dest_run_its, load_stellar_main_wallet, load_sui_main_wallet,
    read_stellar_contract_address, read_stellar_network_type, read_stellar_token_address,
    read_sui_axe_token_id, sui_its_dest_lookup,
};
use crate::stellar::StellarClient;
use crate::ui;

/// Per-tx transfer amount in token sub-units. Matches its_stellar_to_evm.
const AMOUNT_PER_TX: u64 = 10_000_000;
/// Default cross-chain gas in stroops (10 XLM). Same default as the EVM and
/// Solana destination variants.
const DEFAULT_GAS_STROOPS: u64 = 100_000_000;

pub async fn run(args: LoadTestArgs, _run_start: Instant) -> Result<()> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;

    ui::kv("source", src);
    ui::kv("destination", dest);
    ui::kv(
        "protocol",
        "ITS (interchainTransfer via hub, Sui destination)",
    );

    let sizing = RunSizing::new(&args)?;
    let sustained_params = sizing
        .sustained()
        .map(|(tps, duration_secs, _)| (tps as u64, duration_secs));

    // ----- Stellar source setup -----
    let stellar_rpc = &args.source_rpc;
    let network_type = read_stellar_network_type(&args.config, src)?;
    let stellar_client = StellarClient::new(stellar_rpc, &network_type)?;
    let its_addr = read_stellar_contract_address(&args.config, src, "InterchainTokenService")?;
    let gateway_addr = read_stellar_contract_address(&args.config, src, "AxelarGateway")?;
    let xlm_addr = read_stellar_token_address(&args.config, src)?;
    ui::address("Stellar ITS", &its_addr);
    ui::address("Stellar AxelarGateway", &gateway_addr);
    ui::address("Stellar XLM (gas)", &xlm_addr);

    let main_wallet = load_stellar_main_wallet(args.private_key.as_deref())?;
    ui::kv("Stellar wallet", &main_wallet.address());
    if stellar_client
        .account_sequence(&main_wallet.address())
        .await?
        .is_none()
    {
        eyre::bail!(
            "Stellar main wallet {} is not activated — fund it manually first.",
            main_wallet.address()
        );
    }

    // ----- Sui tokenId + Stellar-side linked token -----
    let token_id = read_sui_axe_token_id(&args.config, dest, args.token_id.as_deref())?;
    ui::kv("Sui token id", &format!("0x{}", hex::encode(token_id)));

    let linked_token_addr = stellar_client
        .its_query_token_address(&main_wallet, &its_addr, token_id)
        .await
        .map_err(|e| eyre!("ITS.interchain_token_address query failed: {e}"))?
        .ok_or_else(|| {
            eyre!(
                "Stellar ITS at {its_addr} has no token linked to Sui AXE tokenId 0x{}. Run the \
                 one-time off-axe link-token step from axelar-contract-deployments, then ensure \
                 the main wallet {} holds a balance of the linked token.",
                hex::encode(token_id),
                main_wallet.address(),
            )
        })?;
    ui::address("Stellar linked token", &linked_token_addr);

    // ----- Sui recipient + ITS channel id + RPC -----
    let sui_wallet = load_sui_main_wallet()?;
    let sui_recipient_bytes = sui_wallet.address.as_bytes().to_vec();
    ui::address("destination Sui address", &sui_wallet.address_hex());
    let (sui_its_channel, sui_rpc) =
        sui_its_dest_lookup(&args.config, dest, Some(&args.destination_rpc))?;
    ui::address("Sui ITS channel (destination)", &sui_its_channel);

    // ----- Gas value -----
    let gas_stroops: u64 = match &args.gas_value {
        Some(v) => v.parse().map_err(|e| eyre!("invalid --gas-value: {e}"))?,
        None => DEFAULT_GAS_STROOPS,
    };
    let gas_xlm = gas_stroops as f64 / 10_000_000.0;
    ui::kv("gas", &format!("{gas_stroops} stroops ({gas_xlm:.4} XLM)"));

    // ----- Send loop: burst (sequential N) or sustained (rate-paced) -----
    let total_to_send = sizing.total_expected;
    let pacing: Option<Duration> = sustained_params.map(|(tps, _)| {
        // Per-tx interval = 1s / tps. Stellar's single-wallet throughput is
        // bounded by ledger close (~5s), so requesting tps>0.2 just queues
        // back-to-back — we still call interval.tick() to keep the loop
        // structure consistent with the EVM/Sol variants.
        Duration::from_millis(1_000 / tps.max(1))
    });
    let capacity = usize::try_from(total_to_send).unwrap_or(0);
    let wallet = Arc::new(main_wallet);
    let jobs = vec![
        ItsStellarSubmitJob {
            wallet: Arc::clone(&wallet),
        };
        capacity
    ];
    let test_start = Instant::now();
    let send = super::submitter::run_serial(
        ItsStellarSubmitter {
            client: stellar_client,
            its_contract: its_addr,
            gateway_contract: gateway_addr,
            token_id,
            destination_chain: args.destination_axelar_id.clone(),
            destination_address_bytes: sui_recipient_bytes,
            gas_token: xlm_addr,
            gas_stroops,
            amount_per_tx: u128::from(AMOUNT_PER_TX),
            axelarnet_gw_addr: String::new(),
        },
        jobs,
        pacing,
    )
    .await?;
    let mut report = LoadTestReport::from_transactions(
        ReportInput {
            source_chain: src.to_string(),
            destination_chain: dest.to_string(),
            destination_address: sui_wallet.address_hex(),
            num_txs: args.num_txs,
            num_keys: total_to_send as usize,
            total_submitted: send.total_submitted,
            test_duration_secs: send.test_duration_secs,
            compute_unit_summary: ComputeUnitSummary::Omit,
        },
        send.metrics,
    );
    if let Some((tps, duration_secs)) = sustained_params {
        report.tps = Some(tps);
        report.duration_secs = Some(duration_secs);
    }

    finalize_sui_dest_run_its(&args, &mut report, &sui_rpc, test_start).await
}
