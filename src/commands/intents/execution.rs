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
use super::presentation::set_intent_traffic_message;
use super::route::{quote_request, validate_quote};
use super::types::{
    Action, ActionKind, ActionPayload, ChainRuntime, EvmTransactionPayload, LegExecution, LegPlan,
    LegResult, PreparedDeposit, Quote, QuoteOutcome, QuoteRequest, RoundTripInputBudget, RoutePlan,
    RunLimits, StatusOutput, SubmittedIntent, TimedQuote, TransferState, WalletAsset, parse_amount,
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
    Debugger,
    Startup(ProgressBar),
    Progress(ProgressBar),
    Traffic {
        progress: ProgressBar,
        context: String,
    },
}

impl ExecutionFeedback {
    fn is_detailed(&self) -> bool {
        matches!(self, Self::Detailed | Self::Debugger)
    }

    fn is_debugger(&self) -> bool {
        matches!(self, Self::Debugger)
    }

    fn stage(&self, route: &str, stage: &str) {
        match self {
            Self::Progress(progress) | Self::Startup(progress) => {
                progress.set_message(format!("{route} · {stage}"));
            }
            Self::Traffic { progress, context } => set_intent_traffic_message(
                progress,
                &format!(
                    "{} intents · {context} · {}",
                    progress.position(),
                    compact_traffic_stage(stage)
                ),
            ),
            Self::Detailed | Self::Debugger => {}
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
                set_intent_traffic_message(
                    progress,
                    &format!("{} intents · {context} · fulfilled", progress.position()),
                );
            }
            Self::Detailed | Self::Debugger | Self::Startup(_) => {}
        }
    }

    fn warn(&self, message: &str) {
        match self {
            Self::Detailed | Self::Debugger | Self::Startup(_) => ui::warn(message),
            Self::Progress(progress) => progress.println(ui::warning_line(message)),
            Self::Traffic { progress, context } => set_intent_traffic_message(
                progress,
                &format!(
                    "{} intents · warning: {} · {context}",
                    progress.position(),
                    ui::scrub_urls(message)
                ),
            ),
        }
    }
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
            settlement_contract: plan.forward_settlement_contract,
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
            settlement_contract: plan.reverse_settlement_contract,
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
    let submitted = submit_leg(client, chains, signer, leg, feedback).await?;
    finish_submitted_intent(client, submitted, limits, feedback).await
}

pub async fn execute_planned_leg(
    client: &RfqClient,
    chains: &std::collections::HashMap<String, ChainRuntime>,
    signer: &PrivateKeySigner,
    plan: LegPlan,
    limits: RunLimits,
    feedback: &ExecutionFeedback,
) -> Result<LegResult> {
    validate_quote(
        &plan.quote.quote,
        &plan.from,
        &plan.to,
        plan.order_type,
        plan.requested_amount,
    )?;
    validate_quote_lifetime(&plan.quote.quote)?;
    let input_amount = parse_amount(&plan.quote.quote.input.amount)?;
    validate_input_limit(input_amount, None)?;
    let settlement = validate_quote_actions(&plan.quote.quote, &plan.from, input_amount)?;
    if settlement != plan.settlement_contract {
        return Err(eyre!("intent quote changed its settlement contract"));
    }
    let submitted = submit_selected_leg(
        chains,
        signer,
        SelectedLegExecution {
            from: plan.from,
            to: plan.to,
            selected: plan.quote,
            input_amount,
            started: Instant::now(),
        },
        feedback,
    )
    .await?;
    finish_submitted_intent(client, submitted, limits, feedback).await
}

struct SelectedLegExecution {
    from: WalletAsset,
    to: WalletAsset,
    selected: TimedQuote,
    input_amount: U256,
    started: Instant,
}

pub(super) async fn submit_leg(
    client: &RfqClient,
    chains: &std::collections::HashMap<String, ChainRuntime>,
    signer: &PrivateKeySigner,
    leg: LegExecution,
    feedback: &ExecutionFeedback,
) -> Result<SubmittedIntent> {
    let started = Instant::now();
    let route = format!("{} -> {}", leg.from.label(), leg.to.label());
    feedback.stage(&route, "requesting quote");
    let (selected, input_amount) = quote_for_execution(client, signer.address(), &leg).await?;
    submit_selected_leg(
        chains,
        signer,
        SelectedLegExecution {
            from: leg.from,
            to: leg.to,
            selected,
            input_amount,
            started,
        },
        feedback,
    )
    .await
}

