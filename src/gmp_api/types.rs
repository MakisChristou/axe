//! Typed view over the Axelarscan GMP API records this crate reads.
//!
//! The live `/gmp/searchGMP` record carries ~50 fields; we model only the
//! subset our callers need (the express-reimbursement monitor and the
//! load-test verifier's final executed-state recheck). Everything is
//! `Option` / `#[serde(default)]` so a partial record never fails to parse.

use alloy::primitives::{Address, U256};
use serde::Deserialize;

/// `keccak256("Transfer(address,address,uint256)")` — ERC-20 transfer topic0.
const TRANSFER_TOPIC0: &str = "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";

/// One GMP message record as returned by `searchGMP`.
#[derive(Debug, Clone, Deserialize)]
pub struct ExpressRecord {
    #[serde(default)]
    pub command_id: Option<String>,
    #[serde(default)]
    pub message_id: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    /// The ITS token symbol for this transfer, for display alongside amounts.
    #[serde(default)]
    pub symbol: Option<String>,
    /// Present only when an express execution happened on the destination.
    #[serde(default)]
    pub express_executed: Option<ExpressExecuted>,
    /// Present once the canonical `ITS.execute` landed (carries the
    /// `ExpressExecutionFulfilled` reimbursement atomically).
    #[serde(default)]
    pub executed: Option<Executed>,
    #[serde(default)]
    pub interchain_transfer: Option<InterchainTransfer>,
    #[serde(default)]
    pub call: Option<Call>,
}

/// The `express_executed` sub-object: who fronted the funds and where.
#[derive(Debug, Clone, Deserialize)]
pub struct ExpressExecuted {
    #[serde(default)]
    pub chain: Option<String>,
    #[serde(rename = "sourceChain", default)]
    pub source_chain: Option<String>,
    #[serde(rename = "transactionHash", default)]
    pub transaction_hash: Option<String>,
    /// The express executor contract (e.g. the Squid router).
    #[serde(default)]
    pub contract_address: Option<String>,
    /// Top-level `from` mirrors `receipt.from` (the relayer EOA).
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default)]
    pub receipt: Option<Receipt>,
}

impl ExpressExecuted {
    /// The EOA that fronted the funds: `receipt.from`, falling back to the
    /// mirrored top-level `from`.
    pub fn relayer_eoa(&self) -> Option<&str> {
        self.receipt
            .as_ref()
            .and_then(|r| r.from.as_deref())
            .or(self.from.as_deref())
    }
}

/// The `executed` sub-object: the canonical execute that triggers reimbursement.
/// The `ExpressExecutionFulfilled` reimbursement fires atomically inside this tx,
/// so its `receipt.logs` carry the inbound `Transfer` that pays the executor back.
#[derive(Debug, Clone, Deserialize)]
pub struct Executed {
    #[serde(rename = "transactionHash", default)]
    pub transaction_hash: Option<String>,
    #[serde(default)]
    pub receipt: Option<Receipt>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Receipt {
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default)]
    pub logs: Vec<Log>,
}

/// A single event log as the GMP API exposes it (no `address` field is surfaced,
/// so transfers are matched by topic signature and the executor EOA, not token).
#[derive(Debug, Clone, Deserialize)]
pub struct Log {
    #[serde(default)]
    pub topics: Vec<String>,
    #[serde(default)]
    pub data: String,
}

/// A decoded ERC-20 `Transfer(from, to, amount)` event.
#[derive(Debug, Clone)]
pub struct Erc20Transfer {
    pub from: Address,
    pub to: Address,
    pub amount: U256,
}

impl Receipt {
    /// Decode every ERC-20 `Transfer` log in this receipt. Logs that are not
    /// transfers, or whose topics/data don't parse, are skipped.
    pub fn erc20_transfers(&self) -> Vec<Erc20Transfer> {
        self.logs
            .iter()
            .filter_map(|log| {
                let [topic0, from, to] = &log.topics[..3.min(log.topics.len())] else {
                    return None;
                };
                if !topic0.eq_ignore_ascii_case(TRANSFER_TOPIC0) {
                    return None;
                }
                Some(Erc20Transfer {
                    from: topic_address(from)?,
                    to: topic_address(to)?,
                    amount: hex_u256(&log.data)?,
                })
            })
            .collect()
    }
}

