//! Solana -> Sui ITS load test.
//!
//! Pre-conditions handled outside axe (one-time per network):
//!   1. A Sui-side AXE coin is registered on Sui ITS, tokenId stored in
//!      `chains.sui.contracts.AXE.objects.TokenId`.
//!   2. The same tokenId is linked on the Solana ITS program via the
//!      `axelar-contract-deployments` link-token flow. After link, the
//!      Solana mint at `find_interchain_token_pda(its_root, tokenId)` is
//!      initialised and the source signer holds a balance via the associated
//!      token account.
//!
//! Burst mode (`--num-txs N`): one tx per slot, sequential from main signer.
//! Sustained mode (`--tps T --duration-secs D`): fires `T` parallel sends
//! per second from the main signer for `D` seconds. Solana txs are not
//! nonce-ordered, so parallel sends from the same keypair work — the
//! source ATA balance is the only shared resource, which we pre-check.

use std::str::FromStr;
use std::sync::Arc;
use std::time::Instant;

use eyre::eyre;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signer;

use super::its_sol_source;
use super::metrics::{ComputeUnitSummary, LoadTestReport, ReportInput, RunIdentity, TxMetrics};
use super::run_sizing::{RunSizing, SustainedPlan};
use super::{
    LoadTestArgs, finalize_sui_dest_run_its, load_sui_main_wallet, read_sui_axe_token_id,
    sui_its_dest_lookup, validate_solana_rpc,
};
use crate::solana::{self, rpc_client};
use crate::ui;

const AMOUNT_PER_TX: u64 = 1;
const TOKEN_PROGRAM_2022: &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";

pub async fn run(args: LoadTestArgs, _run_start: Instant) -> eyre::Result<()> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;

    let sol_rpc = args.source_rpc.clone();
    validate_solana_rpc(&sol_rpc).await?;

    ui::kv("source", src);
    ui::kv("destination", dest);
    ui::kv(
        "protocol",
        "ITS (interchainTransfer via hub, Sui destination)",
    );

    // ----- Sizing -----
    let sizing = RunSizing::new(&args)?;
    let total_to_send = sizing.total_expected;

    // ----- Main keypair -----
    let main_keypair = solana::load_keypair(args.keypair.as_deref())?;
    let main_pubkey = main_keypair.pubkey();
    ui::kv("Solana wallet", &main_pubkey.to_string());

    // ----- Token id + mint (deterministic from tokenId) -----
    let token_id = read_sui_axe_token_id(&args.config, dest, args.token_id.as_deref())?;
    ui::kv("Sui token id", &format!("0x{}", hex::encode(token_id)));

    let (its_root, _) = solana::find_its_root_pda(args.network);
    let (mint, _) = solana::find_interchain_token_pda(args.network, &its_root, &token_id);
    ui::address("Solana mint (linked)", &mint.to_string());

    let rpc = rpc_client(&sol_rpc);
    let mint_acc = rpc
        .get_account_with_commitment(&mint, rpc.commitment())
        .map_err(|e| eyre!("rpc.get_account_with_commitment({mint}) failed: {e}"))?
        .value;
    if mint_acc.is_none() {
        eyre::bail!(
            "Solana ITS has no mint at {mint} for Sui AXE tokenId 0x{}. Run the one-time off-axe \
             link-token step from axelar-contract-deployments, then ensure the source signer \
             {main_pubkey} holds a balance via the associated token account.",
            hex::encode(token_id),
        );
    }

    // ----- Source ATA + balance -----
    let token_program = Pubkey::from_str(TOKEN_PROGRAM_2022)
        .map_err(|e| eyre!("token-2022 program id parse: {e}"))?;
    let source_ata = solana::get_associated_token_address(&main_pubkey, &mint, &token_program);
    let bal = rpc.get_token_account_balance(&source_ata).map_err(|e| {
        eyre!(
            "get_token_account_balance({source_ata}) failed — does the source signer hold any \
             linked AXE on Solana yet?: {e}"
        )
    })?;
    ui::kv("source ATA", &source_ata.to_string());
    ui::kv("source ATA balance", &bal.amount);

    let total_needed = u128::from(total_to_send) * u128::from(AMOUNT_PER_TX);
    let on_hand: u128 = bal.amount.parse().unwrap_or(0);
    if on_hand < total_needed {
        eyre::bail!(
            "source ATA {source_ata} holds {on_hand} AXE but the run plans to send {total_needed}. \
             Mint/transfer more to the source signer first."
        );
    }

    // ----- Sui recipient -----
    let sui_wallet = load_sui_main_wallet()?;
    let sui_recipient_bytes = sui_wallet.address.as_bytes().to_vec();
    ui::address("destination Sui address", &sui_wallet.address_hex());

    // ----- Sui ITS channel id + RPC -----
    let (sui_its_channel, sui_rpc) =
        sui_its_dest_lookup(&args.config, dest, Some(&args.destination_rpc))?;
    ui::address("Sui ITS channel (destination)", &sui_its_channel);

    // ----- Gas value (lamports) -----
    let gas_value: u64 = match &args.gas_value {
        Some(v) => v.parse().map_err(|e| eyre!("invalid --gas-value: {e}"))?,
        None => 10_000_000, // 0.01 SOL default
    };
    ui::kv("gas value", &format!("{gas_value} lamports"));

    // ----- Send loop -----
    let test_start = Instant::now();
    let dest_chain_id = args.destination_axelar_id.clone();
    let submitter = its_sol_source::ItsSolanaSubmitter {
        rpc_url: sol_rpc,
        network: args.network,
        token_id: token_id.into(),
        mint,
        destination_chain: dest_chain_id,
        destination_address: sui_recipient_bytes,
        amount: AMOUNT_PER_TX,
        gas_value: super::units::Lamports::new(gas_value),
        metric_context: its_sol_source::MetricContext::DestinationManaged,
    };
    let job = its_sol_source::ItsSolanaSubmitJob {
        keypair: Arc::new(main_keypair),
        source_account: source_ata,
    };

    let send = if let Some(SustainedPlan {
        tps,
        duration_secs,
        key_cycle,
    }) = sizing.sustained()
    {
        let spinner = ui::wait_spinner(&format!(
            "[0/{duration_secs}s] starting sustained ITS send..."
        ));
        let result = super::sustained::run_sustained_loop(
            SustainedPlan {
                tps,
                duration_secs,
                key_cycle,
            },
            None,
            its_sol_source::its_sustained_tasks(submitter, vec![job; sizing.num_keys], None),
            None,
            spinner,
        )
        .await?;
        SendResult {
            metrics: result.metrics,
            total_submitted: result.total_submitted,
            test_duration_secs: result.test_duration_secs,
        }
    } else {
        let num_txs = sizing.num_keys;
        let result = its_sol_source::run_its_burst(submitter, vec![job; num_txs], 1).await?;
        SendResult {
            metrics: result.metrics,
            total_submitted: result.total_submitted,
            test_duration_secs: result.test_duration_secs,
        }
    };

    let mut report = LoadTestReport::from_transactions(
        ReportInput {
            run: RunIdentity::from_args(&args),
            destination_address: sui_wallet.address_hex(),
            num_txs: args.num_txs,
            num_keys: send.total_submitted as usize,
            total_submitted: send.total_submitted,
            test_duration_secs: send.test_duration_secs,
            compute_unit_summary: ComputeUnitSummary::Omit,
        },
        send.metrics,
    );
    finalize_sui_dest_run_its(&args, &mut report, &sui_rpc, test_start).await
}

struct SendResult {
    metrics: Vec<TxMetrics>,
    total_submitted: u64,
    test_duration_secs: f64,
}
