use std::path::PathBuf;
use std::time::{Duration, Instant};

use alloy::primitives::{Address, U256};
use chrono::Utc;
use eyre::{Result, bail, eyre};
use serde_json::json;

use super::client::RfqClient;
use super::route::{DiscoveryFeedback, discover_wallet, plan_sweep, validate_quote_route};
use super::types::{
    AssetSpec, ChainInfo, EvmTransactionPayload, HumanAmount, OrderType, Quote, QuoteOutcome,
    QuoteRequest, RoutePlan, StatusResponse, TokenInfo, TokensResponse, TransferState,
    format_units, parse_amount,
};
use crate::config::ChainsConfig;
use crate::types::Network;
use crate::ui;

pub struct ApiArgs {
    pub network: Network,
    pub api_url: Option<String>,
}

pub struct CatalogArgs {
    pub api: ApiArgs,
    pub chain: Option<String>,
    pub json: bool,
}

pub struct RoutesArgs {
    pub api: ApiArgs,
    pub config: PathBuf,
    pub wallet: Address,
    pub wallet_bps: u16,
    pub order_type: OrderType,
    pub json: bool,
}

#[derive(Clone)]
pub struct QuoteRequestArgs {
    pub from: AssetSpec,
    pub to: AssetSpec,
    pub amount: HumanAmount,
    pub sender: Address,
    pub recipient: Address,
    pub order_type: OrderType,
}

pub struct QuoteArgs {
    pub api: ApiArgs,
    pub request: QuoteRequestArgs,
    pub json: bool,
}

pub struct StatusArgs {
    pub api: ApiArgs,
    pub quote_id: String,
    pub watch: bool,
    pub poll_interval: Duration,
    pub timeout: Duration,
    pub json: bool,
}

pub(super) struct PreparedQuote {
    pub request: QuoteRequest,
    pub from: TokenInfo,
    pub to: TokenInfo,
    pub requested_amount: U256,
    pub order_type: OrderType,
}

pub async fn catalog_chains(args: CatalogArgs) -> Result<()> {
    let client = api_client(&args.api)?;
    let mut response = client.chains().await?;
    response
        .chains
        .sort_by(|left, right| left.chain_id.cmp(&right.chain_id));
    if args.json {
        println!("{}", serde_json::to_string_pretty(&response)?);
    } else {
        render_chains(&response.chains);
    }
    Ok(())
}

pub async fn catalog_tokens(args: CatalogArgs) -> Result<()> {
    let client = api_client(&args.api)?;
    let mut response = client.tokens().await?;
    if let Some(chain) = args.chain {
        response.tokens.retain(|token| token.chain_id == chain);
    }
    response.tokens.sort_by(|left, right| {
        (&left.chain_id, &left.symbol, &left.address).cmp(&(
            &right.chain_id,
            &right.symbol,
            &right.address,
        ))
    });
    if args.json {
        println!("{}", serde_json::to_string_pretty(&response)?);
    } else {
        render_tokens(&response.tokens);
    }
    Ok(())
}

pub async fn routes(args: RoutesArgs) -> Result<()> {
    let client = api_client(&args.api)?;
    let config = ChainsConfig::load(&args.config).await?;
    let discovery = discover_wallet(
        &client,
        &config,
        args.wallet,
        if args.json {
            DiscoveryFeedback::Quiet
        } else {
            DiscoveryFeedback::Detailed
        },
    )
    .await?;
    let plans = plan_sweep(
        &client,
        &discovery,
        args.wallet,
        args.wallet_bps,
        args.order_type,
    )
    .await;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&routes_json(&plans))?);
    } else {
        render_routes(&plans);
    }
    Ok(())
}

pub async fn quote(args: QuoteArgs) -> Result<()> {
    let client = api_client(&args.api)?;
    let prepared = prepare_quote(&client, &args.request).await?;
    let timed = require_quote(&client, &prepared.request).await?;
    validate_quote_route(
        &timed.quote,
        args.request.from.id(),
        args.request.to.id(),
        prepared.order_type,
        prepared.requested_amount,
    )?;
    if args.json {
        let value = json!({
            "latencyMs": duration_ms(timed.latency),
            "quote": timed.quote,
        });
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        render_quote(&timed.quote, timed.latency, &prepared)?;
    }
    Ok(())
}

pub async fn status(args: StatusArgs) -> Result<()> {
    let client = api_client(&args.api)?;
    if !args.watch {
        let response = checked_status(&client, &args.quote_id).await?;
        return render_status(&response, args.json);
    }
    watch_status(&client, args).await
}

