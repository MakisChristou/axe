use std::str::FromStr;
use std::time::{Duration, Instant};

use alloy::hex;
use alloy::primitives::{Address, Bytes, U256};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol_types::SolCall;
use chrono::Utc;
use eyre::{Result, WrapErr, eyre};
use indicatif::ProgressBar;

use super::client::RfqClient;
use super::route::{quote_request, validate_quote};
use super::types::{
    Action, ChainRuntime, EvmTransactionPayload, LegExecution, LegResult, Quote, QuoteOutcome,
    QuoteRequest, RoundTripInputBudget, RoutePlan, RunLimits, StatusOutput, TimedQuote,
    TransferState, WalletAsset, parse_amount,
};
use crate::evm::{ERC20, EvmEndpoints, send_tx_robust_with_warning};
use crate::retry::retry_with_fallback_all;
use crate::ui;

const RECEIPT_TIMEOUT: Duration = Duration::from_secs(90);
const MIN_QUOTE_LIFETIME: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ApprovalRequirement {
    spender: Address,
    amount: U256,
}

#[derive(Clone)]
pub enum ExecutionFeedback {
    Detailed,
    Progress(ProgressBar),
    Traffic {
        progress: ProgressBar,
        context: String,
    },
}

impl ExecutionFeedback {
    fn is_detailed(&self) -> bool {
        matches!(self, Self::Detailed)
    }

    fn stage(&self, route: &str, stage: &str) {
        match self {
            Self::Progress(progress) => progress.set_message(format!("{route} · {stage}")),
            Self::Traffic { progress, context } => progress.set_message(format!(
                "intents {} · {context} · {} · {}",
                progress.position(),
                compact_traffic_route(route),
                compact_traffic_stage(stage)
            )),
            Self::Detailed => {}
        }
    }

    fn leg_completed(&self, route: &str) {
        match self {
            Self::Progress(progress) => {
                progress.inc(1);
                progress.set_message(format!("{route} · fulfilled"));
            }
            Self::Traffic { progress, context } => {
                progress.inc(1);
                progress.set_message(format!(
                    "intents {} · {context} · {} · fulfilled",
                    progress.position(),
                    compact_traffic_route(route)
                ));
            }
            Self::Detailed => {}
        }
    }

    fn warn(&self, message: &str) {
        match self {
            Self::Detailed => ui::warn(message),
            Self::Progress(progress) => progress.println(ui::warning_line(message)),
            Self::Traffic { progress, context } => progress.set_message(format!(
                "intents {} · {context} · warning: {}",
                progress.position(),
                ui::scrub_urls(message)
            )),
        }
    }
}

fn compact_traffic_route(route: &str) -> String {
    route
        .replace(" Sepolia", "")
        .replace(" Fuji", "")
        .replace(" -> ", " → ")
}

fn compact_traffic_stage(stage: &str) -> &str {
    match stage {
        "requesting quote" => "quote",
        "checking allowance" => "allowance",
        "approving token" => "approval",
        "submitting deposit" => "deposit",
        "awaiting fulfillment" => "fulfillment",
        other => other,
    }
}

pub async fn execute_round_trip(
    client: &RfqClient,
    chains: &std::collections::HashMap<String, ChainRuntime>,
    signer: &PrivateKeySigner,
    plan: &RoutePlan,
    limits: RunLimits,
    feedback: &ExecutionFeedback,
    results: &mut Vec<LegResult>,
) -> Result<()> {
    if feedback.is_detailed() {
        ui::section(&format!("{} -> {}", plan.from.label(), plan.to.label()));
    }
    let forward_input_limit = match plan.input_budget {
        RoundTripInputBudget::SpendableBalance => None,
        RoundTripInputBudget::Capped { forward, .. } => Some(forward),
    };
    let forward = execute_leg(
        client,
        chains,
        signer,
        LegExecution {
            from: plan.from.clone(),
            to: plan.to.clone(),
            order_type: plan.order_type,
            amount: plan.requested_amount,
            recipient: signer.address(),
            max_input_amount: forward_input_limit,
        },
        limits,
        feedback,
    )
    .await?;
    let reverse_amount = match plan.order_type {
        super::types::OrderType::ExactInput => forward.output_amount,
        super::types::OrderType::ExactOutput => forward.input_amount,
    };
    let mut reverse_source = plan.to.clone();
    reverse_source.balance = reverse_source
        .balance
        .checked_add(forward.output_amount)
        .ok_or_else(|| eyre!("{} balance overflowed", reverse_source.label()))?;
    let reverse_input_limit = match plan.input_budget {
        RoundTripInputBudget::SpendableBalance => None,
        RoundTripInputBudget::Capped { reverse_top_up, .. } => Some(
            forward
                .output_amount
                .checked_add(reverse_top_up)
                .ok_or_else(|| eyre!("{} input limit overflowed", reverse_source.label()))?,
        ),
    };
    results.push(forward);
    let reverse = execute_leg(
        client,
        chains,
        signer,
        LegExecution {
            from: reverse_source,
            to: plan.from.clone(),
            order_type: plan.order_type,
            amount: reverse_amount,
            recipient: signer.address(),
            max_input_amount: reverse_input_limit,
        },
        limits,
        feedback,
    )
    .await?;
    results.push(reverse);
    Ok(())
}

