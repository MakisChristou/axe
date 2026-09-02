use std::collections::{HashMap, HashSet};

use alloy::primitives::{Address, U256, U512};
use alloy::providers::Provider;
use eyre::{Result, WrapErr, eyre};
use rand::seq::SliceRandom;

use super::RouteChoice;
use super::client::RfqClient;
use super::types::{
    AssetId, AssetSpec, AssetType, ChainInfo, ChainRuntime, HumanAmount, LegPlan, OrderType, Quote,
    QuoteOutcome, QuoteRequest, RoundTripInputBudget, RoutePlan, TokenInfo, WalletAsset,
    format_units, is_native_token, parse_amount,
};
use crate::config::{ChainConfig, ChainsConfig};
use crate::evm::{ERC20, EvmEndpoints};
use crate::retry::retry_with_fallback_all;
use crate::ui;

const BPS_DENOMINATOR: u64 = 10_000;
const EXACT_OUTPUT_HEADROOM_BPS: u64 = 9_500;
const EXACT_OUTPUT_CALIBRATION_ATTEMPTS: usize = 3;
const GAS_RESERVE_WEI: u64 = 1_000_000_000_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IntentRpcOverride {
    EthereumSepolia,
    ArbitrumSepolia,
    BaseSepolia,
    AvalancheFuji,
}

struct IntentRpcCandidate {
    source: String,
    url: String,
}

impl IntentRpcOverride {
    const fn for_chain_id(chain_id: u64) -> Option<Self> {
        match chain_id {
            11_155_111 => Some(Self::EthereumSepolia),
            421_614 => Some(Self::ArbitrumSepolia),
            84_532 => Some(Self::BaseSepolia),
            43_113 => Some(Self::AvalancheFuji),
            _ => None,
        }
    }

    const fn env_name(self) -> &'static str {
        match self {
            Self::EthereumSepolia => "SEP_RPC_URL",
            Self::ArbitrumSepolia => "ARB_SEPOLIA_RPC_URL",
            Self::BaseSepolia => "BASE_SEPOLIA_RPC_URL",
            Self::AvalancheFuji => "FUJI_RPC_URL",
        }
    }

    fn url(self) -> Option<String> {
        std::env::var(self.env_name())
            .ok()
            .filter(|url| !url.trim().is_empty())
    }
}

pub struct RouteDiscovery {
    pub chains: HashMap<String, ChainRuntime>,
    pub assets: Vec<WalletAsset>,
}

#[derive(Clone, Copy)]
pub enum PlanningFeedback {
    Visible,
    Hidden,
}

#[derive(Clone, Copy)]
pub enum DiscoveryFeedback {
    Detailed,
    Quiet,
}

impl DiscoveryFeedback {
    fn warn(self, message: &str) {
        if matches!(self, Self::Detailed) {
            ui::warn(message);
        }
    }
}

pub async fn discover_wallet(
    client: &RfqClient,
    config: &ChainsConfig,
    wallet: Address,
    feedback: DiscoveryFeedback,
) -> Result<RouteDiscovery> {
    let catalog_chains = client.chains().await?.chains;
    let catalog_tokens = client.tokens().await?.tokens;
    let chains = resolve_evm_chains(config, catalog_chains, feedback).await?;
    let assets = read_wallet_assets(&chains, catalog_tokens, wallet, feedback).await;
    if assets.is_empty() {
        return Err(eyre!(
            "RFQ catalog has no EVM tokens whose chains resolve through the axe config"
        ));
    }
    if matches!(feedback, DiscoveryFeedback::Detailed) {
        render_wallet_stats(wallet, &chains, &assets);
    }
    Ok(RouteDiscovery { chains, assets })
}

pub(super) async fn resolve_evm_chains(
    config: &ChainsConfig,
    catalog: Vec<ChainInfo>,
    feedback: DiscoveryFeedback,
) -> Result<HashMap<String, ChainRuntime>> {
    let mut resolved = HashMap::new();
    for chain in catalog
        .into_iter()
        .filter(|chain| chain.chain_type == "evm")
    {
        let Some(reference) = chain.chain_id.strip_prefix("eip155:") else {
            continue;
        };
        let Ok(chain_id) = reference.parse::<u64>() else {
            feedback.warn(&format!(
                "skipping malformed RFQ chain ID {}",
                chain.chain_id
            ));
            continue;
        };
        let candidates = evm_chain_candidates(config, chain_id, &chain.chain_label);
        if candidates.is_empty() {
            feedback.warn(&format!(
                "{} is advertised by RFQ but absent from the axe chains config",
                chain.chain_id
            ));
            continue;
        }
        let Some(rpc_url) = first_working_rpc(&chain, chain_id, candidates, feedback).await else {
            feedback.warn(&format!(
                "{} has no usable RPC in the axe chains config",
                chain.chain_id
            ));
            continue;
        };
        resolved.insert(
            chain.chain_id.clone(),
            ChainRuntime {
                label: chain.chain_label,
                rpc_url,
            },
        );
    }
    Ok(resolved)
}

fn evm_chain_candidates<'a>(
    config: &'a ChainsConfig,
    chain_id: u64,
    label: &str,
) -> Vec<(&'a str, &'a ChainConfig)> {
    let normalized_label = normalize_chain_name(label);
    let mut candidates: Vec<_> = config
        .chains
        .iter()
        .filter(|(_, configured)| configured.evm_chain_id == Some(chain_id))
        .map(|(key, configured)| (key.as_str(), configured))
        .collect();
    candidates.sort_by_key(|(key, configured)| {
        let label_match = [
            *key,
            configured.name.as_deref().unwrap_or_default(),
            configured.axelar_id.as_deref().unwrap_or_default(),
        ]
        .into_iter()
        .any(|candidate| normalize_chain_name(candidate) == normalized_label);
        (!label_match, *key)
    });
    candidates
}

