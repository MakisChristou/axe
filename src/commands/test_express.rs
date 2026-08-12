//! Express-execution reimbursement monitor (observe-only, v1).
//!
//! Axelar "express execution" lets a relayer front tokens to the recipient on
//! the destination chain (via `expressExecute` on the ITS edge) *before* the
//! canonical GMP proof lands. The relayer is then **reimbursed** when the
//! canonical `ITS.execute` lands — `ExpressExecutionFulfilled` fires atomically
//! inside that execute tx. The signal this monitor reports is exactly that:
//! did the express executor get reimbursed?
//!
//! This module never express-executes anything itself. It observes via the
//! Axelarscan GMP API, so a reported reimbursement is always the real
//! `gmp-express-executor` service being paid back, never a stand-in. The
//! transfer under watch can be originated by
//! [`crate::commands::express_originate`] (`--originate`), which only sends
//! the qualifying source call and hands the tx hash here. Two modes:
//! - `--source-tx <hash>`: poll one message through both phases to
//!   terminal/timeout.
//! - else: for each requested chain, list the `--recent` newest express
//!   transfers and print their two-phase status.
//!
//! The `searchGMP` reqwest client and the `ExpressRecord` view (with the
//! `Phase1`/`Phase2` classifier) live in the shared [`crate::gmp_api`] module,
//! so the load-test verifier can reuse them for its final executed-state check.

use std::time::Instant;

use eyre::{Result, eyre};

use crate::gmp_api::{self, AmountCheck, ExpressRecord, Phase1, Phase2};
use crate::timing::EXPRESS_POLL_INTERVAL;
use crate::types::Network;
use crate::ui;

/// Default number of recent express transfers to report per chain in scan mode.
const DEFAULT_RECENT: usize = 5;

pub async fn run_config(
    network: Network,
    chains: Vec<String>,
    source_tx: Option<String>,
    recent: usize,
    timeout_secs: u64,
) -> Result<()> {
    let base = gmp_api::base_url(network).ok_or_else(|| {
        eyre::eyre!(
            "network {} has no Axelarscan GMP API deployment",
            network.as_str()
        )
    })?;

    ui::section("Express Execution Monitor");
    ui::kv("network", network.as_str());
    ui::kv("gmp api", base);

    match source_tx {
        Some(tx) => poll_single_tx(base, &tx, timeout_secs).await,
        None => scan_chains(base, &chains, recent).await,
    }
}

/// Mode A: poll one source tx through both express phases to terminal/timeout.
async fn poll_single_tx(base: &str, tx: &str, timeout_secs: u64) -> Result<()> {
    ui::section(&format!("Single-tx watch: {tx}"));
    let start = Instant::now();
    let deadline = start + std::time::Duration::from_secs(timeout_secs);

    let mut phase1_printed = false;

    loop {
        let record = gmp_api::search_by_tx(base, tx).await?;
        let Some(record) = record else {
            ui::info("not yet indexed by the GMP API");
            if Instant::now() >= deadline {
                ui::warn(&format!(
                    "tx not indexed within {timeout_secs}s — nothing observed"
                ));
                return Ok(());
            }
            tokio::time::sleep(EXPRESS_POLL_INTERVAL).await;
            continue;
        };

        let (phase1, phase2) = record.phase_status();

        if !phase1_printed && matches!(&phase1, Phase1::Executed { .. }) {
            print_phase1(&phase1);
            phase1_printed = true;
        }

        match (&phase1, &phase2) {
            (Phase1::NotObserved, _) => {
                if Instant::now() >= deadline {
                    ui::warn(&format!(
                        "no express execution observed within {timeout_secs}s ({})",
                        ui::format_elapsed(start)
                    ));
                    return Ok(());
                }
            }
            (Phase1::Executed { .. }, Phase2::Reimbursed { .. }) => {
                print_phase2(&phase2);
                let check = record.reimbursement_amount_check();
                report_amount_check(check.as_ref(), record.symbol.as_deref());
                if let Some(reason) = amount_check_failure(check.as_ref()) {
                    return Err(eyre!("express reimbursement amount check failed: {reason}"));
                }
                ui::success(&format!(
                    "express executor reimbursed in full ({})",
                    ui::format_elapsed(start)
                ));
                return Ok(());
            }
            (Phase1::Executed { .. }, _) => {
                if Instant::now() >= deadline {
                    ui::warn(&format!(
                        "reimbursement still PENDING after {timeout_secs}s — canonical execute not observed ({})",
                        ui::format_elapsed(start)
                    ));
                    return Ok(());
                }
            }
        }

        tokio::time::sleep(EXPRESS_POLL_INTERVAL).await;
    }
}