pub async fn execute_leg(
    client: &RfqClient,
    chains: &std::collections::HashMap<String, ChainRuntime>,
    signer: &PrivateKeySigner,
    leg: LegExecution,
    limits: RunLimits,
    feedback: &ExecutionFeedback,
) -> Result<LegResult> {
    let end_to_end_started = Instant::now();
    let route = format!("{} -> {}", leg.from.label(), leg.to.label());
    feedback.stage(&route, "requesting quote");
    let (selected, input_amount) = quote_for_execution(client, signer.address(), &leg).await?;

    ensure_input_approval(
        chains,
        signer,
        &leg.from,
        &selected,
        input_amount,
        feedback,
        &route,
    )
    .await?;

    let deposits: Vec<Action> = selected
        .quote
        .actions
        .iter()
        .filter(|action| action.kind == "transaction")
        .cloned()
        .collect();
    if deposits.len() != 1 {
        return Err(eyre!(
            "intent quote {} returned {} transaction actions; expected exactly one deposit",
            selected.quote.quote_id,
            deposits.len()
        ));
    }
    let deposit = deposits
        .first()
        .ok_or_else(|| eyre!("intent quote had no deposit transaction"))?;
    validate_deposit_action(deposit, &leg.from, input_amount)?;
    if feedback.is_detailed() {
        ui::kv("quote", &selected.quote.quote_id);
        if let Some(swap_id) = selected.quote.backend.tracking.swap_id.as_deref() {
            ui::kv("swap", swap_id);
        }
    }
    feedback.stage(&route, "submitting deposit");
    let deposit_started = Instant::now();
    let deposit_hash = execute_actions(
        chains,
        signer,
        &leg.from,
        &deposits,
        "intent deposit",
        feedback,
    )
    .await?
    .ok_or_else(|| eyre!("intent quote had no deposit transaction"))?;
    let deposit_confirmation_latency_ms = elapsed_ms(deposit_started.elapsed());
    if feedback.is_detailed() {
        ui::tx_hash("deposit", &deposit_hash);
    }

    feedback.stage(&route, "awaiting fulfillment");
    let fulfillment_started = Instant::now();
    let status_output =
        wait_for_fulfillment(client, &selected.quote, &leg.to, limits, feedback, &route).await?;
    let fulfillment_latency_ms = elapsed_ms(fulfillment_started.elapsed());
    let end_to_end_latency_ms = elapsed_ms(end_to_end_started.elapsed());
    let output_amount = parse_amount(&status_output.amount)?;
    if feedback.is_detailed() {
        ui::success(&format!(
            "fulfilled {route} in {:.1}s",
            end_to_end_started.elapsed().as_secs_f64()
        ));
    }
    feedback.leg_completed(&route);
    Ok(LegResult {
        input_amount,
        output_amount,
        quote_latency_ms: elapsed_ms(selected.latency),
        deposit_confirmation_latency_ms,
        fulfillment_latency_ms,
        end_to_end_latency_ms,
    })
}

async fn quote_for_execution(
    client: &RfqClient,
    sender: Address,
    leg: &LegExecution,
) -> Result<(TimedQuote, U256)> {
    let request = quote_request(
        &leg.from,
        &leg.to,
        sender,
        leg.recipient,
        leg.order_type,
        leg.amount,
    );
    let selected = require_quote(client, &request).await?;
    validate_quote(
        &selected.quote,
        &leg.from,
        &leg.to,
        leg.order_type,
        leg.amount,
    )?;
    validate_quote_lifetime(&selected.quote)?;
    let input_amount = parse_amount(&selected.quote.input.amount)?;
    validate_input_limit(input_amount, leg.max_input_amount)?;
    Ok((selected, input_amount))
}