async fn first_working_rpc(
    chain: &ChainInfo,
    expected_chain_id: u64,
    candidates: Vec<(&str, &ChainConfig)>,
    feedback: DiscoveryFeedback,
) -> Option<String> {
    let rpc_override = IntentRpcOverride::for_chain_id(expected_chain_id)
        .and_then(|kind| kind.url().map(|url| (kind, url)));
    for candidate in intent_rpc_candidates(candidates, rpc_override) {
        if let Some(rpc_url) = validate_rpc_candidate(
            chain,
            expected_chain_id,
            &candidate.source,
            candidate.url,
            feedback,
        )
        .await
        {
            return Some(rpc_url);
        }
    }
    None
}

fn intent_rpc_candidates(
    configured: Vec<(&str, &ChainConfig)>,
    rpc_override: Option<(IntentRpcOverride, String)>,
) -> Vec<IntentRpcCandidate> {
    let mut candidates = Vec::new();
    if let Some((kind, url)) = rpc_override {
        candidates.push(IntentRpcCandidate {
            source: format!("override {}", kind.env_name()),
            url,
        });
    }
    candidates.extend(configured.into_iter().filter_map(|(key, chain)| {
        chain.rpc.clone().map(|url| IntentRpcCandidate {
            source: format!("config {key}"),
            url,
        })
    }));
    candidates
}

async fn validate_rpc_candidate(
    chain: &ChainInfo,
    expected_chain_id: u64,
    source: &str,
    rpc_url: String,
    feedback: DiscoveryFeedback,
) -> Option<String> {
    let endpoints = match EvmEndpoints::connect(std::slice::from_ref(&rpc_url)) {
        Ok(endpoints) => endpoints,
        Err(error) => {
            feedback.warn(&format!(
                "skipping RPC {source} for {}: {}",
                chain.chain_id,
                ui::scrub_urls(&error.to_string())
            ));
            return None;
        }
    };
    let provider = endpoints.providers().first()?;
    match provider.get_chain_id().await {
        Ok(actual_chain_id) if actual_chain_id == expected_chain_id => Some(rpc_url),
        Ok(actual_chain_id) => {
            feedback.warn(&format!(
                "skipping RPC {source}: expected {}, got eip155:{actual_chain_id}",
                chain.chain_id
            ));
            None
        }
        Err(error) => {
            feedback.warn(&format!(
                "skipping RPC {source} for {}: {}",
                chain.chain_id,
                ui::scrub_urls(&error.to_string())
            ));
            None
        }
    }
}