pub(super) async fn prepare_deposit(
    client: &RfqClient,
    sender: Address,
    leg: LegExecution,
) -> Result<PreparedDeposit> {
    let (selected, input_amount) = quote_for_execution(client, sender, &leg).await?;
    let action = deposit_action(&selected.quote, &leg.from, input_amount)?;
    Ok(PreparedDeposit {
        transaction: crate::evm::pipeline::PipelineTransaction {
            request: action_request(action, sender, &leg.from.id.chain_id)?,
            token: leg.from.id.token_address.parse()?,
            amount: input_amount,
            deadline: Instant::now()
                + selected
                    .quote
                    .quote_expires_in()
                    .saturating_sub(Duration::from_secs(2)),
        },
        quote_id: selected.quote.quote_id,
        quote_latency_ms: elapsed_ms(selected.latency),
    })
}

fn deposit_action<'a>(quote: &'a Quote, source: &WalletAsset, amount: U256) -> Result<&'a Action> {
    let mut deposits = quote
        .actions
        .iter()
        .filter(|action| action.kind == ActionKind::Transaction);
    let deposit = deposits
        .next()
        .ok_or_else(|| eyre!("intent quote had no deposit transaction"))?;
    if deposits.next().is_some() {
        return Err(eyre!("intent quote returned multiple deposit transactions"));
    }
    validate_deposit_action(deposit, source, amount)?;
    Ok(deposit)
}

async fn submit_selected_leg(
    chains: &std::collections::HashMap<String, ChainRuntime>,
    signer: &PrivateKeySigner,
    execution: SelectedLegExecution,
    feedback: &ExecutionFeedback,
) -> Result<SubmittedIntent> {
    let route = format!("{} -> {}", execution.from.label(), execution.to.label());
    let selected = execution.selected;

    ensure_input_approval(
        chains,
        signer,
        &execution.from,
        &selected,
        execution.input_amount,
        feedback,
        &route,
    )
    .await?;

    let deposit = deposit_action(&selected.quote, &execution.from, execution.input_amount)?;
    if feedback.is_detailed() {
        ui::kv("quote", &selected.quote.quote_id);
        if let Some(swap_id) = selected.quote.backend.swap_id() {
            ui::kv("swap", swap_id);
        }
    }
    feedback.stage(&route, "submitting deposit");
    let deposit_started = Instant::now();
    let deposit_hash = execute_actions(
        chains,
        signer,
        &execution.from,
        std::slice::from_ref(deposit),
        "intent deposit",
        feedback,
    )
    .await?
    .ok_or_else(|| eyre!("intent quote had no deposit transaction"))?;
    let deposit_confirmation_latency_ms = elapsed_ms(deposit_started.elapsed());
    if feedback.is_detailed() {
        ui::tx_hash("deposit", &deposit_hash);
    }

    Ok(SubmittedIntent {
        from: execution.from,
        to: execution.to,
        selected,
        input_amount: execution.input_amount,
        started: execution.started,
        deposit_confirmation_latency_ms,
    })
}