fn validate_input_limit(input: U256, maximum: Option<U256>) -> Result<()> {
    if maximum.is_some_and(|maximum| input > maximum) {
        return Err(eyre!(
            "quote requires more input than the route's wallet safety limit"
        ));
    }
    Ok(())
}

async fn ensure_input_approval(
    chains: &std::collections::HashMap<String, ChainRuntime>,
    signer: &PrivateKeySigner,
    source: &WalletAsset,
    selected: &TimedQuote,
    input_amount: U256,
    feedback: &ExecutionFeedback,
    route: &str,
) -> Result<()> {
    let approvals = selected
        .quote
        .actions
        .iter()
        .filter(|action| action.kind == "approval")
        .cloned()
        .collect::<Vec<_>>();
    if approvals.is_empty() {
        return Ok(());
    }
    feedback.stage(route, "checking allowance");
    let required =
        required_approvals(chains, signer, source, &approvals, input_amount, feedback).await?;
    if required.is_empty() {
        return Ok(());
    }
    feedback.stage(route, "approving token");
    execute_actions(
        chains,
        signer,
        source,
        &required,
        "intent approval",
        feedback,
    )
    .await?;
    validate_quote_lifetime(&selected.quote)
        .wrap_err("intent quote no longer has enough validity remaining after token approval")
}

async fn require_quote(client: &RfqClient, request: &QuoteRequest) -> Result<TimedQuote> {
    match client.quote(request).await? {
        QuoteOutcome::Available(quote) => Ok(*quote),
        QuoteOutcome::Unavailable(reason) => Err(eyre!("intent quote unavailable: {reason}")),
    }
}

fn validate_quote_lifetime(quote: &Quote) -> Result<()> {
    let remaining = quote.quote_expires_in();
    if remaining < MIN_QUOTE_LIFETIME {
        return Err(eyre!(
            "quote {} has only {:.1}s of validity remaining",
            quote.quote_id,
            remaining.as_secs_f64()
        ));
    }
    Ok(())
}

async fn execute_actions(
    chains: &std::collections::HashMap<String, ChainRuntime>,
    signer: &PrivateKeySigner,
    source: &WalletAsset,
    actions: &[Action],
    label: &str,
    feedback: &ExecutionFeedback,
) -> Result<Option<String>> {
    let chain = chains
        .get(&source.id.chain_id)
        .ok_or_else(|| eyre!("no RPC resolved for {}", source.id.chain_id))?;
    let endpoints = EvmEndpoints::connect(std::slice::from_ref(&chain.rpc_url))?;
    let mut last_hash = None;
    for action in actions {
        let request = action_request(action, signer.address(), &source.id.chain_id)?;
        let receipt = send_tx_robust_with_warning(
            &endpoints,
            signer,
            request,
            label,
            RECEIPT_TIMEOUT,
            |message| feedback.warn(message),
        )
        .await?;
        if !receipt.status() {
            return Err(eyre!(
                "{} action '{}' reverted in transaction {}",
                label,
                action.id,
                receipt.transaction_hash
            ));
        }
        last_hash = Some(receipt.transaction_hash.to_string());
    }
    Ok(last_hash)
}

fn action_request(
    action: &Action,
    signer: Address,
    expected_chain: &str,
) -> Result<TransactionRequest> {
    if action.chain != expected_chain {
        return Err(eyre!(
            "action '{}' targets {}, expected {expected_chain}",
            action.id,
            action.chain
        ));
    }
    let payload = action_payload(action)?;
    if payload.kind != "evm_transaction" {
        return Err(eyre!(
            "action '{}' has payload type '{}', expected evm_transaction",
            action.id,
            payload.kind
        ));
    }
    if let Some(from) = payload.from {
        let from: Address = from
            .parse()
            .wrap_err_with(|| format!("action '{}' has an invalid from address", action.id))?;
        if from != signer {
            return Err(eyre!(
                "action '{}' sender {from} does not match axe wallet {signer}",
                action.id
            ));
        }
    }
    let to: Address = payload
        .to
        .parse()
        .wrap_err_with(|| format!("action '{}' has an invalid target", action.id))?;
    let data = hex::decode(payload.data.strip_prefix("0x").unwrap_or(&payload.data))
        .wrap_err_with(|| format!("action '{}' has invalid calldata", action.id))?;
    let value = U256::from_str(&payload.value)
        .wrap_err_with(|| format!("action '{}' has an invalid value", action.id))?;
    Ok(TransactionRequest::default()
        .from(signer)
        .to(to)
        .input(Bytes::from(data).into())
        .value(value))
}

