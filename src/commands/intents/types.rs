use std::fmt::{Display, Formatter};
use std::hash::{Hash, Hasher};
use std::str::FromStr;
use std::time::Duration;

use alloy::primitives::{Address, U256};
use chrono::{DateTime, Utc};
use clap::ValueEnum;
use serde::{Deserialize, Deserializer, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ChainsResponse {
    pub chains: Vec<ChainInfo>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChainInfo {
    pub chain_id: String,
    pub chain_label: String,
    pub chain_type: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TokensResponse {
    pub tokens: Vec<TokenInfo>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenInfo {
    pub chain_id: String,
    pub address: String,
    pub symbol: String,
    pub decimals: u8,
}

#[derive(Clone, Debug, Serialize)]
pub struct CatalogResponse {
    pub chains: Vec<CatalogChain>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogChain {
    #[serde(flatten)]
    pub chain: ChainInfo,
    pub tokens: Vec<CatalogToken>,
}

#[derive(Clone, Debug, Serialize)]
pub struct CatalogToken {
    pub address: String,
    pub symbol: String,
    pub decimals: u8,
}

impl From<TokenInfo> for CatalogToken {
    fn from(token: TokenInfo) -> Self {
        Self {
            address: token.address,
            symbol: token.symbol,
            decimals: token.decimals,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuoteRequest {
    pub from_chain: String,
    pub from_token: String,
    pub to_chain: String,
    pub to_token: String,
    pub amount: String,
    pub order_type: OrderType,
    pub sender: String,
    pub recipient: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, ValueEnum)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OrderType {
    #[default]
    ExactInput,
    ExactOutput,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum AssetType {
    #[default]
    Token,
    Native,
}

impl AssetType {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Token => "token",
            Self::Native => "native",
        }
    }

    pub const fn matches(self, asset: &WalletAsset) -> bool {
        asset.native == matches!(self, Self::Native)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct QuoteResponse {
    pub quotes: Vec<Quote>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Quote {
    pub quote_id: String,
    pub backend: Backend,
    pub validity: Validity,
    pub input: QuoteAmount,
    pub output: QuoteAmount,
    pub actions: Vec<Action>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Backend {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub tracking: BackendTracking,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackendTracking {
    pub swap_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Validity {
    #[serde(deserialize_with = "deserialize_datetime")]
    pub quote_expires_at: DateTime<Utc>,
    #[serde(default, deserialize_with = "deserialize_optional_datetime")]
    pub fulfillment_deadline: Option<DateTime<Utc>>,
}

fn deserialize_datetime<'de, D>(deserializer: D) -> Result<DateTime<Utc>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    DateTime::parse_from_rfc3339(&value)
        .map(|datetime| datetime.with_timezone(&Utc))
        .map_err(serde::de::Error::custom)
}

fn deserialize_optional_datetime<'de, D>(deserializer: D) -> Result<Option<DateTime<Utc>>, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer)?
        .map(|value| {
            DateTime::parse_from_rfc3339(&value).map(|datetime| datetime.with_timezone(&Utc))
        })
        .transpose()
        .map_err(serde::de::Error::custom)
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct QuoteAmount {
    pub chain: String,
    pub token: String,
    pub amount: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Action {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub chain: String,
    pub payload: serde_json::Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EvmTransactionPayload {
    #[serde(rename = "type")]
    pub kind: String,
    pub from: Option<String>,
    pub to: String,
    pub data: String,
    pub value: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusResponse {
    pub quote_id: String,
    pub state: TransferState,
    pub destination: Option<ChainDelivery>,
    pub output: Option<StatusOutput>,
    pub refund: Option<Refund>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TransferState {
    AwaitingDeposit,
    Pending,
    Done,
    Refunded,
    Failed,
    NotFound,
}

impl TransferState {
    pub const fn label(self) -> &'static str {
        match self {
            Self::AwaitingDeposit => "awaiting deposit",
            Self::Pending => "pending",
            Self::Done => "done",
            Self::Refunded => "refunded",
            Self::Failed => "failed",
            Self::NotFound => "not found",
        }
    }

    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Refunded | Self::Failed)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChainDelivery {
    pub tx_hash: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct StatusOutput {
    pub chain: String,
    pub token: String,
    pub amount: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Refund {
    pub chain: String,
    pub token: String,
    pub amount: String,
    pub tx_hash: String,
}

#[derive(Clone, Debug)]
pub struct TimedQuote {
    pub quote: Quote,
    pub latency: Duration,
}

#[derive(Clone, Debug)]
pub enum QuoteOutcome {
    Available(Box<TimedQuote>),
    Unavailable(String),
}

#[derive(Clone, Debug)]
pub struct ChainRuntime {
    pub label: String,
    pub rpc_url: String,
}

#[derive(Clone, Debug, Eq)]
pub struct AssetId {
    pub chain_id: String,
    pub token_address: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssetSpec(AssetId);

impl AssetSpec {
    pub fn id(&self) -> &AssetId {
        &self.0
    }
}

impl FromStr for AssetSpec {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (chain_id, token_address) = value
            .split_once('/')
            .ok_or_else(|| "expected <CAIP-2 chain>/<token address>".to_owned())?;
        if token_address.contains('/') {
            return Err("expected exactly one '/' between chain and token".to_owned());
        }
        let reference = chain_id
            .strip_prefix("eip155:")
            .ok_or_else(|| "intent assets currently require an eip155 chain ID".to_owned())?;
        reference
            .parse::<u64>()
            .map_err(|_| format!("invalid EVM chain ID '{chain_id}'"))?;
        let token_address = token_address
            .parse::<Address>()
            .map_err(|_| format!("invalid EVM token address '{token_address}'"))?;
        Ok(Self(AssetId {
            chain_id: chain_id.to_owned(),
            token_address: token_address.to_string(),
        }))
    }
}

impl Display for AssetSpec {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HumanAmount(String);

impl HumanAmount {
    pub fn to_base_units(&self, decimals: u8) -> eyre::Result<U256> {
        let (whole, fraction) = decimal_parts(&self.0).map_err(eyre::Report::msg)?;
        let decimals = usize::from(decimals);
        if fraction.len() > decimals {
            return Err(eyre::eyre!(
                "amount '{}' has more than {decimals} decimal places",
                self.0
            ));
        }
        let whole = if whole.is_empty() { "0" } else { whole };
        let scaled = format!("{whole}{fraction}{}", "0".repeat(decimals - fraction.len()));
        scaled
            .parse::<U256>()
            .map_err(|error| eyre::eyre!("amount '{}' is too large: {error}", self.0))
    }
}

impl FromStr for HumanAmount {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        decimal_parts(value)?;
        Ok(Self(value.to_owned()))
    }
}

impl Display for HumanAmount {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

fn decimal_parts(value: &str) -> Result<(&str, &str), String> {
    let mut parts = value.split('.');
    let whole = parts.next().unwrap_or_default();
    let fraction = parts.next().unwrap_or_default();
    if parts.next().is_some()
        || (whole.is_empty() && fraction.is_empty())
        || !whole.chars().all(|character| character.is_ascii_digit())
        || !fraction.chars().all(|character| character.is_ascii_digit())
    {
        return Err(format!("invalid decimal amount '{value}'"));
    }
    Ok((whole, fraction))
}

impl PartialEq for AssetId {
    fn eq(&self, other: &Self) -> bool {
        self.chain_id == other.chain_id
            && self
                .token_address
                .eq_ignore_ascii_case(&other.token_address)
    }
}

impl Hash for AssetId {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.chain_id.hash(state);
        self.token_address.to_ascii_lowercase().hash(state);
    }
}

#[derive(Clone, Debug)]
pub struct WalletAsset {
    pub id: AssetId,
    pub chain_label: String,
    pub symbol: String,
    pub decimals: u8,
    pub balance: U256,
    pub native: bool,
}

impl WalletAsset {
    pub fn label(&self) -> String {
        format!("{}/{}", self.chain_label, self.symbol)
    }
}

#[derive(Clone, Debug)]
pub struct RoutePlan {
    pub from: WalletAsset,
    pub to: WalletAsset,
    pub order_type: OrderType,
    pub requested_amount: U256,
    pub input_amount: U256,
    pub expected_return: U256,
    pub forward_quote_ms: u64,
    pub reverse_quote_ms: u64,
    pub input_budget: RoundTripInputBudget,
}

#[derive(Clone, Copy, Debug)]
pub enum RoundTripInputBudget {
    SpendableBalance,
    Capped { forward: U256, reverse_top_up: U256 },
}

#[derive(Clone, Debug)]
pub struct LegPlan {
    pub from: WalletAsset,
    pub to: WalletAsset,
    pub order_type: OrderType,
    pub requested_amount: U256,
    pub input_amount: U256,
    pub expected_output: U256,
    pub quote_ms: u64,
}

#[derive(Clone, Debug)]
pub struct LegExecution {
    pub from: WalletAsset,
    pub to: WalletAsset,
    pub order_type: OrderType,
    pub amount: U256,
    pub recipient: Address,
    pub max_input_amount: Option<U256>,
}

#[derive(Clone, Debug)]
pub struct LegResult {
    pub input_amount: U256,
    pub output_amount: U256,
    pub quote_latency_ms: u64,
    pub deposit_confirmation_latency_ms: u64,
    pub fulfillment_latency_ms: u64,
    pub end_to_end_latency_ms: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct RunLimits {
    pub poll_interval: Duration,
    pub fulfillment_timeout: Duration,
}

pub fn is_native_token(address: &str) -> bool {
    address
        .parse::<Address>()
        .is_ok_and(|address| address.is_zero())
}

pub fn format_units(amount: U256, decimals: u8) -> String {
    let digits = amount.to_string();
    let decimals = usize::from(decimals);
    if decimals == 0 {
        return digits;
    }
    let padded = if digits.len() <= decimals {
        format!("{}{}", "0".repeat(decimals + 1 - digits.len()), digits)
    } else {
        digits
    };
    let split = padded.len() - decimals;
    let fraction = padded[split..].trim_end_matches('0');
    if fraction.is_empty() {
        padded[..split].to_owned()
    } else {
        format!("{}.{}", &padded[..split], fraction)
    }
}

pub fn parse_amount(value: &str) -> eyre::Result<U256> {
    value
        .parse::<U256>()
        .map_err(|error| eyre::eyre!("invalid base-unit amount '{value}': {error}"))
}

impl Display for AssetId {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}/{}", self.chain_id, self.token_address)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_base_units_without_float_rounding() {
        assert_eq!(format_units(U256::from(1_000_000u64), 6), "1");
        assert_eq!(format_units(U256::from(1_250_000u64), 6), "1.25");
        assert_eq!(format_units(U256::from(1u64), 6), "0.000001");
    }

    #[test]
    fn asset_ids_compare_evm_addresses_case_insensitively() {
        let lower = AssetId {
            chain_id: "eip155:1".into(),
            token_address: "0x000000000000000000000000000000000000dead".into(),
        };
        let mixed = AssetId {
            chain_id: "eip155:1".into(),
            token_address: "0x000000000000000000000000000000000000dEaD".into(),
        };
        assert_eq!(lower, mixed);
    }

    #[test]
    fn parses_typed_evm_asset_specs() {
        let spec = "eip155:11155111/0x000000000000000000000000000000000000dEaD"
            .parse::<AssetSpec>()
            .unwrap();

        assert_eq!(spec.id().chain_id, "eip155:11155111");
        assert!(
            spec.id()
                .token_address
                .eq_ignore_ascii_case("0x000000000000000000000000000000000000dead")
        );
        assert!("avalanche/AVAX".parse::<AssetSpec>().is_err());
    }

    #[test]
    fn parses_human_amounts_without_floats() {
        assert_eq!(
            "0.001"
                .parse::<HumanAmount>()
                .unwrap()
                .to_base_units(18)
                .unwrap(),
            U256::from(1_000_000_000_000_000u64)
        );
        assert_eq!(
            "1.25"
                .parse::<HumanAmount>()
                .unwrap()
                .to_base_units(6)
                .unwrap(),
            U256::from(1_250_000u64)
        );
        assert!(
            "0.0000001"
                .parse::<HumanAmount>()
                .unwrap()
                .to_base_units(6)
                .is_err()
        );
        assert!("1e3".parse::<HumanAmount>().is_err());
    }

    #[test]
    fn serializes_rfq_order_types() {
        assert_eq!(
            serde_json::to_string(&OrderType::ExactInput).unwrap(),
            "\"EXACT_INPUT\""
        );
        assert_eq!(
            serde_json::to_string(&OrderType::ExactOutput).unwrap(),
            "\"EXACT_OUTPUT\""
        );
    }

    #[test]
    fn token_is_the_default_asset_type() {
        assert_eq!(AssetType::default(), AssetType::Token);
        assert_eq!(
            serde_json::to_string(&AssetType::Token).unwrap(),
            "\"token\""
        );
        assert_eq!(
            serde_json::to_string(&AssetType::Native).unwrap(),
            "\"native\""
        );
    }

    #[test]
    fn only_final_transfer_states_are_terminal() {
        assert!(TransferState::Done.is_terminal());
        assert!(TransferState::Refunded.is_terminal());
        assert!(TransferState::Failed.is_terminal());
        assert!(!TransferState::AwaitingDeposit.is_terminal());
        assert!(!TransferState::Pending.is_terminal());
        assert!(!TransferState::NotFound.is_terminal());
    }
}