fn normalize_chain_name(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

pub(super) async fn read_wallet_assets(
    chains: &HashMap<String, ChainRuntime>,
    tokens: Vec<TokenInfo>,
    wallet: Address,
    feedback: DiscoveryFeedback,
) -> Vec<WalletAsset> {
    let mut assets = Vec::new();
    for token in tokens {
        let Some(chain) = chains.get(&token.chain_id) else {
            continue;
        };
        match read_balance(chain, &token, wallet).await {
            Ok(balance) => assets.push(WalletAsset {
                id: AssetId {
                    chain_id: token.chain_id,
                    token_address: token.address.clone(),
                },
                chain_label: chain.label.clone(),
                symbol: token.symbol,
                decimals: token.decimals,
                balance,
                native: is_native_token(&token.address),
            }),
            Err(error) => feedback.warn(&format!(
                "could not read {}/{}: {}",
                chain.label,
                token.symbol,
                ui::scrub_urls(&error.to_string())
            )),
        }
    }
    assets
}

async fn read_balance(chain: &ChainRuntime, token: &TokenInfo, wallet: Address) -> Result<U256> {
    let endpoints = EvmEndpoints::connect(std::slice::from_ref(&chain.rpc_url))?;
    if is_native_token(&token.address) {
        return retry_with_fallback_all(
            "intents native balance",
            endpoints.providers(),
            |provider| async move { provider.get_balance(wallet).await },
        )
        .await
        .map_err(Into::into);
    }
    let token_address: Address = token
        .address
        .parse()
        .wrap_err_with(|| format!("invalid EVM token address {}", token.address))?;
    retry_with_fallback_all(
        "intents token balance",
        endpoints.providers(),
        |provider| async move {
            ERC20::new(token_address, provider)
                .balanceOf(wallet)
                .call()
                .await
        },
    )
    .await
    .map_err(Into::into)
}

pub async fn plan_send(
    client: &RfqClient,
    discovery: &RouteDiscovery,
    signer: Address,
    recipient: Address,
    choice: &RouteChoice,
    feedback: PlanningFeedback,
) -> Result<LegPlan> {
    let plan = match choice {
        RouteChoice::Random {
            wallet_bps,
            order_type,
            asset_type,
        } => {
            random_send_plan(
                client,
                discovery,
                signer,
                recipient,
                *asset_type,
                *wallet_bps,
                *order_type,
            )
            .await?
        }
        RouteChoice::Explicit {
            from,
            to,
            amount,
            wallet_bps,
            order_type,
            asset_type,
        } => {
            let (from, to) = explicit_assets(discovery, from, to, *asset_type)?;
            ensure_source_ready(discovery, from)?;
            let amount = selected_amount(from, to, *order_type, amount.as_ref(), *wallet_bps)?;
            preflight_leg(client, from, to, signer, recipient, *order_type, amount)
                .await?
                .ok_or_else(|| eyre!("the solver returned no quote for the explicit route"))?
        }
    };
    if matches!(feedback, PlanningFeedback::Visible) {
        render_leg_plan(&plan);
    }
    Ok(plan)
}

pub async fn plan_roundtrip(
    client: &RfqClient,
    discovery: &RouteDiscovery,
    signer: Address,
    choice: &RouteChoice,
) -> Result<RoutePlan> {
    let plan = match choice {
        RouteChoice::Random {
            wallet_bps,
            order_type,
            asset_type,
        } => {
            random_roundtrip_plan(
                client,
                discovery,
                signer,
                *asset_type,
                *wallet_bps,
                *order_type,
            )
            .await?
        }
        RouteChoice::Explicit {
            from,
            to,
            amount,
            wallet_bps,
            order_type,
            asset_type,
        } => {
            let (from, to) = explicit_assets(discovery, from, to, *asset_type)?;
            ensure_roundtrip_ready(discovery, from, to)?;
            let input_budget = if amount.is_some() {
                RoundTripInputBudget::SpendableBalance
            } else {
                capped_input_budget(from, to, *wallet_bps)
                    .ok_or_else(|| eyre!("{} has no balance after the gas reserve", from.label()))?
            };
            let amount = selected_amount(from, to, *order_type, amount.as_ref(), *wallet_bps)?;
            preflight_pair(client, from, to, signer, *order_type, amount, input_budget)
                .await?
                .ok_or_else(|| eyre!("the solver returned no bidirectional quote for the route"))?
        }
    };
    render_plans(std::slice::from_ref(&plan));
    Ok(plan)
}

pub async fn plan_sweep(
    client: &RfqClient,
    discovery: &RouteDiscovery,
    signer: Address,
    asset_type: AssetType,
    wallet_bps: u16,
    order_type: OrderType,
    planning_feedback: PlanningFeedback,
) -> Vec<RoutePlan> {
    let mut plans = Vec::new();
    let mut seen = HashSet::new();
    let pairs = unordered_cross_chain_pairs(&discovery.assets, asset_type);
    let spinner = matches!(planning_feedback, PlanningFeedback::Visible).then(|| {
        super::presentation::intent_activity_bar(&format!(
            "preflighting {} asset pairs",
            pairs.len()
        ))
    });
    for (left, right) in pairs {
        let (from, to) = choose_start(left, right);
        let pair_key = canonical_pair(&from.id, &to.id);
        if !seen.insert(pair_key) || from.balance.is_zero() {
            continue;
        }
        let Some(amount) = candidate_quote_amount(from, to, order_type, wallet_bps) else {
            continue;
        };
        let Some(input_budget) = capped_input_budget(from, to, wallet_bps) else {
            continue;
        };
        if !chains_have_gas(&discovery.assets, &from.id.chain_id, &to.id.chain_id) {
            continue;
        }
        match preflight_pair(client, from, to, signer, order_type, amount, input_budget).await {
            Ok(Some(plan)) => {
                plans.push(plan);
            }
            Ok(None) => {}
            Err(_) => {}
        }
    }
    if let Some(spinner) = spinner {
        spinner.finish_and_clear();
    }
    plans
}

async fn random_send_plan(
    client: &RfqClient,
    discovery: &RouteDiscovery,
    signer: Address,
    recipient: Address,
    asset_type: AssetType,
    wallet_bps: u16,
    order_type: OrderType,
) -> Result<LegPlan> {
    let mut pairs = directed_cross_chain_pairs(&discovery.assets, asset_type);
    pairs.shuffle(&mut rand::thread_rng());
    for (from, to) in pairs {
        if ensure_source_ready(discovery, from).is_err() {
            continue;
        }
        let Some(amount) = candidate_quote_amount(from, to, order_type, wallet_bps) else {
            continue;
        };
        match preflight_leg(client, from, to, signer, recipient, order_type, amount).await {
            Ok(Some(plan)) => return Ok(plan),
            Ok(None) => {}
            Err(error) => ui::warn(&format!(
                "skipping {} -> {}: {}",
                from.label(),
                to.label(),
                ui::scrub_urls(&error.to_string())
            )),
        }
    }
    Err(eyre!(
        "No {}-to-{} route is both funded and quoted. Fund matching assets or choose a different --asset-type.",
        asset_type.label(),
        asset_type.label()
    ))
}

async fn random_roundtrip_plan(
    client: &RfqClient,
    discovery: &RouteDiscovery,
    signer: Address,
    asset_type: AssetType,
    wallet_bps: u16,
    order_type: OrderType,
) -> Result<RoutePlan> {
    let mut pairs = directed_cross_chain_pairs(&discovery.assets, asset_type);
    pairs.shuffle(&mut rand::thread_rng());
    for (from, to) in pairs {
        if ensure_roundtrip_ready(discovery, from, to).is_err() {
            continue;
        }
        let Some(amount) = candidate_quote_amount(from, to, order_type, wallet_bps) else {
            continue;
        };
        let Some(input_budget) = capped_input_budget(from, to, wallet_bps) else {
            continue;
        };
        match preflight_pair(client, from, to, signer, order_type, amount, input_budget).await {
            Ok(Some(plan)) => return Ok(plan),
            Ok(None) => {}
            Err(error) => ui::warn(&format!(
                "skipping {} -> {} -> {}: {}",
                from.label(),
                to.label(),
                from.label(),
                ui::scrub_urls(&error.to_string())
            )),
        }
    }
    Err(eyre!(
        "No {}-to-{} round-trip route is both funded and quoted. Fund matching assets or choose a different --asset-type.",
        asset_type.label(),
        asset_type.label()
    ))
}

fn directed_cross_chain_pairs(
    assets: &[WalletAsset],
    asset_type: AssetType,
) -> Vec<(&WalletAsset, &WalletAsset)> {
    assets
        .iter()
        .flat_map(|from| {
            assets
                .iter()
                .filter(move |to| {
                    asset_type.matches(from)
                        && asset_type.matches(to)
                        && from.id.chain_id != to.id.chain_id
                        && !from.balance.is_zero()
                })
                .map(move |to| (from, to))
        })
        .collect()
}

fn explicit_assets<'a>(
    discovery: &'a RouteDiscovery,
    from: &AssetSpec,
    to: &AssetSpec,
    asset_type: AssetType,
) -> Result<(&'a WalletAsset, &'a WalletAsset)> {
    if from.id().chain_id == to.id().chain_id {
        return Err(eyre!("intent routes must cross chains"));
    }
    let find = |asset: &AssetSpec| {
        discovery
            .assets
            .iter()
            .find(|candidate| &candidate.id == asset.id())
            .ok_or_else(|| eyre!("asset {asset} is not in the resolved RFQ catalog"))
    };
    let (from, to) = (find(from)?, find(to)?);
    if !asset_type.matches(from) || !asset_type.matches(to) {
        return Err(eyre!(
            "--asset-type {} requires {} assets for both --from and --to. Run `axe intents catalog` to choose matching assets.",
            asset_type.label(),
            asset_type.label()
        ));
    }
    Ok((from, to))
}