fn action_payload(action: &Action) -> Result<EvmTransactionPayload> {
    serde_json::from_value(action.payload.clone())
        .wrap_err_with(|| format!("action '{}' is not an EVM transaction", action.id))
}

async fn required_approvals(
    chains: &std::collections::HashMap<String, ChainRuntime>,
    signer: &PrivateKeySigner,
    source: &WalletAsset,
    actions: &[Action],
    amount: U256,
    feedback: &ExecutionFeedback,
) -> Result<Vec<Action>> {
    let chain = chains
        .get(&source.id.chain_id)
        .ok_or_else(|| eyre!("no RPC resolved for {}", source.id.chain_id))?;
    let endpoints = EvmEndpoints::connect(std::slice::from_ref(&chain.rpc_url))?;
    let token: Address = source
        .id
        .token_address
        .parse()
        .wrap_err("source token has an invalid EVM address")?;
    let owner = signer.address();
    let mut required = Vec::new();
    for action in actions {
        let requirement = approval_requirement(action, source, amount)?;
        let allowance = retry_with_fallback_all(
            "intent token allowance",
            endpoints.providers(),
            |provider| async move {
                ERC20::new(token, provider)
                    .allowance(owner, requirement.spender)
                    .call()
                    .await
            },
        )
        .await
        .map_err(|error| eyre!("could not read intent token allowance: {error}"))?;
        if allowance < requirement.amount {
            required.push(maximum_approval_action(action, requirement)?);
            if feedback.is_detailed() {
                ui::info("Token allowance is insufficient; approving the maximum amount.");
            }
        } else if feedback.is_detailed() {
            ui::success("Token allowance is already sufficient; skipping approval.");
        }
    }
    Ok(required)
}

fn maximum_approval_action(action: &Action, requirement: ApprovalRequirement) -> Result<Action> {
    let mut payload = action_payload(action)?;
    payload.data = format!(
        "0x{}",
        hex::encode(
            ERC20::approveCall {
                spender: requirement.spender,
                amount: U256::MAX,
            }
            .abi_encode()
        )
    );
    Ok(Action {
        id: action.id.clone(),
        kind: action.kind.clone(),
        chain: action.chain.clone(),
        payload: serde_json::to_value(payload).wrap_err("could not encode maximum approval")?,
    })
}

fn approval_requirement(
    action: &Action,
    source: &WalletAsset,
    amount: U256,
) -> Result<ApprovalRequirement> {
    if source.native {
        return Err(eyre!("native-token quote unexpectedly requested approval"));
    }
    let payload = action_payload(action)?;
    let target: Address = payload
        .to
        .parse()
        .wrap_err_with(|| format!("action '{}' has an invalid target", action.id))?;
    let source_token: Address = source
        .id
        .token_address
        .parse()
        .wrap_err("source token has an invalid EVM address")?;
    let value = U256::from_str(&payload.value)
        .wrap_err_with(|| format!("action '{}' has an invalid value", action.id))?;
    let data = hex::decode(payload.data.strip_prefix("0x").unwrap_or(&payload.data))
        .wrap_err_with(|| format!("action '{}' has invalid calldata", action.id))?;
    if target != source_token || !value.is_zero() || data.len() != 68 {
        return Err(eyre!(
            "action '{}' is not a sufficient approval of the source token",
            action.id
        ));
    }
    let requirement = ApprovalRequirement {
        spender: Address::from_slice(&data[16..36]),
        amount: U256::from_be_slice(&data[36..]),
    };
    if data[..4] != [0x09, 0x5e, 0xa7, 0xb3] || requirement.amount < amount {
        return Err(eyre!(
            "action '{}' is not a sufficient approval of the source token",
            action.id
        ));
    }
    Ok(requirement)
}

fn validate_deposit_action(action: &Action, source: &WalletAsset, amount: U256) -> Result<()> {
    let payload = action_payload(action)?;
    let value = U256::from_str(&payload.value)
        .wrap_err_with(|| format!("action '{}' has an invalid value", action.id))?;
    let expected_value = if source.native { amount } else { U256::ZERO };
    if value != expected_value || payload.data == "0x" || payload.data.is_empty() {
        return Err(eyre!(
            "action '{}' deposit value or calldata does not match the quoted input",
            action.id
        ));
    }
    Ok(())
}