pub(super) fn api_client(args: &ApiArgs) -> Result<RfqClient> {
    RfqClient::new(args.network, args.api_url.as_deref())
}

pub(super) async fn prepare_quote(
    client: &RfqClient,
    args: &QuoteRequestArgs,
) -> Result<PreparedQuote> {
    let tokens = client.tokens().await?;
    let from = find_token(&tokens, &args.from)?;
    let to = find_token(&tokens, &args.to)?;
    let decimals = match args.order_type {
        OrderType::ExactInput => from.decimals,
        OrderType::ExactOutput => to.decimals,
    };
    let requested_amount = args.amount.to_base_units(decimals)?;
    if requested_amount.is_zero() {
        bail!("Use an amount greater than zero");
    }
    let request = QuoteRequest {
        from_chain: args.from.id().chain_id.clone(),
        from_token: args.from.id().token_address.clone(),
        to_chain: args.to.id().chain_id.clone(),
        to_token: args.to.id().token_address.clone(),
        amount: requested_amount.to_string(),
        order_type: args.order_type,
        sender: args.sender.to_string(),
        recipient: args.recipient.to_string(),
    };
    Ok(PreparedQuote {
        request,
        from,
        to,
        requested_amount,
        order_type: args.order_type,
    })
}

fn find_token(tokens: &TokensResponse, asset: &AssetSpec) -> Result<TokenInfo> {
    tokens
        .tokens
        .iter()
        .find(|token| {
            token.chain_id == asset.id().chain_id
                && token
                    .address
                    .eq_ignore_ascii_case(&asset.id().token_address)
        })
        .cloned()
        .ok_or_else(|| {
            eyre!(
                "Asset {asset} is not in the intent token catalog. Run `axe intents catalog tokens` to list supported assets."
            )
        })
}

async fn require_quote(
    client: &RfqClient,
    request: &QuoteRequest,
) -> Result<super::types::TimedQuote> {
    match client.quote(request).await? {
        QuoteOutcome::Available(quote) => Ok(*quote),
        QuoteOutcome::Unavailable(reason) => Err(eyre!(
            "No intent quote is available: {reason}. Check the route, amount, and solver liquidity."
        )),
    }
}

async fn checked_status(client: &RfqClient, quote_id: &str) -> Result<StatusResponse> {
    let response = client.status(quote_id).await?;
    if response.quote_id != quote_id {
        bail!("RFQ status returned a different quote ID");
    }
    Ok(response)
}

async fn watch_status(client: &RfqClient, args: StatusArgs) -> Result<()> {
    let started = Instant::now();
    let mut last_state = None;
    loop {
        let response = checked_status(client, &args.quote_id).await?;
        ensure_watchable_status(&response)?;
        if last_state != Some(response.state) {
            if !args.json {
                ui::kv("status", response.state.label());
            }
            last_state = Some(response.state);
        }
        if response.state.is_terminal() {
            return render_status(&response, args.json);
        }
        if started.elapsed() >= args.timeout {
            bail!(
                "Quote {} did not reach a terminal state within {}. Increase --timeout-secs or run the command again.",
                args.quote_id,
                ui::format_duration(args.timeout)
            );
        }
        tokio::time::sleep(args.poll_interval).await;
    }
}

fn ensure_watchable_status(status: &StatusResponse) -> Result<()> {
    if status.state == TransferState::NotFound {
        bail!(
            "Quote {} was not found. Check the quote ID and selected network.",
            status.quote_id
        );
    }
    Ok(())
}

fn render_chains(chains: &[ChainInfo]) {
    ui::section("intent chains");
    ui::kv("chains", &chains.len().to_string());
    for chain in chains {
        println!(
            "  {:<24} {:<10} {}",
            chain.chain_id, chain.chain_type, chain.chain_label
        );
    }
}

fn render_tokens(tokens: &[TokenInfo]) {
    ui::section("intent tokens");
    ui::kv("tokens", &tokens.len().to_string());
    for token in tokens {
        println!(
            "  {:<10} {:<18} {}  ({} decimals)",
            token.symbol, token.chain_id, token.address, token.decimals
        );
    }
}