fn selected_amount(
    source: &WalletAsset,
    destination: &WalletAsset,
    order_type: OrderType,
    amount: Option<&HumanAmount>,
    wallet_bps: u16,
) -> Result<U256> {
    let amount = match amount {
        Some(amount) => amount.to_base_units(match order_type {
            OrderType::ExactInput => source.decimals,
            OrderType::ExactOutput => destination.decimals,
        })?,
        None => candidate_quote_amount(source, destination, order_type, wallet_bps)
            .ok_or_else(|| eyre!("{} has no spendable balance", source.label()))?,
    };
    if amount.is_zero() {
        return Err(eyre!("requested amount must be greater than zero"));
    }
    if order_type == OrderType::ExactInput {
        ensure_input_is_spendable(source, amount)?;
    }
    Ok(amount)
}

fn ensure_input_is_spendable(source: &WalletAsset, amount: U256) -> Result<()> {
    let spendable = spendable_balance(source)
        .ok_or_else(|| eyre!("{} has no balance after the gas reserve", source.label()))?;
    if amount > spendable {
        return Err(eyre!(
            "requested {} {}, but only {} is spendable",
            format_units(amount, source.decimals),
            source.symbol,
            format_units(spendable, source.decimals)
        ));
    }
    Ok(())
}

fn ensure_source_ready(discovery: &RouteDiscovery, source: &WalletAsset) -> Result<()> {
    if source.balance.is_zero() {
        return Err(eyre!("{} has no source balance", source.label()));
    }
    if !chain_has_gas(&discovery.assets, &source.id.chain_id) {
        return Err(eyre!("{} has insufficient native gas", source.chain_label));
    }
    Ok(())
}

fn ensure_roundtrip_ready(
    discovery: &RouteDiscovery,
    from: &WalletAsset,
    to: &WalletAsset,
) -> Result<()> {
    ensure_source_ready(discovery, from)?;
    if !chains_have_gas(&discovery.assets, &from.id.chain_id, &to.id.chain_id) {
        return Err(eyre!("both round-trip chains need native gas"));
    }
    Ok(())
}

fn unordered_cross_chain_pairs(
    assets: &[WalletAsset],
    asset_type: AssetType,
) -> Vec<(&WalletAsset, &WalletAsset)> {
    let mut pairs = Vec::new();
    for (index, left) in assets.iter().enumerate() {
        for right in &assets[index + 1..] {
            if left.id.chain_id != right.id.chain_id
                && asset_type.matches(left)
                && asset_type.matches(right)
                && (!left.balance.is_zero() || !right.balance.is_zero())
            {
                pairs.push((left, right));
            }
        }
    }
    pairs
}

fn choose_start<'a>(
    left: &'a WalletAsset,
    right: &'a WalletAsset,
) -> (&'a WalletAsset, &'a WalletAsset) {
    if left.balance.is_zero() && !right.balance.is_zero() {
        (right, left)
    } else {
        (left, right)
    }
}

fn canonical_pair(left: &AssetId, right: &AssetId) -> (String, String) {
    let left = format!(
        "{}/{}",
        left.chain_id,
        left.token_address.to_ascii_lowercase()
    );
    let right = format!(
        "{}/{}",
        right.chain_id,
        right.token_address.to_ascii_lowercase()
    );
    if left <= right {
        (left, right)
    } else {
        (right, left)
    }
}

fn candidate_amount(asset: &WalletAsset, wallet_bps: u16) -> Option<U256> {
    let spendable = spendable_balance(asset)?;
    let amount = spendable * U256::from(wallet_bps) / U256::from(BPS_DENOMINATOR);
    (!amount.is_zero()).then_some(amount)
}

fn candidate_quote_amount(
    source: &WalletAsset,
    destination: &WalletAsset,
    order_type: OrderType,
    wallet_bps: u16,
) -> Option<U256> {
    let input_amount = candidate_amount(source, wallet_bps)?;
    match order_type {
        OrderType::ExactInput => Some(input_amount),
        OrderType::ExactOutput => {
            scale_decimals(input_amount, source.decimals, destination.decimals)
        }
    }
}

