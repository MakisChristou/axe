use std::fmt::{Display, Formatter};
use std::hash::{Hash, Hasher};
use std::str::FromStr;
use std::time::Duration;

use alloy::primitives::{Address, U256};
use chrono::{DateTime, Utc};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};

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

    pub fn matches_token_address(self, address: &str) -> bool {
        is_native_token(address) == matches!(self, Self::Native)
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
    pub selection_reason: SelectionReason,
    pub backend: Backend,
    pub estimated_time_seconds: u64,
    pub validity: Validity,
    pub input: QuoteInput,
    pub output: QuoteOutput,
    pub fees: Fees,
    pub actions: Vec<Action>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SelectionReason {
    BestAvailable,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Backend {
    #[serde(rename = "type")]
    pub kind: BackendType,
    pub name: String,
    pub tracking: serde_json::Value,
    pub metadata: serde_json::Value,
}

impl Backend {
    pub fn swap_id(&self) -> Option<&str> {
        self.tracking.get("swapId").and_then(|value| value.as_str())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum BackendType {
    Intent,
    Its,
    Gateway,
    GatewayExpress,
}

impl Display for BackendType {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            Self::Intent => "intent",
            Self::Its => "its",
            Self::Gateway => "gateway",
            Self::GatewayExpress => "gateway-express",
        };
        formatter.write_str(value)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Validity {
    #[serde(rename = "type")]
    pub kind: String,
    pub quote_expires_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fulfillment_deadline: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuoteInput {
    pub chain: String,
    pub token: String,
    pub amount: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub amount_usd_approx: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuoteOutput {
    pub chain: String,
    pub token: String,
    pub amount: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub minimum_amount: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub amount_usd_approx: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Fees {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gas: Option<FeeEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<FeeEntry>,
    pub integrator: Option<FeeEntry>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FeeEntry {
    pub amount: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub amount_usd_approx: Option<String>,
    pub token: FeeToken,
    pub payment_method: PaymentMethod,
    pub quote_treatment: QuoteTreatment,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bps: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recipient: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FeeToken {
    pub chain: String,
    pub symbol: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PaymentMethod {
    WalletNative,
    TxValue,
    InputToken,
    OutputToken,
    Sponsored,
    Offchain,
}

impl Display for PaymentMethod {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            Self::WalletNative => "wallet native",
            Self::TxValue => "transaction value",
            Self::InputToken => "input token",
            Self::OutputToken => "output token",
            Self::Sponsored => "sponsored",
            Self::Offchain => "off-chain",
        };
        formatter.write_str(value)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QuoteTreatment {
    OutsideQuote,
    IncludedInQuote,
    Informational,
}

impl Display for QuoteTreatment {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            Self::OutsideQuote => "outside quote",
            Self::IncludedInQuote => "included in quote",
            Self::Informational => "informational",
        };
        formatter.write_str(value)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Action {
    pub id: String,
    pub label: String,
    #[serde(rename = "type")]
    pub kind: ActionKind,
    pub chain: String,
    pub payload: ActionPayload,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    Approval,
    Transaction,
    DepositAddress,
}

impl Display for ActionKind {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            Self::Approval => "approval",
            Self::Transaction => "transaction",
            Self::DepositAddress => "deposit_address",
        };
        formatter.write_str(value)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ActionPayload {
    EvmTransaction(EvmTransactionPayload),
    SolanaInstructions(SolanaInstructionsPayload),
    DepositAddress(DepositAddressPayload),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EvmTransactionPayload {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    pub to: String,
    pub data: String,
    pub value: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gas_limit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_fee_per_gas: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_priority_fee_per_gas: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SolanaInstructionsPayload {
    pub instructions: Vec<serde_json::Value>,
    #[serde(default)]
    pub address_lookup_table_addresses: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DepositAddressPayload {
    pub address: String,
    pub amount: String,
    pub token: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusResponse {
    pub quote_id: String,
    pub state: TransferState,
    pub backend: Backend,
    pub source: Option<ChainExecution>,
    pub destination: Option<ChainDelivery>,
    pub input: Option<StatusInput>,
    pub output: Option<StatusOutput>,
    pub refund: Option<Refund>,
    pub details: serde_json::Value,
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
pub struct ChainExecution {
    pub chain: String,
    pub tx_hash: String,
    pub message_id: String,
    pub timestamp: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChainDelivery {
    pub chain: String,
    pub tx_hash: String,
    pub timestamp: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct StatusInput {
    pub chain: String,
    pub token: String,
    pub amount: String,
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
    pub request: QuoteRequest,
    pub quote: TimedQuote,
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
    use serde_json::json;

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

    #[test]
    fn quote_response_round_trips_the_current_rfq_schema() {
        let response = json!({
            "quotes": [{
                "quoteId": "quote-id",
                "selectionReason": "best_available",
                "backend": {
                    "type": "intent",
                    "name": "Axelar Intents",
                    "tracking": { "swapId": "0xswap" },
                    "metadata": { "solver": "test" }
                },
                "estimatedTimeSeconds": 60,
                "validity": {
                    "type": "expires_at",
                    "quoteExpiresAt": "2026-09-02T14:08:56Z",
                    "fulfillmentDeadline": "2026-09-02T14:26:56Z"
                },
                "input": {
                    "chain": "eip155:421614",
                    "token": "0xsource",
                    "amount": "1000000",
                    "amountUsdApprox": "1.00"
                },
                "output": {
                    "chain": "eip155:43113",
                    "token": "0xdestination",
                    "amount": "996499",
                    "minimumAmount": "996499",
                    "amountUsdApprox": "1.00"
                },
                "fees": {
                    "gas": {
                        "amount": "53338194000000",
                        "amountUsdApprox": "0.13",
                        "token": {
                            "chain": "eip155:421614",
                            "symbol": "ETH",
                            "type": "native"
                        },
                        "paymentMethod": "wallet_native",
                        "quoteTreatment": "outside_quote"
                    },
                    "user": {
                        "amount": "3501",
                        "amountUsdApprox": "0.01",
                        "token": {
                            "chain": "eip155:43113",
                            "symbol": "USDC",
                            "address": "0xdestination"
                        },
                        "paymentMethod": "output_token",
                        "quoteTreatment": "included_in_quote"
                    },
                    "integrator": null
                },
                "actions": [{
                    "id": "deposit",
                    "label": "Deposit 1 USDC",
                    "type": "transaction",
                    "chain": "eip155:421614",
                    "payload": {
                        "type": "evm_transaction",
                        "from": "0xsender",
                        "to": "0xcontract",
                        "data": "0x1234",
                        "value": "0",
                        "gasLimit": "100000",
                        "maxFeePerGas": "2",
                        "maxPriorityFeePerGas": "1"
                    }
                }]
            }]
        });

        let typed: QuoteResponse = serde_json::from_value(response.clone()).unwrap();

        assert_eq!(typed.quotes[0].backend.swap_id(), Some("0xswap"));
        assert_eq!(
            typed.quotes[0].fees.gas.as_ref().unwrap().amount,
            "53338194000000"
        );
        assert_eq!(serde_json::to_value(typed).unwrap(), response);
    }

    #[test]
    fn status_response_round_trips_the_current_rfq_schema() {
        let response = json!({
            "quoteId": "quote-id",
            "state": "REFUNDED",
            "backend": {
                "type": "intent",
                "name": "Axelar Intents",
                "tracking": { "swapId": "0xswap" },
                "metadata": {}
            },
            "source": {
                "chain": "eip155:421614",
                "txHash": "0xsource-tx",
                "messageId": "0xmessage",
                "timestamp": "2026-09-02T13:57:25Z"
            },
            "destination": {
                "chain": "eip155:43113",
                "txHash": "0xdestination-tx",
                "timestamp": "2026-09-02T13:57:28Z"
            },
            "input": {
                "chain": "eip155:421614",
                "token": "0xsource",
                "amount": "1000000"
            },
            "output": {
                "chain": "eip155:43113",
                "token": "0xdestination",
                "amount": "996499"
            },
            "refund": {
                "chain": "eip155:421614",
                "token": "0xsource",
                "amount": "1000000",
                "txHash": "0xrefund"
            },
            "details": { "reason": "test" }
        });

        let typed: StatusResponse = serde_json::from_value(response.clone()).unwrap();

        assert_eq!(typed.source.as_ref().unwrap().message_id, "0xmessage");
        assert_eq!(serde_json::to_value(typed).unwrap(), response);
    }
}
