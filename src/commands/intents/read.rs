use std::collections::HashMap;
use std::time::{Duration, Instant};

use alloy::primitives::{Address, U256};
use chrono::Utc;
use comfy_table::{Cell, Color};
use eyre::{Result, bail, eyre};
use serde_json::json;

use super::client::RfqClient;
use super::presentation::asset_table;
use super::types::{
    AssetSpec, CatalogChain, CatalogResponse, CatalogToken, EvmTransactionPayload, HumanAmount,
    LegPlan, OrderType, Quote, QuoteRequest, StatusResponse, TokenInfo, TokensResponse,
    TransferState, format_units, is_native_token, parse_amount,
};
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

#[derive(Clone)]
pub struct QuoteRequestArgs {
    pub from: AssetSpec,
    pub to: AssetSpec,
    pub amount: HumanAmount,
    pub sender: Address,
    pub recipient: Address,
    pub order_type: OrderType,
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

pub async fn catalog(args: CatalogArgs) -> Result<()> {
    let client = api_client(&args.api)?;
    let (chains, tokens) = tokio::try_join!(client.chains(), client.tokens())?;
    let response = merge_catalog(chains.chains, tokens.tokens, args.chain.as_deref())?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&response)?);
    } else {
        render_catalog(&response);
    }
    Ok(())
}

pub(super) fn merge_catalog(
    mut chains: Vec<super::types::ChainInfo>,
    tokens: Vec<TokenInfo>,
    chain_filter: Option<&str>,
) -> Result<CatalogResponse> {
    if let Some(chain_filter) = chain_filter {
        chains.retain(|chain| chain.chain_id == chain_filter);
        if chains.is_empty() {
            bail!(
                "Chain {chain_filter} is not in the intent catalog. Run without --chain to list available chains."
            );
        }
    }
    chains.sort_by(|left, right| {
        (&left.chain_label, &left.chain_id).cmp(&(&right.chain_label, &right.chain_id))
    });
    let mut tokens_by_chain = group_tokens(tokens);
    let chains = chains
        .into_iter()
        .map(|chain| CatalogChain {
            tokens: tokens_by_chain
                .remove(&chain.chain_id)
                .unwrap_or_default()
                .into_iter()
                .map(CatalogToken::from)
                .collect(),
            chain,
        })
        .collect();
    Ok(CatalogResponse { chains })
}

fn group_tokens(tokens: Vec<TokenInfo>) -> HashMap<String, Vec<TokenInfo>> {
    let mut tokens_by_chain = HashMap::<String, Vec<TokenInfo>>::new();
    for token in tokens {
        tokens_by_chain
            .entry(token.chain_id.clone())
            .or_default()
            .push(token);
    }
    for tokens in tokens_by_chain.values_mut() {
        tokens.sort_by(|left, right| {
            (&left.symbol, &left.address).cmp(&(&right.symbol, &right.address))
        });
    }
    tokens_by_chain
}