fn capped_input_budget(
    source: &WalletAsset,
    destination: &WalletAsset,
    wallet_bps: u16,
) -> Option<RoundTripInputBudget> {
    Some(RoundTripInputBudget::Capped {
        forward: candidate_amount(source, wallet_bps)?,
        reverse_top_up: candidate_amount(destination, wallet_bps).unwrap_or_default(),
    })
}

fn scale_decimals(amount: U256, from_decimals: u8, to_decimals: u8) -> Option<U256> {
    let difference = from_decimals.abs_diff(to_decimals);
    let mut scale = U256::from(1);
    for _ in 0..difference {
        scale = scale.checked_mul(U256::from(10))?;
    }
    let scaled = if from_decimals < to_decimals {
        amount.checked_mul(scale)?
    } else {
        amount / scale
    };
    (!scaled.is_zero()).then_some(scaled)
}

fn spendable_balance(asset: &WalletAsset) -> Option<U256> {
    if asset.native {
        asset.balance.checked_sub(U256::from(GAS_RESERVE_WEI))
    } else {
        Some(asset.balance)
    }
}

fn chain_has_gas(assets: &[WalletAsset], chain_id: &str) -> bool {
    assets.iter().any(|asset| {
        asset.id.chain_id == chain_id
            && asset.native
            && asset.balance >= U256::from(GAS_RESERVE_WEI)
    })
}

fn chains_have_gas(assets: &[WalletAsset], first: &str, second: &str) -> bool {
    [first, second]
        .into_iter()
        .all(|chain_id| chain_has_gas(assets, chain_id))
}

async fn preflight_leg(
    client: &RfqClient,
    from: &WalletAsset,
    to: &WalletAsset,
    sender: Address,
    recipient: Address,
    order_type: OrderType,
    requested_amount: U256,
) -> Result<Option<LegPlan>> {
    let request = quote_request(from, to, sender, recipient, order_type, requested_amount);
    let QuoteOutcome::Available(quote) = client.quote(&request).await? else {
        return Ok(None);
    };
    validate_quote(&quote.quote, from, to, order_type, requested_amount)?;
    let input_amount = parse_amount(&quote.quote.input.amount)?;
    let expected_output = parse_amount(&quote.quote.output.amount)?;
    let quote_ms = duration_ms(quote.latency);
    Ok(Some(LegPlan {
        from: from.clone(),
        to: to.clone(),
        order_type,
        requested_amount,
        input_amount,
        expected_output,
        quote_ms,
        request,
        quote: *quote,
    }))
}

async fn preflight_pair(
    client: &RfqClient,
    from: &WalletAsset,
    to: &WalletAsset,
    signer: Address,
    order_type: OrderType,
    requested_amount: U256,
    input_budget: RoundTripInputBudget,
) -> Result<Option<RoutePlan>> {
    let Some(forward) = preflight_forward(
        client,
        from,
        to,
        signer,
        order_type,
        requested_amount,
        input_budget,
    )
    .await?
    else {
        return Ok(None);
    };
    let reverse_amount = match order_type {
        OrderType::ExactInput => forward.output,
        OrderType::ExactOutput => forward.input,
    };
    let reverse_request = quote_request(to, from, signer, signer, order_type, reverse_amount);
    let QuoteOutcome::Available(reverse) = client.quote(&reverse_request).await? else {
        return Ok(None);
    };
    let reverse_source = with_received_balance(to, forward.output)?;
    validate_quote(
        &reverse.quote,
        &reverse_source,
        from,
        order_type,
        reverse_amount,
    )?;
    let reverse_input = parse_amount(&reverse.quote.input.amount)?;
    if !reverse_input_is_within_budget(reverse_input, forward.output, input_budget) {
        return Ok(None);
    }
    let expected_return = parse_amount(&reverse.quote.output.amount)?;
    Ok(Some(RoutePlan {
        from: from.clone(),
        to: to.clone(),
        order_type,
        requested_amount: forward.requested,
        input_amount: forward.input,
        expected_return,
        forward_quote_ms: forward.quote_ms,
        reverse_quote_ms: duration_ms(reverse.latency),
        input_budget,
    }))
}

struct ForwardPreflight {
    requested: U256,
    input: U256,
    output: U256,
    quote_ms: u64,
}

async fn preflight_forward(
    client: &RfqClient,
    from: &WalletAsset,
    to: &WalletAsset,
    signer: Address,
    order_type: OrderType,
    initial_amount: U256,
    input_budget: RoundTripInputBudget,
) -> Result<Option<ForwardPreflight>> {
    let mut requested = initial_amount;
    for _ in 0..EXACT_OUTPUT_CALIBRATION_ATTEMPTS {
        let request = quote_request(from, to, signer, signer, order_type, requested);
        let QuoteOutcome::Available(quote) = client.quote(&request).await? else {
            return Ok(None);
        };
        validate_quote_route(&quote.quote, &from.id, &to.id, order_type, requested)?;
        let input = parse_amount(&quote.quote.input.amount)?;
        if forward_input_is_within_budget(input, input_budget) {
            validate_quote(&quote.quote, from, to, order_type, requested)?;
            return Ok(Some(ForwardPreflight {
                requested,
                input,
                output: parse_amount(&quote.quote.output.amount)?,
                quote_ms: duration_ms(quote.latency),
            }));
        }
        if order_type != OrderType::ExactOutput {
            return Ok(None);
        }
        let Some(limit) = forward_input_limit(input_budget) else {
            return Ok(None);
        };
        let Some(adjusted) = calibrate_exact_output(requested, input, limit) else {
            return Ok(None);
        };
        requested = adjusted;
    }
    Ok(None)
}

