//! Sui to Solana ITS route.
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

use crate::solana::load_keypair;
use std::time::Instant;

use eyre::Result;
use solana_sdk::signer::Signer;

use super::gmp_sui_source::{DEFAULT_GAS_BUDGET, DEFAULT_GAS_VALUE};
use super::its_sui_source::{
    ItsSuiSubmitter, PreparedSuiIts, ensure_gas_balance, parse_hub_gas, prepare_source,
    run_its_sequential,
};
use super::metrics::{ComputeUnitSummary, LoadTestReport, ReportInput, RunIdentity};
use super::run_sizing::RunSizing;
use super::{LoadTestArgs, validate_solana_rpc};
use crate::config::AxelarChainContract;
use crate::config::AxelarGlobalContract;
use crate::config::ChainsConfig;
use crate::ui;

const AMOUNT_PER_TX: u64 = 1;

pub async fn run(args: LoadTestArgs, sizing: RunSizing) -> Result<()> {
    let num_txs = usize::try_from(sizing.require_burst("ITS sui-to-sol")?)?;
    let src = &args.source_chain;
    let dest = &args.destination_chain;
    let cfg = ChainsConfig::load(&args.config).await?;

    let sol_rpc_url = args.destination_rpc.to_string();
    validate_solana_rpc(&sol_rpc_url).await?;

    if cfg
        .axelar
        .contract_address(AxelarChainContract::Gateway, dest)
        .is_err()
    {
        eyre::bail!(
            "destination chain '{dest}' has no Cosmos Gateway in the config — verification would fail."
        );
    }
    if cfg
        .axelar
        .global_contract_address(AxelarGlobalContract::AxelarnetGateway)
        .is_err()
    {
        eyre::bail!("no AxelarnetGateway address in config — required for ITS load test");
    }

    ui::kv("source", src);
    ui::kv("destination", dest);
    ui::kv("protocol", "ITS (interchainTransfer via hub)");

    let PreparedSuiIts {
        client: sui_client,
        wallet: main_wallet,
        contracts: its_contracts,
        coin_type,
        token_id,
        balance,
    } = prepare_source(&args).await?;

    // --- Solana destination address (recipient pubkey) ---
    // The destination_address bytes in the ITS message are the recipient on
    // Solana. We use the deployer's own Solana pubkey (from the keypair
    // axe loads); the destination ITS will route mint into this account's
    // ATA. The Solana ITS creates the ATA on demand if it doesn't exist yet.
    let sol_keypair = load_keypair(args.keypair.as_deref()).await?;
    let sol_pubkey = sol_keypair.pubkey();
    let dest_address_bytes: Vec<u8> = sol_pubkey.to_bytes().to_vec();
    ui::address("destination Solana account", &sol_pubkey.to_string());

    // --- Gas (mist) ---
    let gas_value = parse_hub_gas(args.gas_value.as_deref(), DEFAULT_GAS_VALUE)?;
    ensure_gas_balance(balance, gas_value, DEFAULT_GAS_BUDGET)?;

    // The ITS-via-hub destination on Axelar is the ITS-hub CosmWasm contract,
    // NOT AxelarnetGateway. The Amplifier voting verifier matches
    // `messages_status` against the exact destination_address recorded in the
    // source-side ContractCall, so anything else (AxelarnetGateway, etc.)
    // makes the vote lookup miss even when the message executes end-to-end.
    let its_hub_addr = cfg
        .axelar
        .global_contract_address(AxelarGlobalContract::InterchainTokenService)?
        .to_string();

    // --- Sequential burst through the shared source capability ---
    let test_start = Instant::now();
    let burst = run_its_sequential(
        ItsSuiSubmitter {
            client: sui_client,
            wallet: main_wallet,
            contracts: its_contracts,
            coin_type,
            token_id,
            destination_chain: args.destination_axelar_id.to_string(),
            destination_address_bytes: dest_address_bytes,
            transfer_amount: AMOUNT_PER_TX,
            gas_value,
            gas_budget: DEFAULT_GAS_BUDGET,
            its_hub_address: its_hub_addr,
        },
        num_txs,
    )
    .await?;

    let destination_address = sol_pubkey.to_string();

    let mut report = LoadTestReport::from_transactions(
        ReportInput {
            run: RunIdentity::burst(&args),
            destination_address: destination_address.clone(),
            num_txs: burst.total_submitted,
            num_keys: 1,
            total_submitted: burst.total_submitted,
            test_duration_secs: burst.test_duration_secs,
            compute_unit_summary: ComputeUnitSummary::Omit,
        },
        burst.metrics,
    );

    super::its_verification::finish_batch(
        &args,
        super::its_verification::SolanaItsTarget {
            rpc_url: sol_rpc_url,
        },
        &mut report,
        test_start,
    )
    .await
}