/// Extract the 20-byte address from a 32-byte (left-padded) event topic.
fn topic_address(topic: &str) -> Option<Address> {
    let hex = topic.strip_prefix("0x").unwrap_or(topic);
    let start = hex.len().checked_sub(40)?;
    format!("0x{}", &hex[start..]).parse().ok()
}

/// Parse a `0x`-prefixed hex word into a `U256`.
fn hex_u256(data: &str) -> Option<U256> {
    let hex = data.strip_prefix("0x").unwrap_or(data);
    U256::from_str_radix(hex, 16).ok()
}

/// Sum the amounts of the transfers matching `keep`, or `None` if none match.
fn sum_transfers(
    transfers: &[Erc20Transfer],
    keep: impl Fn(&Erc20Transfer) -> bool,
) -> Option<U256> {
    transfers
        .iter()
        .filter(|t| keep(t))
        .fold(None, |acc: Option<U256>, t| {
            Some(acc.unwrap_or(U256::ZERO) + t.amount)
        })
}

#[derive(Debug, Clone, Deserialize)]
pub struct InterchainTransfer {
    #[serde(rename = "destinationChain", default)]
    pub destination_chain: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Call {
    #[serde(rename = "returnValues", default)]
    pub return_values: Option<CallReturnValues>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CallReturnValues {
    #[serde(rename = "sourceChain", default)]
    pub source_chain: Option<String>,
    #[serde(rename = "destinationChain", default)]
    pub destination_chain: Option<String>,
}

/// Whether the express (front-the-funds) leg has been observed.
#[derive(Debug, Clone)]
pub enum Phase1 {
    /// `express_executed` present. Carries the executor EOA / contract and the
    /// express tx hash for the report.
    Executed {
        executor_eoa: Option<String>,
        executor_contract: Option<String>,
        express_tx: Option<String>,
    },
    /// No `express_executed` on this record.
    NotObserved,
}

/// Whether the canonical execute (which reimburses the express executor) landed.
#[derive(Debug, Clone)]
pub enum Phase2 {
    /// `executed` present → `ExpressExecutionFulfilled` fired atomically.
    Reimbursed { execute_tx: Option<String> },
    /// Express happened but canonical execute hasn't landed yet.
    Pending,
    /// Phase 1 never happened, so reimbursement is not applicable.
    NotApplicable,
}

/// Verdict of comparing the amount the executor fronted at express time against
/// the amount it received back when the canonical execute landed. The amounts
/// are raw token base units (the GMP API does not surface the token contract,
/// so transfers are matched by the executor EOA, not by token).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AmountCheck {
    /// Fronted exactly equals reimbursed — the executor was made whole.
    Match { amount: U256 },
    /// Both legs observed but the amounts differ.
    Mismatch { fronted: U256, reimbursed: U256 },
    /// The executor fronted funds but no inbound transfer to it was found in
    /// the canonical execute tx.
    MissingInbound { fronted: U256 },
    /// No outbound transfer from the executor was found in the express tx, so
    /// there is nothing to assert against (e.g. a non-EVM or non-ERC-20 leg).
    NoFrontedTransfer,
}

impl ExpressRecord {
    /// Whether the GMP API considers this message terminally executed on the
    /// destination (the final leg landed). This is the authoritative signal
    /// the load-test verifier uses before labeling a timed-out transfer as
    /// failed: Axelarscan sets `status` to `"executed"` once the destination
    /// `execute` is observed, and the `executed` sub-object is populated in
    /// the same step.
    pub fn is_executed(&self) -> bool {
        self.status
            .as_deref()
            .is_some_and(|s| s.eq_ignore_ascii_case("executed"))
            || self.executed.is_some()
    }