fn forward_input_limit(budget: RoundTripInputBudget) -> Option<U256> {
    match budget {
        RoundTripInputBudget::SpendableBalance => None,
        RoundTripInputBudget::Capped { forward, .. } => Some(forward),
    }
}

fn calibrate_exact_output(requested: U256, quoted_input: U256, input_limit: U256) -> Option<U256> {
    if quoted_input.is_zero() {
        return None;
    }
    let target_input = U512::from(input_limit) * U512::from(EXACT_OUTPUT_HEADROOM_BPS)
        / U512::from(BPS_DENOMINATOR);
    let adjusted = U512::from(requested) * target_input / U512::from(quoted_input);
    let adjusted = adjusted.saturating_to::<U256>();
    (!adjusted.is_zero() && adjusted < requested).then_some(adjusted)
}

fn forward_input_is_within_budget(input: U256, budget: RoundTripInputBudget) -> bool {
    match budget {
        RoundTripInputBudget::SpendableBalance => true,
        RoundTripInputBudget::Capped { forward, .. } => input <= forward,
    }
}

fn reverse_input_is_within_budget(
    input: U256,
    received: U256,
    budget: RoundTripInputBudget,
) -> bool {
    match budget {
        RoundTripInputBudget::SpendableBalance => true,
        RoundTripInputBudget::Capped { reverse_top_up, .. } => received
            .checked_add(reverse_top_up)
            .is_some_and(|maximum| input <= maximum),
    }
}

fn with_received_balance(asset: &WalletAsset, received: U256) -> Result<WalletAsset> {
    let mut funded = asset.clone();
    funded.balance = funded
        .balance
        .checked_add(received)
        .ok_or_else(|| eyre!("{} balance overflowed", asset.label()))?;
    Ok(funded)
}

pub fn quote_request(
    from: &WalletAsset,
    to: &WalletAsset,
    sender: Address,
    recipient: Address,
    order_type: OrderType,
    amount: U256,
) -> QuoteRequest {
    QuoteRequest {
        from_chain: from.id.chain_id.clone(),
        from_token: from.id.token_address.clone(),
        to_chain: to.id.chain_id.clone(),
        to_token: to.id.token_address.clone(),
        amount: amount.to_string(),
        order_type,
        sender: sender.to_string(),
        recipient: recipient.to_string(),
    }
}

pub fn validate_quote(
    quote: &Quote,
    from: &WalletAsset,
    to: &WalletAsset,
    order_type: OrderType,
    requested_amount: U256,
) -> Result<()> {
    validate_quote_route(quote, &from.id, &to.id, order_type, requested_amount)?;
    let input_amount = parse_amount(&quote.input.amount)?;
    ensure_input_is_spendable(from, input_amount).wrap_err_with(|| {
        format!(
            "quote requires {} {} of input",
            format_units(input_amount, from.decimals),
            from.symbol
        )
    })?;
    Ok(())
}

pub fn validate_quote_route(
    quote: &Quote,
    from: &AssetId,
    to: &AssetId,
    order_type: OrderType,
    requested_amount: U256,
) -> Result<()> {
    let fixed_amount = match order_type {
        OrderType::ExactInput => parse_amount(&quote.input.amount)?,
        OrderType::ExactOutput => parse_amount(&quote.output.amount)?,
    };
    if quote.backend.kind != "intent"
        || quote.input.chain != from.chain_id
        || !quote.input.token.eq_ignore_ascii_case(&from.token_address)
        || quote.output.chain != to.chain_id
        || !quote.output.token.eq_ignore_ascii_case(&to.token_address)
        || fixed_amount != requested_amount
    {
        return Err(eyre!(
            "RFQ returned a quote that does not match the requested route"
        ));
    }
    Ok(())
}