fn render_routes(plans: &[RoutePlan]) {
    ui::section("intent routes");
    ui::kv("quotable round trips", &plans.len().to_string());
    for plan in plans {
        ui::kv(
            "route",
            &format!(
                "{} -> {} -> {}",
                plan.from.label(),
                plan.to.label(),
                plan.from.label()
            ),
        );
        ui::kv(
            "input / expected return",
            &format!(
                "{} / {} {}",
                format_units(plan.input_amount, plan.from.decimals),
                format_units(plan.expected_return, plan.from.decimals),
                plan.from.symbol
            ),
        );
        ui::kv(
            "quote latency",
            &format!(
                "forward {} │ reverse {}",
                ui::format_millis(plan.forward_quote_ms),
                ui::format_millis(plan.reverse_quote_ms)
            ),
        );
    }
}

fn routes_json(plans: &[RoutePlan]) -> serde_json::Value {
    let routes = plans
        .iter()
        .map(|plan| {
            json!({
                "from": plan.from.id.to_string(),
                "to": plan.to.id.to_string(),
                "orderType": plan.order_type,
                "requestedAmount": plan.requested_amount.to_string(),
                "inputAmount": plan.input_amount.to_string(),
                "expectedReturn": plan.expected_return.to_string(),
                "forwardQuoteLatencyMs": plan.forward_quote_ms,
                "reverseQuoteLatencyMs": plan.reverse_quote_ms,
            })
        })
        .collect::<Vec<_>>();
    json!({ "routes": routes })
}

fn render_quote(quote: &Quote, latency: Duration, prepared: &PreparedQuote) -> Result<()> {
    ui::section("intent quote");
    ui::kv("quote ID", &quote.quote_id);
    if let Some(swap_id) = quote.backend.tracking.swap_id.as_deref() {
        ui::kv("swap ID", swap_id);
    }
    ui::kv(
        "route",
        &format!("{} -> {}", prepared.from.symbol, prepared.to.symbol),
    );
    ui::kv(
        "input",
        &format!(
            "{} {}",
            format_units(parse_amount(&quote.input.amount)?, prepared.from.decimals),
            prepared.from.symbol
        ),
    );
    ui::kv(
        "output",
        &format!(
            "{} {}",
            format_units(parse_amount(&quote.output.amount)?, prepared.to.decimals),
            prepared.to.symbol
        ),
    );
    ui::kv("quote latency", &ui::format_duration(latency));
    ui::kv(
        "quote expires in",
        &remaining_until(quote.validity.quote_expires_at),
    );
    if let Some(deadline) = quote.validity.fulfillment_deadline {
        ui::kv("fulfillment deadline", &remaining_until(deadline));
    }
    render_actions(quote);
    Ok(())
}

fn render_actions(quote: &Quote) {
    ui::kv("actions", &quote.actions.len().to_string());
    for action in &quote.actions {
        let target = serde_json::from_value::<EvmTransactionPayload>(action.payload.clone())
            .map(|payload| payload.to)
            .unwrap_or_else(|_| "unknown target".to_owned());
        println!("  {:<12} {:<18} {}", action.kind, action.chain, target);
    }
}

fn remaining_until(deadline: chrono::DateTime<Utc>) -> String {
    (deadline - Utc::now())
        .to_std()
        .map(ui::format_duration)
        .unwrap_or_else(|_| "expired".to_owned())
}

fn render_status(status: &StatusResponse, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(status)?);
        return Ok(());
    }
    ui::section("intent status");
    ui::kv("quote ID", &status.quote_id);
    ui::kv("status", status.state.label());
    if let Some(destination) = &status.destination {
        ui::tx_hash("destination", &destination.tx_hash);
    }
    if let Some(output) = &status.output {
        ui::kv(
            "output",
            &format!("{} {} on {}", output.amount, output.token, output.chain),
        );
    }
    if let Some(refund) = &status.refund {
        ui::kv(
            "refund",
            &format!("{} {} on {}", refund.amount, refund.token, refund.chain),
        );
        ui::tx_hash("refund transaction", &refund.tx_hash);
    }
    Ok(())
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_route_discovery_is_a_valid_result() {
        assert_eq!(routes_json(&[]), json!({ "routes": [] }));
    }

    #[test]
    fn watching_an_unknown_quote_fails_immediately() {
        let status = StatusResponse {
            quote_id: "missing-quote".into(),
            state: TransferState::NotFound,
            destination: None,
            output: None,
            refund: None,
        };

        assert_eq!(
            ensure_watchable_status(&status).unwrap_err().to_string(),
            "Quote missing-quote was not found. Check the quote ID and selected network."
        );
    }
}