    /// Whether the GMP API reports this message terminally failed: status
    /// `"error"`, set when the hub or destination execute reverted. A failed
    /// message can occasionally be revived by manual intervention (re-execute
    /// with more gas), but the automated relay is done — for a load-test
    /// verdict that is a failure.
    pub fn is_failed(&self) -> bool {
        self.status
            .as_deref()
            .is_some_and(|s| s.eq_ignore_ascii_case("error"))
    }

    /// Classify this record into its two express-reimbursement phases.
    pub fn phase_status(&self) -> (Phase1, Phase2) {
        let Some(ee) = &self.express_executed else {
            return (Phase1::NotObserved, Phase2::NotApplicable);
        };

        let phase1 = Phase1::Executed {
            executor_eoa: ee.relayer_eoa().map(str::to_owned),
            executor_contract: ee.contract_address.clone(),
            express_tx: ee.transaction_hash.clone(),
        };

        let phase2 = match &self.executed {
            Some(ex) => Phase2::Reimbursed {
                execute_tx: ex.transaction_hash.clone(),
            },
            None => Phase2::Pending,
        };

        (phase1, phase2)
    }

    /// Assert the executor was reimbursed the exact amount it fronted.
    ///
    /// Returns `None` until both the express and canonical-execute receipts are
    /// available (i.e. before Phase 2). When both are present, it sums the
    /// executor EOA's outbound `Transfer`s in the express tx (what it fronted)
    /// and its inbound `Transfer`s in the execute tx (what it got back), and
    /// compares the two.
    pub fn reimbursement_amount_check(&self) -> Option<AmountCheck> {
        let ee = self.express_executed.as_ref()?;
        let executor: Address = ee.relayer_eoa()?.parse().ok()?;
        let express_receipt = ee.receipt.as_ref()?;
        let execute_receipt = self.executed.as_ref()?.receipt.as_ref()?;

        let fronted = sum_transfers(&express_receipt.erc20_transfers(), |t| t.from == executor);
        let Some(fronted) = fronted else {
            return Some(AmountCheck::NoFrontedTransfer);
        };

        match sum_transfers(&execute_receipt.erc20_transfers(), |t| t.to == executor) {
            None => Some(AmountCheck::MissingInbound { fronted }),
            Some(reimbursed) if reimbursed == fronted => {
                Some(AmountCheck::Match { amount: fronted })
            }
            Some(reimbursed) => Some(AmountCheck::Mismatch {
                fronted,
                reimbursed,
            }),
        }
    }

    /// Best-effort source chain for display.
    pub fn source_chain(&self) -> Option<&str> {
        self.express_executed
            .as_ref()
            .and_then(|e| e.source_chain.as_deref())
            .or_else(|| {
                self.call
                    .as_ref()
                    .and_then(|c| c.return_values.as_ref())
                    .and_then(|r| r.source_chain.as_deref())
            })
    }