pub(super) async fn finish_submitted_intent(
    client: &RfqClient,
    submitted: SubmittedIntent,
    limits: RunLimits,
    feedback: &ExecutionFeedback,
) -> Result<LegResult> {
    let route = format!("{} -> {}", submitted.from.label(), submitted.to.label());
    feedback.stage(&route, "awaiting fulfillment");
    let fulfillment_started = Instant::now();
    let status_output = wait_for_fulfillment(
        client,
        &submitted.selected.quote,
        &submitted.to,
        limits,
        feedback,
        &route,
    )
    .await?;
    let fulfillment_latency_ms = elapsed_ms(fulfillment_started.elapsed());
    let end_to_end_latency_ms = elapsed_ms(submitted.started.elapsed());
    let output_amount = parse_amount(&status_output.amount)?;
    if feedback.is_detailed() {
        ui::success(&format!(
            "fulfilled {route} in {:.1}s",
            submitted.started.elapsed().as_secs_f64()
        ));
    }
    feedback.leg_completed(&route);
    Ok(LegResult {
        input_amount: submitted.input_amount,
        output_amount,
        quote_latency_ms: elapsed_ms(submitted.selected.latency),
        deposit_confirmation_latency_ms: submitted.deposit_confirmation_latency_ms,
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
    let settlement = validate_quote_actions(&selected.quote, &leg.from, input_amount)?;
    if settlement != leg.settlement_contract {
        return Err(eyre!("intent quote changed its settlement contract"));
    }
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
        .filter(|action| action.kind == ActionKind::Approval)
        .cloned()
        .collect::<Vec<_>>();
    if approvals.is_empty() {
        return Ok(());
    }
    feedback.stage(route, "checking allowance");
    let required = required_approvals(
        chains,
        signer,
        source,
        &approvals,
        input_amount,
        None,
        feedback,
    )
    .await?;
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
    if let Some(from) = &payload.from {
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
    let chain_id = expected_chain
        .strip_prefix("eip155:")
        .ok_or_else(|| eyre!("expected an EVM chain ID, got {expected_chain}"))?
        .parse::<u64>()
        .wrap_err_with(|| format!("invalid EVM chain ID {expected_chain}"))?;
    Ok(TransactionRequest {
        chain_id: Some(chain_id),
        ..Default::default()
    }
    .from(signer)
    .to(to)
    .input(Bytes::from(data).into())
    .value(value))
}

fn action_payload(action: &Action) -> Result<&EvmTransactionPayload> {
    match &action.payload {
        ActionPayload::EvmTransaction(payload) => Ok(payload),
        ActionPayload::SolanaInstructions(_) | ActionPayload::DepositAddress(_) => {
            Err(eyre!("action '{}' is not an EVM transaction", action.id))
        }
    }
}

async fn required_approvals(
    chains: &std::collections::HashMap<String, ChainRuntime>,
    signer: &PrivateKeySigner,
    source: &WalletAsset,
    actions: &[Action],
    amount: U256,
    approval_amount: Option<U256>,
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
        let approval_amount = approval_amount.unwrap_or(requirement.amount);
        if approval_amount < requirement.amount {
            return Err(eyre!("approval cap is below the quoted input"));
        }
        if allowance < approval_amount {
            required.push(approval_action(action, requirement, approval_amount)?);
            if feedback.is_detailed() {
                ui::info("Token allowance is insufficient; approving the bounded amount.");
            }
        } else if feedback.is_detailed() {
            ui::success("Token allowance is already sufficient; skipping approval.");
        }
    }
    Ok(required)
}

fn approval_action(
    action: &Action,
    requirement: ApprovalRequirement,
    amount: U256,
) -> Result<Action> {
    if amount < requirement.amount {
        return Err(eyre!("approval amount is below the quoted input"));
    }
    let mut bounded = action.clone();
    let ActionPayload::EvmTransaction(payload) = &mut bounded.payload else {
        return Err(eyre!("action '{}' is not an EVM transaction", action.id));
    };
    payload.data = format!(
        "0x{}",
        hex::encode(
            ERC20::approveCall {
                spender: requirement.spender,
                amount,
            }
            .abi_encode()
        )
    );
    Ok(bounded)
}

pub(super) async fn prepare_stress_approval(
    chains: &std::collections::HashMap<String, ChainRuntime>,
    signer: &PrivateKeySigner,
    plan: &LegPlan,
    approval_amount: U256,
    feedback: &ExecutionFeedback,
) -> Result<()> {
    let mut approvals = plan
        .quote
        .quote
        .actions
        .iter()
        .filter(|action| action.kind == ActionKind::Approval)
        .cloned()
        .collect::<Vec<_>>();
    if approvals.is_empty() {
        approvals.push(stress_approval_action(
            plan,
            signer.address(),
            approval_amount,
        ));
    }
    let route = format!("{} -> {}", plan.from.label(), plan.to.label());
    let required = required_approvals(
        chains,
        signer,
        &plan.from,
        &approvals,
        plan.input_amount,
        Some(approval_amount),
        feedback,
    )
    .await?;
    if required.is_empty() {
        feedback.stage(&route, "allowance sufficient · no approval needed");
        return Ok(());
    }
    feedback.stage(&route, "approving token");
    execute_actions(
        chains,
        signer,
        &plan.from,
        &required,
        "intent stress approval",
        feedback,
    )
    .await?;
    Ok(())
}

fn stress_approval_action(plan: &LegPlan, signer: Address, amount: U256) -> Action {
    Action {
        id: "axe-stress-approval".to_owned(),
        label: "Approve bounded intent stress volume".to_owned(),
        kind: ActionKind::Approval,
        chain: plan.from.id.chain_id.clone(),
        payload: ActionPayload::EvmTransaction(EvmTransactionPayload {
            from: Some(signer.to_string()),
            to: plan.from.id.token_address.clone(),
            data: format!(
                "0x{}",
                hex::encode(
                    ERC20::approveCall {
                        spender: plan.settlement_contract,
                        amount,
                    }
                    .abi_encode()
                )
            ),
            value: "0".to_owned(),
            gas_limit: None,
            max_fee_per_gas: None,
            max_priority_fee_per_gas: None,
        }),
    }
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

fn validate_deposit_action(action: &Action, source: &WalletAsset, amount: U256) -> Result<Address> {
    let payload = action_payload(action)?;
    let target = payload
        .to
        .parse::<Address>()
        .wrap_err_with(|| format!("action '{}' has an invalid target", action.id))?;
    let value = U256::from_str(&payload.value)
        .wrap_err_with(|| format!("action '{}' has an invalid value", action.id))?;
    let expected_value = if source.native { amount } else { U256::ZERO };
    if value != expected_value || payload.data == "0x" || payload.data.is_empty() {
        return Err(eyre!(
            "action '{}' deposit value or calldata does not match the quoted input",
            action.id
        ));
    }
    Ok(target)
}

pub(super) fn validate_quote_actions(
    quote: &Quote,
    source: &WalletAsset,
    input_amount: U256,
) -> Result<Address> {
    let deposits = quote
        .actions
        .iter()
        .filter(|action| action.kind == ActionKind::Transaction)
        .collect::<Vec<_>>();
    if deposits.len() != 1 {
        return Err(eyre!(
            "intent quote {} returned {} transaction actions; expected exactly one deposit",
            quote.quote_id,
            deposits.len()
        ));
    }
    let settlement = validate_deposit_action(deposits[0], source, input_amount)?;
    let approvals = quote
        .actions
        .iter()
        .filter(|action| action.kind == ActionKind::Approval)
        .collect::<Vec<_>>();
    if source.native {
        if !approvals.is_empty() {
            return Err(eyre!("native-token quote unexpectedly requested approval"));
        }
        return Ok(settlement);
    }
    if approvals.len() > 1 {
        return Err(eyre!(
            "token quote returned {} approval actions; expected at most one",
            approvals.len()
        ));
    }
    if let Some(approval) = approvals.first() {
        let requirement = approval_requirement(approval, source, input_amount)?;
        if requirement.spender != settlement {
            return Err(eyre!(
                "intent approval spender does not match the deposit contract"
            ));
        }
    }
    Ok(settlement)
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
                    if feedback.is_debugger() {
                        ui::section("intent status response");
                        println!("{}", serde_json::to_string_pretty(&status)?);
                    } else if feedback.is_detailed() {
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
                        validate_status_output(&output, expected_output, quote)?;
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

fn validate_status_output(
    output: &StatusOutput,
    expected: &WalletAsset,
    quote: &Quote,
) -> Result<()> {
    if output.chain != expected.id.chain_id
        || !output
            .token
            .eq_ignore_ascii_case(&expected.id.token_address)
    {
        return Err(eyre!(
            "RFQ DONE output does not match the requested destination asset"
        ));
    }
    let promised = quote
        .output
        .minimum_amount
        .as_deref()
        .unwrap_or(&quote.output.amount);
    if parse_amount(&output.amount)? < parse_amount(promised)? {
        return Err(eyre!("RFQ DONE output is below the quoted minimum"));
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
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    use super::super::types::{
        AssetId, Backend, BackendType, Fees, QuoteInput, QuoteOutput, SelectionReason, Validity,
    };

    #[tokio::test]
    async fn startup_feedback_updates_stage_without_printing_recovery_warnings() {
        let progress = ProgressBar::hidden();
        let feedback = ExecutionFeedback::Startup(progress.clone());
        let warnings = Arc::new(AtomicU64::new(0));
        ui::count_warnings(Arc::clone(&warnings), async {
            feedback.stage("Base Sepolia/USDC", "checking allowance");
            feedback.warn("temporary RPC failure");
        })
        .await;

        assert_eq!(progress.message(), "Base Sepolia/USDC · checking allowance");
        assert_eq!(warnings.load(Ordering::Relaxed), 1);
        assert!(!feedback.is_detailed());
    }

    fn evm_action(
        id: &str,
        kind: ActionKind,
        chain: &str,
        from: Address,
        to: Address,
        data: String,
    ) -> Action {
        Action {
            id: id.into(),
            label: id.into(),
            kind,
            chain: chain.into(),
            payload: ActionPayload::EvmTransaction(EvmTransactionPayload {
                from: Some(from.to_string()),
                to: to.to_string(),
                data,
                value: "0".into(),
                gas_limit: None,
                max_fee_per_gas: None,
                max_priority_fee_per_gas: None,
            }),
        }
    }

    fn token_asset(chain_id: &str, token: Address) -> WalletAsset {
        WalletAsset {
            id: AssetId {
                chain_id: chain_id.to_owned(),
                token_address: token.to_string(),
            },
            chain_label: chain_id.to_owned(),
            symbol: "USDC".to_owned(),
            decimals: 6,
            balance: U256::from(1_000_000u64),
            native: false,
        }
    }

    fn quote_with_actions(actions: Vec<Action>) -> Quote {
        Quote {
            quote_id: "quote-id".to_owned(),
            selection_reason: SelectionReason::BestAvailable,
            backend: Backend {
                kind: BackendType::Intent,
                name: "solver".to_owned(),
                tracking: serde_json::json!({}),
                metadata: serde_json::json!({}),
            },
            estimated_time_seconds: 10,
            validity: Validity {
                kind: "expires_at".to_owned(),
                quote_expires_at: Utc::now() + chrono::Duration::minutes(1),
                fulfillment_deadline: None,
            },
            input: QuoteInput {
                chain: "eip155:1".to_owned(),
                token: Address::from([1u8; 20]).to_string(),
                amount: "100".to_owned(),
                amount_usd_approx: None,
            },
            output: QuoteOutput {
                chain: "eip155:2".to_owned(),
                token: Address::from([4u8; 20]).to_string(),
                amount: "95".to_owned(),
                minimum_amount: Some("90".to_owned()),
                amount_usd_approx: None,
            },
            fees: Fees::default(),
            actions,
        }
    }

    #[test]
    fn rejects_an_action_for_another_chain() {
        let action = evm_action(
            "deposit",
            ActionKind::Transaction,
            "eip155:2",
            Address::ZERO,
            Address::ZERO,
            "0x".into(),
        );
        let error = action_request(&action, Address::ZERO, "eip155:1").unwrap_err();
        assert!(error.to_string().contains("expected eip155:1"));
    }

    #[test]
    fn accepts_a_matching_evm_action() {
        let signer = "0x0000000000000000000000000000000000000001"
            .parse::<Address>()
            .unwrap();
        let action = evm_action(
            "deposit",
            ActionKind::Transaction,
            "eip155:43113",
            signer,
            "0x0000000000000000000000000000000000000002"
                .parse()
                .unwrap(),
            "0x1234".into(),
        );
        let request = action_request(&action, signer, "eip155:43113").unwrap();
        assert_eq!(request.chain_id, Some(43_113));
    }

    #[test]
    fn parses_the_allowance_required_by_an_approval_action() {
        let token = Address::from([1u8; 20]);
        let spender = Address::from([2u8; 20]);
        let amount = U256::from(1_000_000u64);
        let data = ERC20::approveCall { spender, amount }.abi_encode();
        let action = evm_action(
            "approve",
            ActionKind::Approval,
            "eip155:1",
            Address::ZERO,
            token,
            format!("0x{}", hex::encode(data)),
        );
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
    fn rewrites_a_validated_approval_to_the_bounded_amount() {
        let token = Address::from([1u8; 20]);
        let spender = Address::from([2u8; 20]);
        let amount = U256::from(1_000_000u64);
        let action = evm_action(
            "approve",
            ActionKind::Approval,
            "eip155:1",
            Address::ZERO,
            token,
            format!(
                "0x{}",
                hex::encode(ERC20::approveCall { spender, amount }.abi_encode())
            ),
        );
        let requirement = ApprovalRequirement { spender, amount };

        let bounded_amount = amount + U256::from(500_000u64);
        let bounded = approval_action(&action, requirement, bounded_amount).unwrap();
        let payload = action_payload(&bounded).unwrap();
        let calldata = ERC20::approveCall::abi_decode(
            &hex::decode(payload.data.strip_prefix("0x").unwrap()).unwrap(),
        )
        .unwrap();

        assert_eq!(calldata.spender, spender);
        assert_eq!(calldata.amount, bounded_amount);
        assert_eq!(payload.to, token.to_string());
        assert_eq!(bounded.chain, action.chain);
    }

    #[test]
    fn rejects_an_approval_spender_that_is_not_the_deposit_contract() {
        let token = Address::from([1u8; 20]);
        let settlement = Address::from([2u8; 20]);
        let other_spender = Address::from([3u8; 20]);
        let source = token_asset("eip155:1", token);
        let approval = evm_action(
            "approve",
            ActionKind::Approval,
            "eip155:1",
            Address::ZERO,
            token,
            format!(
                "0x{}",
                hex::encode(
                    ERC20::approveCall {
                        spender: other_spender,
                        amount: U256::from(100),
                    }
                    .abi_encode()
                )
            ),
        );
        let deposit = evm_action(
            "deposit",
            ActionKind::Transaction,
            "eip155:1",
            Address::ZERO,
            settlement,
            "0x1234".to_owned(),
        );

        let error = validate_quote_actions(
            &quote_with_actions(vec![approval, deposit]),
            &source,
            U256::from(100),
        )
        .unwrap_err();

        assert!(error.to_string().contains("does not match"));
    }

    #[test]
    fn rejects_fulfillment_below_the_quoted_minimum() {
        let quote = quote_with_actions(Vec::new());
        let expected = token_asset("eip155:2", Address::from([4u8; 20]));
        let output = StatusOutput {
            chain: "eip155:2".to_owned(),
            token: Address::from([4u8; 20]).to_string(),
            amount: "89".to_owned(),
        };

        assert!(validate_status_output(&output, &expected, &quote).is_err());
        assert!(
            validate_status_output(
                &StatusOutput {
                    amount: "90".to_owned(),
                    ..output
                },
                &expected,
                &quote
            )
            .is_ok()
        );
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
            context: "1.5 i/m · 1 err · avg 1m15s".to_owned(),
        };

        feedback.stage(
            "Arbitrum Sepolia/ETH -> Base Sepolia/ETH",
            "submitting deposit",
        );
        assert_eq!(
            progress.message(),
            "0 intents · 1.5 i/m · 1 err · avg 1m15s · deposit"
        );

        feedback.leg_completed("Arbitrum Sepolia/ETH -> Base Sepolia/ETH");
        assert_eq!(progress.position(), 1);
        assert!(
            progress
                .message()
                .starts_with("1 intents · 1.5 i/m · 1 err")
        );

        feedback.warn("pending state unavailable; using latest-pinned nonce+gas immediately");
        assert!(progress.message().starts_with(
            "1 intents · warning: pending state unavailable; using latest-pinned nonce+gas immed…"
        ));
    }

    #[test]
    fn automatic_execution_rejects_quotes_above_the_wallet_limit() {
        assert!(validate_input_limit(U256::from(100), Some(U256::from(100))).is_ok());
        assert!(validate_input_limit(U256::from(101), Some(U256::from(100))).is_err());
        assert!(validate_input_limit(U256::MAX, None).is_ok());
    }
}