async fn wait_for_fulfillment(
    client: &RfqClient,
    quote: &Quote,
    expected_output: &WalletAsset,
    limits: RunLimits,
    feedback: &ExecutionFeedback,
    route: &str,
) -> Result<StatusOutput> {
    let deadline = Instant::now() + effective_timeout(quote, limits.fulfillment_timeout);
    let mut last_state = None;
    let mut refund_tx = None;
    loop {
        match client.status(&quote.quote_id).await {
            Ok(status) if status.quote_id != quote.quote_id => {
                return Err(eyre!("RFQ status returned a different quote ID"));
            }
            Ok(status) => {
                if last_state != Some(status.state) {
                    if feedback.is_detailed() {
                        ui::kv(
                            "status",
                            &format!("{} ({})", status.state.label(), quote.quote_id),
                        );
                    } else {
                        feedback.stage(route, status.state.label());
                    }
                    last_state = Some(status.state);
                }
                if let Some(refund) = status.refund.as_ref()
                    && refund_tx.as_ref() != Some(&refund.tx_hash)
                {
                    validate_refund(refund, quote)?;
                    if feedback.is_detailed() {
                        ui::tx_hash("refund submitted", &refund.tx_hash);
                    }
                    refund_tx = Some(refund.tx_hash.clone());
                }
                match status.state {
                    TransferState::Done => {
                        let output = status
                            .output
                            .ok_or_else(|| eyre!("DONE status omitted output"))?;
                        validate_status_output(&output, expected_output)?;
                        if feedback.is_detailed()
                            && let Some(destination) = status.destination
                        {
                            ui::tx_hash("destination", &destination.tx_hash);
                        }
                        return Ok(output);
                    }
                    TransferState::Refunded => {
                        return Err(eyre!("intent {} was refunded", quote.quote_id));
                    }
                    TransferState::Failed => {
                        return Err(eyre!("intent {} failed", quote.quote_id));
                    }
                    TransferState::AwaitingDeposit
                    | TransferState::Pending
                    | TransferState::NotFound => {}
                }
            }
            Err(error) => {
                if feedback.is_detailed() {
                    ui::warn(&format!(
                        "status poll failed; retrying: {}",
                        ui::scrub_urls(&error.to_string())
                    ));
                } else {
                    feedback.stage(route, "status retrying");
                }
            }
        }
        if Instant::now() >= deadline {
            return Err(eyre!(
                "intent {} did not fulfill before timeout (last state: {}; refund: {})",
                quote.quote_id,
                last_state.map_or("unknown", TransferState::label),
                refund_tx.as_deref().unwrap_or("not submitted")
            ));
        }
        tokio::time::sleep(limits.poll_interval).await;
    }
}

fn validate_refund(refund: &super::types::Refund, quote: &Quote) -> Result<()> {
    if refund.chain != quote.input.chain
        || !refund.token.eq_ignore_ascii_case(&quote.input.token)
        || parse_amount(&refund.amount)? != parse_amount(&quote.input.amount)?
    {
        return Err(eyre!("RFQ refund does not match the deposited input"));
    }
    Ok(())
}

fn effective_timeout(quote: &Quote, configured: Duration) -> Duration {
    let deadline = quote
        .validity
        .fulfillment_deadline
        .and_then(|deadline| (deadline - Utc::now()).to_std().ok())
        .map(|remaining| remaining + Duration::from_secs(30));
    deadline.map_or(configured, |deadline| deadline.min(configured))
}

fn validate_status_output(output: &StatusOutput, expected: &WalletAsset) -> Result<()> {
    if output.chain != expected.id.chain_id
        || !output
            .token
            .eq_ignore_ascii_case(&expected.id.token_address)
    {
        return Err(eyre!(
            "RFQ DONE output does not match the requested destination asset"
        ));
    }
    Ok(())
}

fn elapsed_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