    /// Best-effort destination chain for display.
    pub fn destination_chain(&self) -> Option<&str> {
        self.express_executed
            .as_ref()
            .and_then(|e| e.chain.as_deref())
            .or_else(|| {
                self.interchain_transfer
                    .as_ref()
                    .and_then(|t| t.destination_chain.as_deref())
            })
            .or_else(|| {
                self.call
                    .as_ref()
                    .and_then(|c| c.return_values.as_ref())
                    .and_then(|r| r.destination_chain.as_deref())
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::U256;

    /// Left-pad a 20-byte address into a 32-byte event topic.
    fn topic(addr: &str) -> String {
        format!("0x{:0>64}", addr.trim_start_matches("0x").to_lowercase())
    }

    /// A 32-byte hex data word for an amount.
    fn word(n: u64) -> String {
        format!("0x{n:064x}")
    }

    fn transfer_log(from: &str, to: &str, amount: u64) -> String {
        format!(
            r#"{{ "topics": ["{TRANSFER_TOPIC0}", "{}", "{}"], "data": "{}" }}"#,
            topic(from),
            topic(to),
            word(amount)
        )
    }

    const EOA: &str = "0xe743a49f04f2f77eb2d3b753ae3ad599de8cea84";
    const OTHER: &str = "0x00000000000000000000000000000000000000ff";

    fn reimbursement_record(
        express_logs: &[String],
        execute_logs: Option<&[String]>,
    ) -> ExpressRecord {
        let executed = match execute_logs {
            Some(logs) => format!(
                r#""executed": {{ "receipt": {{ "logs": [{}] }} }},"#,
                logs.join(",")
            ),
            None => String::new(),
        };
        let json = format!(
            r#"{{ "symbol": "TT", "express_executed": {{ "from": "{EOA}", "receipt": {{ "logs": [{}] }} }}, {executed} "message_id": "m" }}"#,
            express_logs.join(",")
        );
        serde_json::from_str(&json).unwrap()
    }

    #[test]
    fn amount_check_matches_when_fronted_equals_reimbursed() {
        let rec = reimbursement_record(
            &[transfer_log(EOA, OTHER, 1000)],
            Some(&[transfer_log(OTHER, EOA, 1000)]),
        );
        assert_eq!(
            rec.reimbursement_amount_check(),
            Some(AmountCheck::Match {
                amount: U256::from(1000)
            })
        );
    }

    #[test]
    fn amount_check_flags_mismatch() {
        let rec = reimbursement_record(
            &[transfer_log(EOA, OTHER, 1000)],
            Some(&[transfer_log(OTHER, EOA, 900)]),
        );
        assert_eq!(
            rec.reimbursement_amount_check(),
            Some(AmountCheck::Mismatch {
                fronted: U256::from(1000),
                reimbursed: U256::from(900),
            })
        );
    }

    #[test]
    fn amount_check_flags_missing_inbound() {
        // Execute tx has a transfer, but none of it lands on the executor EOA.
        let rec = reimbursement_record(
            &[transfer_log(EOA, OTHER, 1000)],
            Some(&[transfer_log(OTHER, OTHER, 1000)]),
        );
        assert_eq!(
            rec.reimbursement_amount_check(),
            Some(AmountCheck::MissingInbound {
                fronted: U256::from(1000)
            })
        );
    }

    #[test]
    fn amount_check_sums_multiple_legs() {
        let rec = reimbursement_record(
            &[transfer_log(EOA, OTHER, 600), transfer_log(EOA, OTHER, 400)],
            Some(&[transfer_log(OTHER, EOA, 1000)]),
        );
        assert_eq!(
            rec.reimbursement_amount_check(),
            Some(AmountCheck::Match {
                amount: U256::from(1000)
            })
        );
    }

    #[test]
    fn amount_check_no_fronted_transfer() {
        let rec = reimbursement_record(
            &[transfer_log(OTHER, OTHER, 1000)],
            Some(&[transfer_log(OTHER, EOA, 1000)]),
        );
        assert_eq!(
            rec.reimbursement_amount_check(),
            Some(AmountCheck::NoFrontedTransfer)
        );
    }

    #[test]
    fn amount_check_unavailable_before_execute() {
        let rec = reimbursement_record(&[transfer_log(EOA, OTHER, 1000)], None);
        assert_eq!(rec.reimbursement_amount_check(), None);
    }

    fn record(status: Option<&str>, executed: bool) -> ExpressRecord {
        let executed_json = if executed {
            r#""executed": { "transactionHash": "0xabc" },"#
        } else {
            ""
        };
        let status_json = status
            .map(|s| format!(r#""status": "{s}","#))
            .unwrap_or_default();
        serde_json::from_str(&format!(
            "{{ {status_json} {executed_json} \"message_id\": \"m\" }}"
        ))
        .unwrap()
    }

    #[test]
    fn is_executed_true_when_status_executed() {
        assert!(record(Some("executed"), false).is_executed());
        // Case-insensitive, per defensive parsing of the live API.
        assert!(record(Some("Executed"), false).is_executed());
    }

    #[test]
    fn is_executed_true_when_executed_object_present() {
        assert!(record(Some("approved"), true).is_executed());
    }

    #[test]
    fn is_executed_false_for_unexecuted_message() {
        assert!(!record(Some("error"), false).is_executed());
        assert!(!record(None, false).is_executed());
    }
}