pub(super) fn render_planned_quote(plan: &LegPlan, json: bool) -> Result<()> {
    let response = json!({ "quotes": [&plan.quote.quote] });
    if json {
        let output = json!({
            "request": &plan.request,
            "response": response,
            "latencyMs": duration_ms(plan.quote.latency),
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(());
    }

    let from = TokenInfo {
        chain_id: plan.from.id.chain_id.clone(),
        address: plan.from.id.token_address.clone(),
        symbol: plan.from.symbol.clone(),
        decimals: plan.from.decimals,
    };
    let to = TokenInfo {
        chain_id: plan.to.id.chain_id.clone(),
        address: plan.to.id.token_address.clone(),
        symbol: plan.to.symbol.clone(),
        decimals: plan.to.decimals,
    };
    render_quote(&plan.quote.quote, plan.quote.latency, &from, &to)?;
    ui::section("quote request");
    println!("{}", serde_json::to_string_pretty(&plan.request)?);
    ui::section("quote response");
    println!("{}", serde_json::to_string_pretty(&response)?);
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

pub(super) fn prepare_quote_from_tokens(
    tokens: &TokensResponse,
    args: &QuoteRequestArgs,
) -> Result<PreparedQuote> {
    let from = find_token(tokens, &args.from)?;
    let to = find_token(tokens, &args.to)?;
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
                "Asset {asset} is not in the intent token catalog. Run `axe intents catalog` to list supported assets."
            )
        })
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

fn render_catalog(catalog: &CatalogResponse) {
    ui::section("intent catalog");
    let token_count: usize = catalog.chains.iter().map(|chain| chain.tokens.len()).sum();
    ui::kv("supported chains", &catalog.chains.len().to_string());
    ui::kv("supported tokens", &token_count.to_string());
    for entry in &catalog.chains {
        println!();
        println!(
            "  {}  ·  {}  ·  {} assets  ·  {}",
            entry.chain.chain_label,
            entry.chain.chain_id,
            entry.tokens.len(),
            entry.chain.chain_type.to_ascii_uppercase()
        );
        if entry.tokens.is_empty() {
            ui::info("No assets advertised.");
            continue;
        }
        let mut table = asset_table(&["Asset", "Kind", "Address", "Decimals"]);
        for token in &entry.tokens {
            let native = is_native_token(&token.address);
            table.add_row(vec![
                Cell::new(&token.symbol).fg(Color::Cyan),
                Cell::new(if native { "native" } else { "token" }),
                Cell::new(if native { "—" } else { &token.address }),
                Cell::new(token.decimals),
            ]);
        }
        println!("{table}");
    }
}

fn render_quote(quote: &Quote, latency: Duration, from: &TokenInfo, to: &TokenInfo) -> Result<()> {
    ui::section("intent quote");
    ui::kv("quote ID", &quote.quote_id);
    if let Some(swap_id) = quote.backend.tracking.swap_id.as_deref() {
        ui::kv("swap ID", swap_id);
    }
    ui::kv(
        "route",
        &format!(
            "{}/{} -> {}/{}",
            from.chain_id, from.symbol, to.chain_id, to.symbol
        ),
    );
    ui::kv(
        "input",
        &format!(
            "{} {}",
            format_units(parse_amount(&quote.input.amount)?, from.decimals),
            from.symbol
        ),
    );
    ui::kv(
        "output",
        &format!(
            "{} {}",
            format_units(parse_amount(&quote.output.amount)?, to.decimals),
            to.symbol
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

    fn chain(id: &str, label: &str) -> super::super::types::ChainInfo {
        super::super::types::ChainInfo {
            chain_id: id.into(),
            chain_label: label.into(),
            chain_type: "evm".into(),
        }
    }

    fn token(chain_id: &str, symbol: &str, address: &str) -> TokenInfo {
        TokenInfo {
            chain_id: chain_id.into(),
            address: address.into(),
            symbol: symbol.into(),
            decimals: 6,
        }
    }

    #[test]
    fn catalog_nests_sorted_tokens_under_sorted_chains() {
        let catalog = merge_catalog(
            vec![chain("eip155:2", "Zulu"), chain("eip155:1", "Alpha")],
            vec![
                token("eip155:1", "USDC", "0x2"),
                token("eip155:1", "ETH", "0x1"),
                token("eip155:2", "USDC", "0x3"),
            ],
            None,
        )
        .unwrap();

        assert_eq!(catalog.chains[0].chain.chain_label, "Alpha");
        assert_eq!(catalog.chains[0].tokens[0].symbol, "ETH");
        assert_eq!(catalog.chains[0].tokens[1].symbol, "USDC");
        assert_eq!(catalog.chains[1].chain.chain_label, "Zulu");
        assert_eq!(
            serde_json::to_value(&catalog).unwrap()["chains"][0]["chainId"],
            "eip155:1"
        );
        assert!(
            serde_json::to_value(&catalog).unwrap()["chains"][0]["tokens"][0]
                .get("chainId")
                .is_none()
        );
    }

    #[test]
    fn catalog_filter_explains_how_to_find_a_valid_chain() {
        let error = merge_catalog(
            vec![chain("eip155:1", "Ethereum")],
            Vec::new(),
            Some("eip155:2"),
        )
        .unwrap_err();

        assert_eq!(
            error.to_string(),
            "Chain eip155:2 is not in the intent catalog. Run without --chain to list available chains."
        );
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
