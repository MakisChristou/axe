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
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use eyre::eyre;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};
use tokio::sync::Mutex;

use super::metrics::{ComputeUnitSummary, LoadTestReport, ReportInput, TxMetrics};
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
    let sustained_params = args.tps.zip(args.duration_secs);
    let total_to_send: u64 = match sustained_params {
        Some((tps, dur)) => tps * dur,
        None => args.num_txs.max(1),
    };

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
    let main_kp_secret: [u8; 32] = main_keypair.to_bytes()[..32]
        .try_into()
        .expect("Keypair::to_bytes() must produce 64 bytes (32 secret + 32 public)");

    let metrics = if let Some((tps, duration_secs)) = sustained_params {
        run_sustained(SustainedRequest {
            sol_rpc: sol_rpc.clone(),
            main_kp_secret,
            network: args.network,
            main_pubkey: main_pubkey.to_string(),
            token_id,
            source_ata,
            mint,
            dest_chain_id: dest_chain_id.clone(),
            sui_recipient_bytes: sui_recipient_bytes.clone(),
            gas_value,
            tps,
            duration_secs,
        })
        .await
    } else {
        run_burst(BurstRequest {
            sol_rpc: &sol_rpc,
            main_keypair: &main_keypair,
            network: args.network,
            main_pubkey: main_pubkey.to_string(),
            token_id,
            source_ata,
            mint,
            dest_chain_id: &dest_chain_id,
            sui_recipient_bytes: &sui_recipient_bytes,
            gas_value,
            num_txs: args.num_txs.max(1) as usize,
        })
        .await
    };

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
            destination_address: sui_wallet.address_hex(),
            num_txs: args.num_txs,
            num_keys: total_submitted as usize,
            total_submitted,
            test_duration_secs: test_duration,
            compute_unit_summary: ComputeUnitSummary::Omit,
        },
        metrics,
    );
    if let Some((tps, duration_secs)) = sustained_params {
        report.tps = Some(tps);
        report.duration_secs = Some(duration_secs);
    }

    finalize_sui_dest_run_its(&args, &mut report, &sui_rpc, test_start).await
}

struct BurstRequest<'a> {
    sol_rpc: &'a str,
    main_keypair: &'a Keypair,
    network: crate::types::Network,
    main_pubkey: String,
    token_id: [u8; 32],
    source_ata: Pubkey,
    mint: Pubkey,
    dest_chain_id: &'a str,
    sui_recipient_bytes: &'a [u8],
    gas_value: u64,
    num_txs: usize,
}

async fn run_burst(request: BurstRequest<'_>) -> Vec<TxMetrics> {
    let BurstRequest {
        sol_rpc,
        main_keypair,
        network,
        main_pubkey,
        token_id,
        source_ata,
        mint,
        dest_chain_id,
        sui_recipient_bytes,
        gas_value,
        num_txs,
    } = request;
    let mut metrics: Vec<TxMetrics> = Vec::with_capacity(num_txs);
    let spinner = ui::wait_spinner(&format!("sending (0/{num_txs} confirmed)..."));
    for _ in 0..num_txs {
        let result = solana::send_its_interchain_transfer(solana::InterchainTransferRequest {
            rpc_url: sol_rpc,
            keypair: main_keypair,
            network,
            token_id: &token_id,
            source_account: &source_ata,
            mint: &mint,
            destination_chain: dest_chain_id,
            destination_address: sui_recipient_bytes,
            amount: AMOUNT_PER_TX,
            gas_value,
        });
        match result {
            Ok((sig, mut m)) => {
                // The Amplifier voting verifier indexes Solana ITS messages by
                // `{sig}-{outer_ix}.{inner_ix}` where the inner index is the
                // exact CPI position of the gateway's call_contract — which
                // varies per tx (we observed `-1.7` in CI vs the static
                // `-2.1` shape this code used to assume). Parse it from the
                // confirmed tx logs (same helper its_sol_to_evm uses) so the
                // VotingVerifier `messages_status` query matches what
                // Axelar actually stored. Fall back to the synthetic
                // `{sig}-{call_contract_index}.1` if log parsing fails, so we
                // don't lose verification altogether on RPC hiccups.
                m.signature = solana::extract_its_message_id(sol_rpc, network, &sig)
                    .unwrap_or_else(|_| {
                        format!("{}-{}.1", sig, solana::solana_call_contract_index(network))
                    });
                metrics.push(m);
            }
            Err(e) => {
                metrics.push(failed_metric(main_pubkey.clone(), e.to_string()));
            }
        }
        let confirmed = metrics.iter().filter(|m| m.success).count();
        spinner.set_message(format!("sending ({confirmed}/{num_txs} confirmed)..."));
    }
    spinner.finish_and_clear();
    metrics
}

