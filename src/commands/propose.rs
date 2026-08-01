//! `axe propose` — submit an AxelarServiceGovernance proposal (operator
//! fast-path or time-lock) to an edge chain's ASG via an Axelar gov proposal
//! that calls `AxelarnetGateway.call_contract`. Verifies the target is a real,
//! correctly-wired ASG, submits, prints the vote action, and monitors to a
//! terminal status. With `--relay` it then delivers the GMP to the edge chain
//! and executes it (operator fast-path or time-lock) — see `relay.rs`.

mod helpers;
mod relay;
pub mod types;

use std::str::FromStr;

use alloy::primitives::{Address, Bytes};
use chrono::TimeZone;
use cosmrs::crypto::secp256k1::SigningKey;
use eyre::Result;
use owo_colors::OwoColorize;
use serde_json::json;

use crate::config::ChainsConfig;
use crate::config_source;
use crate::cosmos::{
    build_execute_msg_any, build_submit_proposal_any, check_axelar_balance, derive_axelar_wallet,
    extract_proposal_id, sign_and_broadcast_cosmos_tx,
};
use crate::types::Network;
use crate::ui;

use helpers::AsgInfo;
use types::{ProposalType, ResolvedConfig};

pub use types::ProposeArgs;

/// Extra seconds added to the ASG's minimum delay when auto-computing a
/// time-lock `eta`, so the schedule is comfortably in the future.
const ETA_BUFFER_SECS: u64 = 300;
/// Gas headroom required on top of the gov deposit, in `uaxl`.
const GAS_BUFFER_UAXL: u128 = 10_000_000;

pub async fn run(args: ProposeArgs) -> Result<()> {
    let network = args.network;
    gate_mainnet(network, args.confirm_mainnet)?;

    let config_source = config_source::resolve(network, None).await?;
    let config = ChainsConfig::load(config_source.path()).await?;
    let cfg = helpers::resolve(network, &config, &args.chain)?;
    let (target, calldata, action_label) = resolve_action(&args, &cfg)?;

    ui::section(&format!(
        "propose: {action_label} on {} [{}]",
        args.chain,
        args.proposal_type.label()
    ));

    let asg = verify(&cfg).await?;
    helpers::check_not_already_proposed(&cfg, args.proposal_type, target, calldata.clone()).await?;

    // With --relay, refuse before submitting if the EVM relayer key is missing
    // or unfunded on the edge chain — don't spend a proposal we can't relay.
    if args.relay {
        relay::preflight_relayer(&cfg).await?;
    }

    let eta = compute_eta(args.proposal_type, &args, &asg);
    let calldata_hex = format!("0x{}", alloy::hex::encode(&calldata));
    let payload = helpers::encode_governance_payload(
        args.proposal_type.command(),
        target,
        calldata.clone(),
        eta,
    );

    let target_label = helpers::target_name(&config, &args.chain, target);
    if !confirm_submit(
        &args,
        &cfg,
        &asg,
        target,
        target_label.as_deref(),
        &calldata_hex,
        eta,
    )
    .await
    {
        ui::info("aborted — no proposal submitted");
        return Ok(());
    }

    let mnemonic = std::env::var("MNEMONIC")
        .map_err(|_| eyre::eyre!("MNEMONIC not set — needed to sign the gov proposal"))?;
    let (signing_key, submitter) = derive_axelar_wallet(&mnemonic)?;
    ui::address("submitter", &submitter);

    let proposal_id = submit(
        &cfg,
        &args,
        &action_label,
        &payload,
        &signing_key,
        &submitter,
    )
    .await?;
    ui::kv("proposal submitted", &proposal_id.to_string());
    ui::action_required(&[
        "Vote on the proposal:",
        &format!(
            "./vote_{env}_proposal.sh {env}-nodes {proposal_id}",
            env = vote_env(network)
        ),
    ]);

    helpers::monitor_proposal(&cfg.lcd, proposal_id).await?;

    if args.relay {
        let plan = relay::RelayPlan {
            ptype: args.proposal_type,
            target,
            calldata,
            payload,
        };
        relay::relay(&cfg, &asg, &plan, &signing_key, &submitter).await?;
    } else {
        ui::info("proposal passed; not relaying (pass --relay to relay + execute).");
    }
    Ok(())
}

async fn verify(cfg: &ResolvedConfig) -> Result<AsgInfo> {
    let spinner = ui::wait_spinner("verifying ASG on-chain (read-only)...");
    let asg = helpers::verify_asg(cfg).await;
    spinner.finish_and_clear();
    let asg = asg?;

    ui::address("ASG", &cfg.asg_address);
    ui::address("operator", &asg.operator.to_string());
    ui::kv("governanceChain", &asg.governance_chain);
    ui::kv("governanceAddress", &asg.governance_address);
    ui::kv(
        "minTimeLockDelay",
        &format!("{}s", asg.minimum_time_lock_delay),
    );
    Ok(asg)
}