impl Quote {
    fn quote_expires_in(&self) -> Duration {
        (self.validity.quote_expires_at - Utc::now())
            .to_std()
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    use super::super::types::AssetId;

    #[test]
    fn rejects_an_action_for_another_chain() {
        let action = Action {
            id: "deposit".into(),
            kind: "transaction".into(),
            chain: "eip155:2".into(),
            payload: json!({}),
        };
        let error = action_request(&action, Address::ZERO, "eip155:1").unwrap_err();
        assert!(error.to_string().contains("expected eip155:1"));
    }

    #[test]
    fn accepts_a_matching_evm_action() {
        let signer = "0x0000000000000000000000000000000000000001"
            .parse::<Address>()
            .unwrap();
        let action = Action {
            id: "deposit".into(),
            kind: "transaction".into(),
            chain: "eip155:1".into(),
            payload: json!({
                "type": "evm_transaction",
                "from": signer.to_string(),
                "to": "0x0000000000000000000000000000000000000002",
                "data": "0x1234",
                "value": "0"
            }),
        };
        assert!(action_request(&action, signer, "eip155:1").is_ok());
    }

    #[test]
    fn parses_the_allowance_required_by_an_approval_action() {
        let token = Address::from([1u8; 20]);
        let spender = Address::from([2u8; 20]);
        let amount = U256::from(1_000_000u64);
        let data = ERC20::approveCall { spender, amount }.abi_encode();
        let action = Action {
            id: "approve".into(),
            kind: "approval".into(),
            chain: "eip155:1".into(),
            payload: json!({
                "type": "evm_transaction",
                "from": Address::ZERO.to_string(),
                "to": token.to_string(),
                "data": format!("0x{}", hex::encode(data)),
                "value": "0"
            }),
        };
        let source = WalletAsset {
            id: AssetId {
                chain_id: "eip155:1".into(),
                token_address: token.to_string(),
            },
            chain_label: "Ethereum".into(),
            symbol: "USDC".into(),
            decimals: 6,
            balance: amount,
            native: false,
        };

        assert_eq!(
            approval_requirement(&action, &source, amount).unwrap(),
            ApprovalRequirement { spender, amount }
        );
        assert!(approval_requirement(&action, &source, amount + U256::from(1u8)).is_err());
    }

    #[test]
    fn rewrites_a_validated_approval_to_the_maximum_amount() {
        let token = Address::from([1u8; 20]);
        let spender = Address::from([2u8; 20]);
        let amount = U256::from(1_000_000u64);
        let action = Action {
            id: "approve".into(),
            kind: "approval".into(),
            chain: "eip155:1".into(),
            payload: json!({
                "type": "evm_transaction",
                "from": Address::ZERO.to_string(),
                "to": token.to_string(),
                "data": format!(
                    "0x{}",
                    hex::encode(ERC20::approveCall { spender, amount }.abi_encode())
                ),
                "value": "0"
            }),
        };
        let requirement = ApprovalRequirement { spender, amount };

        let maximum = maximum_approval_action(&action, requirement).unwrap();
        let payload = action_payload(&maximum).unwrap();
        let calldata = ERC20::approveCall::abi_decode(
            &hex::decode(payload.data.strip_prefix("0x").unwrap()).unwrap(),
        )
        .unwrap();

        assert_eq!(calldata.spender, spender);
        assert_eq!(calldata.amount, U256::MAX);
        assert_eq!(payload.to, token.to_string());
        assert_eq!(maximum.chain, action.chain);
    }

    #[test]
    fn sweep_feedback_advances_only_when_a_leg_completes() {
        let progress = ProgressBar::hidden();
        let feedback = ExecutionFeedback::Progress(progress.clone());

        feedback.stage("Fuji/USDC -> Base/USDC", "pending");
        assert_eq!(progress.position(), 0);
        feedback.leg_completed("Fuji/USDC -> Base/USDC");
        assert_eq!(progress.position(), 1);
    }

    #[test]
    fn traffic_feedback_keeps_compact_status_on_one_line() {
        let progress = ProgressBar::hidden();
        let feedback = ExecutionFeedback::Traffic {
            progress: progress.clone(),
            context: "trips 4 · errors 1 · cycle 3 · native/exact-in 1/3".to_owned(),
        };

        feedback.stage(
            "Arbitrum Sepolia/ETH -> Base Sepolia/ETH",
            "submitting deposit",
        );
        assert_eq!(
            progress.message(),
            "intents 0 · trips 4 · errors 1 · cycle 3 · native/exact-in 1/3 · Arbitrum/ETH → Base/ETH · deposit"
        );

        feedback.leg_completed("Arbitrum Sepolia/ETH -> Base Sepolia/ETH");
        assert_eq!(progress.position(), 1);
        assert!(progress.message().starts_with("intents 1 · trips 4"));
    }

    #[test]
    fn automatic_execution_rejects_quotes_above_the_wallet_limit() {
        assert!(validate_input_limit(U256::from(100), Some(U256::from(100))).is_ok());
        assert!(validate_input_limit(U256::from(101), Some(U256::from(100))).is_err());
        assert!(validate_input_limit(U256::MAX, None).is_ok());
    }
}