/// Sustained mode: every second for `duration_secs`, fan out `tps`
/// parallel `interchain_transfer` calls from the main keypair. Solana
/// txs are non-nonced, so parallel sends from one keypair work; the
/// source ATA's atomic balance is the only shared resource (pre-checked
/// to cover `tps * duration_secs` amount).
///
/// The underlying `send_its_interchain_transfer` call is sync (blocks
/// on confirmation). We wrap each invocation in `tokio::spawn_blocking`
/// so the tokio runtime doesn't stall on slow confirmations.
struct SustainedRequest {
    sol_rpc: String,
    main_kp_secret: [u8; 32],
    network: crate::types::Network,
    main_pubkey: String,
    token_id: [u8; 32],
    source_ata: Pubkey,
    mint: Pubkey,
    dest_chain_id: String,
    sui_recipient_bytes: Vec<u8>,
    gas_value: u64,
    tps: u64,
    duration_secs: u64,
}

async fn run_sustained(request: SustainedRequest) -> Vec<TxMetrics> {
    let SustainedRequest {
        sol_rpc,
        main_kp_secret,
        network,
        main_pubkey,
        token_id,
        source_ata,
        mint,
        dest_chain_id,
        sui_recipient_bytes,
        gas_value,
        tps,
        duration_secs,
    } = request;
    let total_expected = tps * duration_secs;
    let metrics_list: Arc<Mutex<Vec<TxMetrics>>> = Arc::new(Mutex::new(Vec::new()));
    let confirmed = Arc::new(AtomicU64::new(0));
    let failed = Arc::new(AtomicU64::new(0));
    let spinner = ui::wait_spinner(&format!(
        "[0/{duration_secs}s] sustained (0/{total_expected} confirmed)..."
    ));

    let mut interval = tokio::time::interval(Duration::from_secs(1));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let task_capacity = usize::try_from(total_expected).unwrap_or(0);
    let mut tasks: Vec<tokio::task::JoinHandle<()>> = Vec::with_capacity(task_capacity);

    for tick in 0..duration_secs {
        interval.tick().await;
        for _ in 0..tps {
            let rpc = sol_rpc.clone();
            let kp_secret = main_kp_secret;
            let src = main_pubkey.clone();
            let tid = token_id;
            let ata = source_ata;
            let m = mint;
            let dc = dest_chain_id.clone();
            let rb = sui_recipient_bytes.clone();
            let metrics_clone = Arc::clone(&metrics_list);
            let confirmed_ctr = Arc::clone(&confirmed);
            let failed_ctr = Arc::clone(&failed);
            let sp = spinner.clone();

            let rpc_for_extract = rpc.clone();
            let handle = tokio::spawn(async move {
                let result = tokio::task::spawn_blocking(move || {
                    let kp = Keypair::new_from_array(kp_secret);
                    solana::send_its_interchain_transfer(solana::InterchainTransferRequest {
                        rpc_url: &rpc,
                        keypair: &kp,
                        network,
                        token_id: &tid,
                        source_account: &ata,
                        mint: &m,
                        destination_chain: &dc,
                        destination_address: &rb,
                        amount: AMOUNT_PER_TX,
                        gas_value,
                    })
                })
                .await;

                let metric = match result {
                    Ok(Ok((sig, mut mm))) => {
                        // Same `{sig}-{outer}.{inner}` format the VotingVerifier
                        // indexes Solana ITS messages by — see burst path above
                        // for the full rationale.
                        mm.signature =
                            solana::extract_its_message_id(&rpc_for_extract, network, &sig)
                                .unwrap_or_else(|_| {
                                    format!(
                                        "{}-{}.1",
                                        sig,
                                        solana::solana_call_contract_index(network)
                                    )
                                });
                        confirmed_ctr.fetch_add(1, Ordering::Relaxed);
                        mm
                    }
                    Ok(Err(e)) => {
                        failed_ctr.fetch_add(1, Ordering::Relaxed);
                        failed_metric(src, e.to_string())
                    }
                    Err(join_err) => {
                        failed_ctr.fetch_add(1, Ordering::Relaxed);
                        failed_metric(src, format!("spawn_blocking join: {join_err}"))
                    }
                };
                metrics_clone.lock().await.push(metric);
                let c = confirmed_ctr.load(Ordering::Relaxed);
                let elapsed_s = tick + 1;
                sp.set_message(format!(
                    "[{elapsed_s}/{duration_secs}s] sustained ({c}/{total_expected} confirmed)..."
                ));
            });
            tasks.push(handle);
        }
    }

    // Wait for all in-flight tasks
    for h in tasks {
        let _ = h.await;
    }
    spinner.finish_and_clear();
    Arc::try_unwrap(metrics_list)
        .map(|m| m.into_inner())
        .unwrap_or_default()
}

fn failed_metric(src: String, err: String) -> TxMetrics {
    TxMetrics {
        signature: String::new(),
        submit_time_ms: 0,
        confirm_time_ms: None,
        latency_ms: None,
        compute_units: None,
        slot: None,
        success: false,
        error: Some(err),
        payload: Vec::new(),
        payload_hash: String::new(),
        source_address: src,
        gmp_destination_chain: String::new(),
        gmp_destination_address: String::new(),
        send_instant: None,
        amplifier_timing: None,
    }
}