async fn submit(
    cfg: &ResolvedConfig,
    args: &ProposeArgs,
    action_label: &str,
    payload: &[u8],
    signing_key: &SigningKey,
    submitter: &str,
) -> Result<u64> {
    let expedited = !args.standard;
    let deposit = if expedited {
        &cfg.expedited_deposit_amount
    } else {
        &cfg.deposit_amount
    };
    let min_balance = deposit.parse::<u128>()? + GAS_BUFFER_UAXL;
    check_axelar_balance(
        &cfg.lcd,
        &cfg.chain_id,
        submitter,
        &cfg.fee_denom,
        min_balance,
    )
    .await?;

    let call_contract = json!({
        "call_contract": {
            "destination_chain": cfg.edge_axelar_id,
            "destination_address": cfg.asg_address,
            "payload": alloy::hex::encode(payload),
        }
    });
    let inner = build_execute_msg_any(&cfg.gov_module, &cfg.axelarnet_gateway, &call_contract)?;
    let title = format!("axe: {action_label} on {}", cfg.edge_axelar_id);
    let summary = format!(
        "Service-Governance {} proposal: {action_label}. Submitted via axe propose.",
        args.proposal_type.label()
    );
    let proposal = build_submit_proposal_any(
        submitter,
        vec![inner],
        &title,
        &summary,
        deposit,
        &cfg.fee_denom,
        expedited,
    )?;

    let tx_resp = sign_and_broadcast_cosmos_tx(
        signing_key,
        submitter,
        &cfg.lcd,
        &cfg.chain_id,
        &cfg.fee_denom,
        cfg.gas_price,
        vec![proposal],
    )
    .await?;
    extract_proposal_id(&tx_resp)
}

fn resolve_action(args: &ProposeArgs, cfg: &ResolvedConfig) -> Result<(Address, Bytes, String)> {
    if let Some(op) = args.op {
        let (target, calldata) = helpers::build_call(op, cfg, args.its_chain.as_deref())?;
        return Ok((target, calldata, op.label().to_string()));
    }

    let calldata_hex = args
        .calldata
        .as_deref()
        .ok_or_else(|| eyre::eyre!("provide --op <name> or --calldata <hex> --target <addr>"))?;
    let target_raw = args
        .target
        .as_deref()
        .ok_or_else(|| eyre::eyre!("--calldata requires --target <addr>"))?;
    let target = Address::from_str(target_raw)
        .map_err(|e| eyre::eyre!("invalid --target '{target_raw}': {e}"))?;
    let stripped = calldata_hex.strip_prefix("0x").unwrap_or(calldata_hex);
    let calldata: Bytes = alloy::hex::decode(stripped)
        .map_err(|e| eyre::eyre!("invalid --calldata hex: {e}"))?
        .into();
    Ok((target, calldata, format!("raw call to {target_raw}")))
}

async fn confirm_submit(
    args: &ProposeArgs,
    cfg: &ResolvedConfig,
    asg: &AsgInfo,
    target: Address,
    target_label: Option<&str>,
    calldata_hex: &str,
    eta: u64,
) -> bool {
    if args.yes {
        return true;
    }
    let expedited = !args.standard;
    let deposit = if expedited {
        &cfg.expedited_deposit_amount
    } else {
        &cfg.deposit_amount
    };
    ui::section("review before submit");
    ui::kv("proposal type", args.proposal_type.label());
    match target_label {
        Some(name) => ui::kv("target", &format!("{target}  ({name})")),
        None => ui::kv(
            "target",
            &format!("{target}  {}", "Unknown Destination".red().bold()),
        ),
    }
    ui::kv("calldata", calldata_hex);
    // Pretty-print the calldata with the same decoder as `axe decode`, for both
    // catalog ops and raw calls. A decode failure on a user-supplied raw call is
    // flagged in red; catalog calldata is known-good, so a selector the decoder
    // doesn't recognise is just left unannotated.
    let decoded = crate::commands::decode::run(calldata_hex).is_ok();
    if !decoded && args.op.is_none() {
        println!("  {}", "Unknown Calldata".red().bold());
    }
    ui::kv(
        "deposit",
        &format!(
            "{} ({})",
            format_deposit(deposit, &cfg.fee_denom),
            if expedited { "expedited" } else { "standard" }
        ),
    );
    if eta > 0 {
        ui::kv("timelock eta", &format_eta(eta));
    }
    if args.relay {
        show_relay_readiness(args.proposal_type, asg.operator);
    }
    ui::confirm("Submit this proposal?").await
}