fn duration_ms(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn render_wallet_stats(
    wallet: Address,
    chains: &HashMap<String, ChainRuntime>,
    assets: &[WalletAsset],
) {
    ui::section("intent wallet");
    ui::address("address", &wallet.to_string());
    let unfunded: Vec<String> = assets
        .iter()
        .filter(|asset| asset.balance.is_zero())
        .map(WalletAsset::label)
        .collect();
    if unfunded.is_empty() {
        ui::success(&format!(
            "All {} supported assets across {} chains are funded.",
            assets.len(),
            chains.len()
        ));
    } else {
        ui::warn(&format!(
            "{} of {} supported assets are not funded: {}",
            unfunded.len(),
            assets.len(),
            unfunded.join(", ")
        ));
    }
}

pub(super) fn render_plans(plans: &[RoutePlan]) {
    ui::section("intent preflight");
    ui::kv("quotable round trips", &plans.len().to_string());
    for plan in plans {
        ui::kv(
            "asset type",
            if plan.from.native { "native" } else { "token" },
        );
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
    ui::kv(
        "average quote latency",
        &format!("{} ms", average_quote_ms(plans)),
    );
}

fn render_leg_plan(plan: &LegPlan) {
    ui::section("intent preflight");
    ui::kv(
        "asset type",
        if plan.from.native { "native" } else { "token" },
    );
    ui::kv(
        "route",
        &format!("{} -> {}", plan.from.label(), plan.to.label()),
    );
    ui::kv(
        "input",
        &format!(
            "{} {}",
            format_units(plan.input_amount, plan.from.decimals),
            plan.from.symbol
        ),
    );
    ui::kv(
        "expected output",
        &format!(
            "{} {}",
            format_units(plan.expected_output, plan.to.decimals),
            plan.to.symbol
        ),
    );
    ui::kv("quote latency", &format!("{} ms", plan.quote_ms));
}

fn average_quote_ms(plans: &[RoutePlan]) -> u64 {
    let total: u128 = plans
        .iter()
        .map(|plan| u128::from(plan.forward_quote_ms) + u128::from(plan.reverse_quote_ms))
        .sum();
    let quotes = plans.len() as u128 * 2;
    u64::try_from(total.checked_div(quotes).unwrap_or_default()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::intents::types::{Backend, QuoteAmount, Validity};

    fn asset(chain: &str, token: &str, balance: u64, native: bool) -> WalletAsset {
        WalletAsset {
            id: AssetId {
                chain_id: chain.into(),
                token_address: token.into(),
            },
            chain_label: chain.into(),
            symbol: token.into(),
            decimals: 6,
            balance: U256::from(balance),
            native,
        }
    }

    #[test]
    fn pairs_only_cross_chain_assets() {
        let assets = vec![
            asset("eip155:1", "a", 1, false),
            asset("eip155:1", "b", 1, false),
            asset("eip155:2", "c", 1, false),
        ];
        let pairs = unordered_cross_chain_pairs(&assets, AssetType::Token);
        assert_eq!(pairs.len(), 2);
    }

    #[test]
    fn directed_pairs_allow_either_chain_to_be_the_source() {
        let assets = vec![
            asset("eip155:1", "a", 1, false),
            asset("eip155:1", "b", 1, false),
            asset("eip155:2", "c", 1, false),
        ];

        assert_eq!(
            directed_cross_chain_pairs(&assets, AssetType::Token).len(),
            4
        );
    }

    #[test]
    fn asset_type_filters_both_ends_of_automatic_routes() {
        let assets = vec![
            asset("eip155:1", "USDC", 1, false),
            asset("eip155:1", "ETH", 1, true),
            asset("eip155:2", "USDC", 1, false),
            asset("eip155:2", "AVAX", 1, true),
        ];

        let token_pairs = unordered_cross_chain_pairs(&assets, AssetType::Token);
        let native_pairs = unordered_cross_chain_pairs(&assets, AssetType::Native);
        assert_eq!(token_pairs.len(), 1);
        assert!(
            token_pairs
                .iter()
                .all(|(left, right)| !left.native && !right.native)
        );
        assert_eq!(native_pairs.len(), 1);
        assert!(
            native_pairs
                .iter()
                .all(|(left, right)| left.native && right.native)
        );
    }

    #[test]
    fn explicit_routes_must_match_the_selected_asset_type() {
        let discovery = RouteDiscovery {
            chains: HashMap::new(),
            assets: vec![
                asset(
                    "eip155:1",
                    "0x0000000000000000000000000000000000000000",
                    1,
                    true,
                ),
                asset(
                    "eip155:2",
                    "0x0000000000000000000000000000000000000000",
                    1,
                    true,
                ),
            ],
        };
        let from = "eip155:1/0x0000000000000000000000000000000000000000"
            .parse::<AssetSpec>()
            .unwrap();
        let to = "eip155:2/0x0000000000000000000000000000000000000000"
            .parse::<AssetSpec>()
            .unwrap();

        assert!(explicit_assets(&discovery, &from, &to, AssetType::Native).is_ok());
        assert_eq!(
            explicit_assets(&discovery, &from, &to, AssetType::Token)
                .unwrap_err()
                .to_string(),
            "--asset-type token requires token assets for both --from and --to. Run `axe intents catalog` to choose matching assets."
        );
    }

    #[test]
    fn chooses_the_funded_side_as_source() {
        let empty = asset("eip155:1", "a", 0, false);
        let funded = asset("eip155:2", "b", 10, false);
        let (from, to) = choose_start(&empty, &funded);
        assert_eq!(from.id, funded.id);
        assert_eq!(to.id, empty.id);
    }

    #[test]
    fn candidate_amount_is_wallet_fraction() {
        let funded = asset("eip155:1", "a", 1_000_000, false);
        assert_eq!(candidate_amount(&funded, 100), Some(U256::from(10_000u64)));
    }

    #[test]
    fn selected_human_amount_uses_source_decimals() {
        let funded = asset("eip155:1", "USDC", 2_000_000, false);
        let destination = asset("eip155:2", "USDC", 0, false);
        let amount = "1.25".parse::<HumanAmount>().unwrap();

        assert_eq!(
            selected_amount(
                &funded,
                &destination,
                OrderType::ExactInput,
                Some(&amount),
                100
            )
            .unwrap(),
            U256::from(1_250_000u64)
        );
    }

    #[test]
    fn exact_output_human_amount_uses_destination_decimals() {
        let funded = asset("eip155:1", "USDC", 2_000_000, false);
        let mut destination = asset("eip155:2", "ETH", 0, false);
        destination.decimals = 18;
        let amount = "0.01".parse::<HumanAmount>().unwrap();

        assert_eq!(
            selected_amount(
                &funded,
                &destination,
                OrderType::ExactOutput,
                Some(&amount),
                100
            )
            .unwrap(),
            U256::from(10_000_000_000_000_000u64)
        );
    }

    #[test]
    fn automatic_exact_output_preserves_the_human_scale() {
        let source = asset("eip155:1", "USDC", 1_000_000, false);
        let mut destination = asset("eip155:2", "ETH", 0, false);
        destination.decimals = 18;

        assert_eq!(
            candidate_quote_amount(&source, &destination, OrderType::ExactOutput, 100),
            Some(U256::from(10_000_000_000_000_000u64))
        );
    }

    #[test]
    fn quote_request_keeps_sender_and_recipient_distinct() {
        let from = asset("eip155:1", "a", 1, false);
        let to = asset("eip155:2", "b", 0, false);
        let sender = Address::from([1u8; 20]);
        let recipient = Address::from([2u8; 20]);

        let request = quote_request(
            &from,
            &to,
            sender,
            recipient,
            OrderType::ExactOutput,
            U256::from(1),
        );

        assert_eq!(request.sender, sender.to_string());
        assert_eq!(request.recipient, recipient.to_string());
        assert_eq!(request.order_type, OrderType::ExactOutput);
    }

    #[test]
    fn exact_output_quote_matches_the_requested_output() {
        let from = asset("eip155:1", "a", 2_000_000, false);
        let to = asset("eip155:2", "b", 0, false);
        let quote = Quote {
            quote_id: "quote-id".into(),
            backend: Backend {
                kind: "intent".into(),
                tracking: Default::default(),
            },
            validity: Validity {
                quote_expires_at: chrono::Utc::now() + chrono::Duration::minutes(1),
                fulfillment_deadline: None,
            },
            input: QuoteAmount {
                chain: from.id.chain_id.clone(),
                token: from.id.token_address.clone(),
                amount: "1003514".into(),
            },
            output: QuoteAmount {
                chain: to.id.chain_id.clone(),
                token: to.id.token_address.clone(),
                amount: "1000000".into(),
            },
            actions: Vec::new(),
        };

        assert!(
            validate_quote(
                &quote,
                &from,
                &to,
                OrderType::ExactOutput,
                U256::from(1_000_000u64)
            )
            .is_ok()
        );
        assert!(
            validate_quote(
                &quote,
                &from,
                &to,
                OrderType::ExactInput,
                U256::from(1_000_000u64)
            )
            .is_err()
        );
    }

    #[test]
    fn average_quote_latency_covers_both_round_trip_legs() {
        let from = asset("eip155:1", "a", 10, false);
        let to = asset("eip155:2", "b", 0, false);
        let plans = [RoutePlan {
            from,
            to,
            order_type: OrderType::ExactInput,
            requested_amount: U256::from(1),
            input_amount: U256::from(1),
            expected_return: U256::from(1),
            forward_quote_ms: 10,
            reverse_quote_ms: 30,
            input_budget: RoundTripInputBudget::SpendableBalance,
        }];

        assert_eq!(average_quote_ms(&plans), 20);
        assert_eq!(average_quote_ms(&[]), 0);
    }

    #[test]
    fn automatic_round_trips_cap_forward_and_reverse_wallet_spend() {
        let budget = RoundTripInputBudget::Capped {
            forward: U256::from(100),
            reverse_top_up: U256::from(10),
        };

        assert!(forward_input_is_within_budget(U256::from(100), budget));
        assert!(!forward_input_is_within_budget(U256::from(101), budget));
        assert!(reverse_input_is_within_budget(
            U256::from(110),
            U256::from(100),
            budget
        ));
        assert!(!reverse_input_is_within_budget(
            U256::from(111),
            U256::from(100),
            budget
        ));
    }

    #[test]
    fn exact_output_calibration_leaves_headroom_below_the_input_limit() {
        assert_eq!(
            calibrate_exact_output(
                U256::from(1_000_000),
                U256::from(1_010_000),
                U256::from(1_000_000)
            ),
            Some(U256::from(940_594))
        );
        let large = calibrate_exact_output(U256::MAX, U256::MAX, U256::MAX).unwrap();
        assert!(large < U256::MAX);
        assert_eq!(
            calibrate_exact_output(U256::from(1), U256::ZERO, U256::from(1)),
            None
        );
    }

    #[test]
    fn chain_label_prioritizes_the_matching_duplicate_config() {
        let config = ChainsConfig::from_json_str(
            r#"{
                "chains": {
                    "test-sepolia": { "chainId": 11155111, "name": "test-Sepolia" },
                    "ethereum-sepolia": {
                        "chainId": 11155111,
                        "name": "Ethereum-Sepolia"
                    }
                },
                "axelar": {}
            }"#,
        )
        .unwrap();
        let candidates = evm_chain_candidates(&config, 11_155_111, "Ethereum Sepolia");
        assert_eq!(
            candidates.first().map(|(key, _)| *key),
            Some("ethereum-sepolia")
        );
    }

    #[test]
    fn intent_rpc_overrides_map_to_their_testnet_chain_ids() {
        let mappings = [
            (11_155_111, IntentRpcOverride::EthereumSepolia),
            (421_614, IntentRpcOverride::ArbitrumSepolia),
            (84_532, IntentRpcOverride::BaseSepolia),
            (43_113, IntentRpcOverride::AvalancheFuji),
        ];

        for (chain_id, expected) in mappings {
            assert_eq!(IntentRpcOverride::for_chain_id(chain_id), Some(expected));
        }
        assert_eq!(IntentRpcOverride::for_chain_id(1), None);
    }

    #[test]
    fn intent_rpc_override_precedes_config_without_removing_fallbacks() {
        let config = ChainsConfig::from_json_str(
            r#"{
                "chains": {
                    "ethereum-sepolia": {
                        "chainId": 11155111,
                        "name": "Ethereum-Sepolia",
                        "rpc": "https://config.example"
                    }
                },
                "axelar": {}
            }"#,
        )
        .unwrap();
        let configured = evm_chain_candidates(&config, 11_155_111, "Ethereum Sepolia");
        let candidates = intent_rpc_candidates(
            configured,
            Some((
                IntentRpcOverride::EthereumSepolia,
                "https://override.example".to_owned(),
            )),
        );

        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].source, "override SEP_RPC_URL");
        assert_eq!(candidates[0].url, "https://override.example");
        assert_eq!(candidates[1].source, "config ethereum-sepolia");
        assert_eq!(candidates[1].url, "https://config.example");
    }
}