/// Mode B: for each chain, list recent express transfers and report both phases.
async fn scan_chains(base: &str, chains: &[String], recent: usize) -> Result<()> {
    let recent = if recent == 0 { DEFAULT_RECENT } else { recent };

    if chains.is_empty() {
        ui::warn("no chains given — pass express-supported chain ids to scan");
        return Ok(());
    }

    for chain in chains {
        scan_one_chain(base, chain, recent).await?;
    }
    Ok(())
}

async fn scan_one_chain(base: &str, chain: &str, recent: usize) -> Result<()> {
    ui::section(&format!(
        "Chain: {chain} (latest {recent} express transfers)"
    ));
    let records = gmp_api::search_recent_express(base, Some(chain), recent).await?;

    if records.is_empty() {
        ui::info("no express transfers observed for this chain");
        return Ok(());
    }

    let total = records.len();
    for (i, record) in records.iter().enumerate() {
        print_record_report(i + 1, total, record);
    }
    Ok(())
}

/// One transfer's two-phase report block in scan mode.
fn print_record_report(index: usize, total: usize, record: &ExpressRecord) {
    let route = format!(
        "{} → {}",
        record.source_chain().unwrap_or("?"),
        record.destination_chain().unwrap_or("?"),
    );
    ui::step_header(index, total, &route);

    if let Some(mid) = &record.message_id {
        ui::kv("message_id", mid);
    }
    if let Some(cid) = &record.command_id {
        ui::kv("command_id", cid);
    }
    if let Some(status) = &record.status {
        ui::kv("status", status);
    }

    let (phase1, phase2) = record.phase_status();
    print_phase1(&phase1);
    print_phase2(&phase2);
    if matches!(phase2, Phase2::Reimbursed { .. }) {
        let check = record.reimbursement_amount_check();
        report_amount_check(check.as_ref(), record.symbol.as_deref());
    }
}

fn print_phase1(phase1: &Phase1) {
    match phase1 {
        Phase1::Executed {
            executor_eoa,
            executor_contract,
            express_tx,
        } => {
            ui::success("Phase 1: express executed (funds fronted)");
            if let Some(eoa) = executor_eoa {
                ui::address("executor EOA", eoa);
            }
            if let Some(contract) = executor_contract {
                ui::address("executor contract", contract);
            }
            if let Some(tx) = express_tx {
                ui::tx_hash("express tx", tx);
            }
        }
        Phase1::NotObserved => {
            ui::warn("Phase 1: no express execution observed");
        }
    }
}

fn print_phase2(phase2: &Phase2) {
    match phase2 {
        Phase2::Reimbursed { execute_tx } => {
            ui::success("Phase 2: executor reimbursed (canonical execute landed)");
            if let Some(tx) = execute_tx {
                ui::tx_hash("execute tx", tx);
            }
        }
        Phase2::Pending => {
            ui::warn("Phase 2: reimbursement PENDING — canonical execute not yet observed");
        }
        Phase2::NotApplicable => {
            ui::info("Phase 2: n/a (no express execution to reimburse)");
        }
    }
}

/// Print the fronted-vs-reimbursed amount verdict. Amounts are raw token base
/// units (the GMP API does not surface the token contract).
fn report_amount_check(check: Option<&AmountCheck>, symbol: Option<&str>) {
    let unit = symbol.map(|s| format!(" {s}")).unwrap_or_default();
    match check {
        Some(AmountCheck::Match { amount }) => {
            ui::success(&format!(
                "amount check: fronted == reimbursed = {amount}{unit} (base units)"
            ));
        }
        Some(AmountCheck::Mismatch {
            fronted,
            reimbursed,
        }) => {
            ui::error(&format!(
                "amount MISMATCH: fronted {fronted}{unit} != reimbursed {reimbursed}{unit} (base units)"
            ));
        }
        Some(AmountCheck::MissingInbound { fronted }) => {
            ui::error(&format!(
                "amount check: executor fronted {fronted}{unit} but received nothing back in the execute tx"
            ));
        }
        Some(AmountCheck::NoFrontedTransfer) => {
            ui::warn(
                "amount check: no executor outbound transfer in the express tx — cannot assert (non-EVM / non-ERC-20 leg?)",
            );
        }
        None => {
            ui::info("amount check: unavailable (execute receipt not yet indexed)");
        }
    }
}

/// The reasons a reimbursement must be treated as a failure rather than a pass:
/// the amounts disagree, or the executor was never paid back.
fn amount_check_failure(check: Option<&AmountCheck>) -> Option<String> {
    match check {
        Some(AmountCheck::Mismatch {
            fronted,
            reimbursed,
        }) => Some(format!(
            "fronted {fronted} != reimbursed {reimbursed} (base units)"
        )),
        Some(AmountCheck::MissingInbound { fronted }) => Some(format!(
            "executor fronted {fronted} (base units) but no inbound transfer in the execute tx"
        )),
        _ => None,
    }
}
