//! Stellar to Sui ITS route.
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
use super::metrics::{ComputeUnitSummary, LoadTestReport, ReportInput, RunIdentity};
use super::run_sizing::RunSizing;
use super::{
    LoadTestArgs, finalize_sui_dest_run_its, load_stellar_main_wallet, load_sui_main_wallet,
    read_stellar_contract_address, read_stellar_network_type, read_stellar_token_address,
    read_sui_axe_token_id, sui_its_dest_lookup,
};
use crate::config::ChainContract;
use crate::stellar::{StellarClient, StellarWallet};
use crate::ui;

/// Per-tx transfer amount in token sub-units. Matches its_stellar_to_evm.
const AMOUNT_PER_TX: u64 = 10_000_000;
/// Default cross-chain gas in stroops (10 XLM). Same default as the EVM and
/// Solana destination variants.
const DEFAULT_GAS_STROOPS: u64 = 100_000_000;

fn parse_gas_stroops(value: Option<&str>) -> Result<super::units::Stroops> {
    let gas = super::units::Stroops::new(match value {
        Some(value) => value
            .parse()
            .map_err(|error| eyre!("invalid --gas-value: {error}"))?,
        None => DEFAULT_GAS_STROOPS,
    });
    let gas_xlm = gas.get() as f64 / 10_000_000.0;
    ui::kv("gas", &format!("{} stroops ({gas_xlm:.4} XLM)", gas.get()));
    Ok(gas)
}

struct StellarSource {
    client: StellarClient,
    wallet: StellarWallet,
    its_address: String,
    gateway_address: String,
    gas_token: String,
}

async fn prepare_stellar_source(args: &LoadTestArgs, source_chain: &str) -> Result<StellarSource> {
    let network_type = read_stellar_network_type(&args.config, source_chain).await?;
    let client = StellarClient::new(&args.source_rpc, &network_type)?;
    let its_address = read_stellar_contract_address(
        &args.config,
        source_chain,
        ChainContract::InterchainTokenService,
    )
    .await?;
    let gateway_address =
        read_stellar_contract_address(&args.config, source_chain, ChainContract::AxelarGateway)
            .await?;
    let gas_token = read_stellar_token_address(&args.config, source_chain).await?;
    let wallet = load_stellar_main_wallet(args.private_key.as_deref())?;

    ui::address("Stellar ITS", &its_address);
    ui::address("Stellar AxelarGateway", &gateway_address);
    ui::address("Stellar XLM (gas)", &gas_token);
    ui::kv("Stellar wallet", &wallet.address());

    if client.account_sequence(&wallet.address()).await?.is_none() {
        eyre::bail!(
            "Stellar main wallet {} is not activated. Fund it manually first.",
            wallet.address()
        );
    }

    Ok(StellarSource {
        client,
        wallet,
        its_address,
        gateway_address,
        gas_token,
    })
}

pub async fn run(args: LoadTestArgs, sizing: RunSizing) -> Result<()> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;

    ui::kv("source", src);
    ui::kv("destination", dest);
    ui::kv(
        "protocol",
        "ITS (interchainTransfer via hub, Sui destination)",
    );

    let sustained_params = sizing
        .sustained()
        .map(|plan| (plan.tps as u64, plan.duration_secs));

    let StellarSource {
        client: stellar_client,
        wallet: main_wallet,
        its_address: its_addr,
        gateway_address: gateway_addr,
        gas_token: xlm_addr,
    } = prepare_stellar_source(&args, src).await?;

    // ----- Sui tokenId + Stellar-side linked token -----
    let token_id = read_sui_axe_token_id(&args.config, dest, args.token_id.as_deref()).await?;
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
        sui_its_dest_lookup(&args.config, dest, Some(&args.destination_rpc)).await?;
    ui::address("Sui ITS channel (destination)", &sui_its_channel);

    // ----- Gas value -----
    let gas_stroops = parse_gas_stroops(args.gas_value.as_deref())?;

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
            token_id: token_id.into(),
            destination_chain: args.destination_axelar_id.to_string(),
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
            run: RunIdentity::from_sizing(&args, sizing),
            destination_address: sui_wallet.address_hex(),
            num_txs: sizing.total_expected,
            num_keys: total_to_send as usize,
            total_submitted: send.total_submitted,
            test_duration_secs: send.test_duration_secs,
            compute_unit_summary: ComputeUnitSummary::Omit,
        },
        send.metrics,
    );
    finalize_sui_dest_run_its(&args, &mut report, &sui_rpc, test_start).await
}