/// With `--relay`, tell the user up front whether the environment can actually
/// finish the relay — so the yes/no is informed. The operator fast-path needs
/// the operator's key for the final `executeOperatorProposal`; the time-lock
/// path's execute is permissionless but still needs a funded EVM key for gas.
fn show_relay_readiness(ptype: ProposalType, operator: Address) {
    match (ptype, relay::operator_key_status(operator)) {
        (ProposalType::Operator, relay::OperatorKey::Operator(addr)) => {
            ui::kv("relay", &format!("execute as operator {addr} ✓"));
        }
        (ProposalType::Operator, relay::OperatorKey::NotOperator(addr)) => ui::warn(&format!(
            "relay: EVM key {addr} is NOT the operator {operator} — the proposal will be relayed + \
             APPROVED but NOT executed; you'll get a cast command to run as the operator"
        )),
        (ProposalType::Operator, relay::OperatorKey::Missing) => ui::warn(&format!(
            "relay: no EVM_GOVERNANCE_OPERATOR_KEY / EVM_PRIVATE_KEY set — cannot relay (operator is {operator})"
        )),
        (ProposalType::Timelock, relay::OperatorKey::Missing) => ui::warn(
            "relay: no EVM_GOVERNANCE_OPERATOR_KEY / EVM_PRIVATE_KEY set — cannot relay (time-lock \
             execute is permissionless but still needs a funded EVM key for gas)",
        ),
        (
            ProposalType::Timelock,
            relay::OperatorKey::Operator(addr) | relay::OperatorKey::NotOperator(addr),
        ) => {
            ui::kv(
                "relay",
                &format!("submitProof + executeProposal after eta with {addr}"),
            );
        }
    }
}

/// Render a micro-denom amount (e.g. `"3000000000"`, `"uaxl"`) as a human
/// figure (`"3000 AXL"`). Micro denoms (`u…`, 1e6 base) are converted and the
/// `u` prefix dropped + upper-cased; anything else is shown verbatim.
fn format_deposit(amount: &str, micro_denom: &str) -> String {
    let (Ok(micro), Some(unit)) = (amount.parse::<u128>(), micro_denom.strip_prefix('u')) else {
        return format!("{amount} {micro_denom}");
    };
    let unit = unit.to_uppercase();
    let whole = micro / 1_000_000;
    let frac = micro % 1_000_000;
    if frac == 0 {
        format!("{whole} {unit}")
    } else {
        let frac = format!("{frac:06}");
        format!("{whole}.{} {unit}", frac.trim_end_matches('0'))
    }
}

/// Render a unix-seconds timestamp as local wall-clock time plus the raw value.
fn format_eta(eta: u64) -> String {
    match chrono::Local.timestamp_opt(eta as i64, 0).single() {
        Some(dt) => format!("{} ({eta})", dt.format("%H:%M:%S %d %b %Y")),
        None => eta.to_string(),
    }
}

fn compute_eta(ptype: ProposalType, args: &ProposeArgs, asg: &AsgInfo) -> u64 {
    match ptype {
        ProposalType::Operator => 0,
        ProposalType::Timelock => args.eta.unwrap_or_else(|| {
            let now = chrono::Utc::now().timestamp().max(0) as u64;
            now + asg.minimum_time_lock_delay + ETA_BUFFER_SECS
        }),
    }
}

fn gate_mainnet(network: Network, confirmed: bool) -> Result<()> {
    if network == Network::Mainnet && !confirmed {
        return Err(eyre::eyre!(
            "refusing to propose on mainnet without --confirm-mainnet (giga-gated)"
        ));
    }
    Ok(())
}

/// Map a network to the short env name used by the `vote_<env>_proposal.sh`
/// scripts and the `<env>-nodes` kube context (devnet-amplifier → `devnet`).
fn vote_env(network: Network) -> &'static str {
    match network {
        Network::Mainnet => "mainnet",
        Network::Testnet => "testnet",
        Network::Stagenet => "stagenet",
        Network::DevnetAmplifier => "devnet",
    }
}

#[cfg(test)]
mod tests {
    use super::format_deposit;

    #[test]
    fn format_deposit_converts_micro_to_whole_axl() {
        assert_eq!(format_deposit("3000000000", "uaxl"), "3000 AXL");
        assert_eq!(format_deposit("2000000000", "uaxl"), "2000 AXL");
    }

    #[test]
    fn format_deposit_trims_fractional_axl() {
        assert_eq!(format_deposit("1500000", "uaxl"), "1.5 AXL");
        assert_eq!(format_deposit("1", "uaxl"), "0.000001 AXL");
    }

    #[test]
    fn format_deposit_passes_through_non_micro_denoms() {
        assert_eq!(format_deposit("123", "axl"), "123 axl");
        assert_eq!(format_deposit("not-a-number", "uaxl"), "not-a-number uaxl");
    }
}
