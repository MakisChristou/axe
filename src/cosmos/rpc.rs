//! LCD REST + Tendermint RPC queries. Everything here is a read against a
//! cosmos node — no signing, no broadcast. Config readers (`read_axelar_*`)
//! live here too because they're "where do I point the LCD client" plumbing.

use std::path::Path;

use alloy::{
    hex,
    primitives::{Address, FixedBytes, U256},
};
use base64::Engine;
use eyre::{Result, WrapErr};
use serde::Deserialize;
use serde_json::Value;

use crate::evm::pubkey_to_address;
use crate::ui;

/// `account` sub-object inside the LCD `/cosmos/auth/v1beta1/accounts`
/// response. The exact set of fields varies by Cosmos SDK version and signer
/// type (BaseAccount, ModuleAccount, etc.), so only the fields we actually
/// use are captured here.
#[derive(Deserialize)]
struct AccountInner {
    #[serde(default)]
    account_number: Option<String>,
    #[serde(default)]
    sequence: Option<String>,
}

/// LCD `/cosmos/bank/v1beta1/balances/{address}/by_denom` response. The
/// `balance` field is `{ denom, amount }`, both as strings.
#[derive(Deserialize)]
struct BalanceResponse {
    balance: Option<Coin>,
}

#[derive(Deserialize)]
struct Coin {
    #[serde(default)]
    amount: Option<String>,
}

/// LCD `/cosmos/tx/v1beta1/simulate` response. The endpoint returns either a
/// top-level `message` (error) or `gas_info.gas_used` (success).
#[derive(Deserialize, Default)]
struct SimulateResponse {
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    gas_info: Option<GasInfo>,
}

#[derive(Deserialize)]
struct GasInfo {
    #[serde(default)]
    gas_used: Option<String>,
}

/// LCD tx-result envelope used by both `lcd_broadcast_tx` and
/// `lcd_wait_for_tx`: `{ "tx_response": { "code": u64, "raw_log": "..." } }`.
/// Only the fields needed for failure detection are captured here; callers
/// receive the raw `Value` to pluck their own fields (`txhash`, `events`,
/// etc.).
#[derive(Deserialize)]
struct TxResultEnvelope {
    tx_response: Option<TxResultBody>,
}

#[derive(Deserialize)]
struct TxResultBody {
    #[serde(default)]
    code: Option<u64>,
    #[serde(default)]
    raw_log: Option<String>,
}

/// LCD `/cosmwasm/wasm/v1/code` paginated listing of code IDs.
#[derive(Deserialize)]
struct CodeListResponse {
    code_infos: Option<Vec<CodeInfo>>,
    #[serde(default)]
    pagination: Option<Pagination>,
}

#[derive(Deserialize)]
struct CodeInfo {
    #[serde(default)]
    code_id: Option<String>,
    #[serde(default)]
    data_hash: Option<String>,
}

#[derive(Deserialize)]
struct Pagination {
    #[serde(default)]
    next_key: Option<String>,
}

/// `target.json` shape, narrowed to the axelar config fields read here. The
/// MultisigProver address lookup is keyed by `chain_axelar_id` at runtime,
/// so it stays a Value lookup inside `fetch_verifier_set`.
#[derive(Deserialize)]
struct AxelarTargetJson {
    axelar: AxelarSection,
}

#[derive(Deserialize)]
struct AxelarSection {
    #[serde(default)]
    rpc: Option<String>,
    #[serde(default)]
    lcd: Option<String>,
    #[serde(default)]
    contracts: Option<Value>,
}

/// Tendermint RPC `block?height=N` response. Only `header.time` is read.
#[derive(Deserialize)]
struct BlockResponse {
    result: Option<BlockResult>,
}

#[derive(Deserialize)]
struct BlockResult {
    block: Option<BlockBody>,
}

#[derive(Deserialize)]
struct BlockBody {
    header: Option<BlockHeader>,
}

#[derive(Deserialize)]
struct BlockHeader {
    #[serde(default)]
    height: Option<String>,
    time: Option<String>,
}

/// Verifier set returned by `current_verifier_set` smart query. The signers
/// map is keyed by an opaque participant id (e.g. consensus address). The
/// numeric `threshold` and per-signer `weight` are kept as raw `Value` so the
/// existing string-or-u64 polymorphism (and its silent fallback to `1` for
/// off-shape weights) survives unchanged.
#[derive(Deserialize)]
struct VerifierSetResponse {
    data: Option<VerifierSetData>,
}

#[derive(Deserialize)]
struct VerifierSetData {
    #[serde(default)]
    id: Option<String>,
    verifier_set: Option<VerifierSet>,
}

#[derive(Deserialize)]
struct VerifierSet {
    signers: Option<std::collections::BTreeMap<String, Signer>>,
    #[serde(default)]
    threshold: Value,
    created_at: Option<u64>,
}

#[derive(Deserialize)]
struct Signer {
    pub_key: Option<PubKey>,
    #[serde(default)]
    weight: Value,
}

#[derive(Deserialize)]
struct PubKey {
    #[serde(default)]
    ecdsa: Option<String>,
}

/// Parse a numeric LCD response field that comes back as a JSON string.
/// Cosmos LCD endpoints often represent u64/u128 as strings (for example,
/// `"sequence": "42"`), and missing or malformed values are response errors.
pub(super) fn parse_required<T>(field: &str, s: Option<&str>) -> Result<T>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    let raw = s.ok_or_else(|| eyre::eyre!("missing numeric LCD field {field}"))?;
    raw.parse()
        .map_err(|e| eyre::eyre!("invalid numeric LCD field {field}={raw}: {e}"))
}

pub(super) async fn lcd_query_account(lcd: &str, address: &str) -> Result<(u64, u64)> {
    let url = format!("{lcd}/cosmos/auth/v1beta1/accounts/{address}");
    let raw: Value = reqwest::get(&url).await?.json().await?;
    if raw.get("account").is_none() {
        return Err(eyre::eyre!("no account in response: {raw}"));
    }
    let account: AccountInner =
        serde_json::from_value(raw["account"].clone()).wrap_err("invalid account response")?;
    let account_number: u64 =
        parse_required("account.account_number", account.account_number.as_deref())?;
    let sequence: u64 = parse_required("account.sequence", account.sequence.as_deref())?;
    Ok((account_number, sequence))
}

/// Query the bank balance of `address` for a single denom. Returns the amount in base units (e.g. uaxl).
pub(super) async fn lcd_query_balance(lcd: &str, address: &str, denom: &str) -> Result<u128> {
    let url = format!("{lcd}/cosmos/bank/v1beta1/balances/{address}/by_denom?denom={denom}");
    let raw: Value = reqwest::get(&url).await?.json().await?;
    let resp: BalanceResponse = serde_json::from_value(raw).wrap_err("invalid balance response")?;
    let balance = resp
        .balance
        .ok_or_else(|| eyre::eyre!("balance response missing balance"))?;
    let amount: u128 = parse_required("balance.amount", balance.amount.as_deref())?;
    Ok(amount)
}

/// Preflight: ensure the Axelar relayer wallet exists on-chain and holds enough fee-denom to relay.
/// Errors with a clear "fund this address" message if the account is missing or balance is below `min_amount`.
/// Macro denom for display: a micro-denom (`uaxl`, 1e6 base) is shown as its
/// whole unit (`AXL`); a denom without the `u` prefix is returned verbatim.
fn display_denom(fee_denom: &str) -> String {
    fee_denom
        .strip_prefix('u')
        .map(str::to_uppercase)
        .unwrap_or_else(|| fee_denom.to_string())
}

pub async fn check_axelar_balance(
    lcd: &str,
    chain_id: &str,
    address: &str,
    fee_denom: &str,
    min_amount: u128,
) -> Result<()> {
    let account_exists = match lcd_query_account(lcd, address).await {
        Ok(_) => true,
        Err(e) if e.to_string().contains("no account in response") => false,
        Err(e) => return Err(e.wrap_err("failed to query Axelar relayer account")),
    };
    let balance = if account_exists {
        lcd_query_balance(lcd, address, fee_denom)
            .await
            .wrap_err("failed to query Axelar relayer balance")?
    } else {
        0
    };

    // Both figures are scaled from micro-units (uaxl) to whole units for
    // display, so label them with the macro denom (uaxl -> AXL) rather than the
    // raw `fee_denom`. The comparison itself stays in raw micro-units.
    let display = balance as f64 / 1_000_000.0;
    let min_display = min_amount as f64 / 1_000_000.0;
    let unit = display_denom(fee_denom);

    if !account_exists || balance < min_amount {
        ui::error(&format!("axelar relayer wallet underfunded on {chain_id}:"));
        ui::error(&format!("  address: {address}"));
        ui::error(&format!(
            "  balance: {display:.6} {unit} (need >= {min_display:.6})"
        ));
        if !account_exists {
            ui::error("  account does not exist on-chain — send any amount to create it");
        }
        return Err(eyre::eyre!(
            "fund {address} with at least {min_display:.6} {unit} on {chain_id} and retry"
        ));
    }

    ui::kv(
        "relayer balance",
        &format!("{display:.6} {unit} (>= {min_display:.6})"),
    );
    Ok(())
}

pub(super) async fn lcd_simulate_tx(lcd: &str, tx_bytes: &[u8]) -> Result<u64> {
    let tx_b64 = base64::engine::general_purpose::STANDARD.encode(tx_bytes);
    let body = serde_json::json!({
        "tx_bytes": tx_b64,
        "mode": "BROADCAST_MODE_UNSPECIFIED"
    });
    let client = reqwest::Client::new();
    let raw: Value = client
        .post(format!("{lcd}/cosmos/tx/v1beta1/simulate"))
        .json(&body)
        .send()
        .await?
        .json()
        .await?;
    let resp: SimulateResponse =
        serde_json::from_value(raw.clone()).wrap_err("invalid simulation response")?;
    if let Some(err) = resp.message.as_deref()
        && !err.is_empty()
    {
        return Err(eyre::eyre!("simulation failed: {err}"));
    }
    let gas_info = resp
        .gas_info
        .ok_or_else(|| eyre::eyre!("simulation response missing gas_info"))?;
    let gas_used: u64 = parse_required("gas_info.gas_used", gas_info.gas_used.as_deref())?;
    if gas_used == 0 {
        return Err(eyre::eyre!(
            "simulation returned 0 gas — response: {}",
            serde_json::to_string_pretty(&raw)?
        ));
    }
    Ok(gas_used)
}

pub(super) async fn lcd_broadcast_tx(lcd: &str, tx_bytes: &[u8]) -> Result<Value> {
    let tx_b64 = base64::engine::general_purpose::STANDARD.encode(tx_bytes);
    let body = serde_json::json!({
        "tx_bytes": tx_b64,
        "mode": "BROADCAST_MODE_SYNC"
    });
    let client = reqwest::Client::new();
    let resp: Value = client
        .post(format!("{lcd}/cosmos/tx/v1beta1/txs"))
        .json(&body)
        .send()
        .await?
        .json()
        .await?;
    let envelope: TxResultEnvelope =
        serde_json::from_value(resp.clone()).wrap_err("invalid broadcast response")?;
    let tx_response = envelope
        .tx_response
        .ok_or_else(|| eyre::eyre!("broadcast response missing tx_response"))?;
    let code = tx_response
        .code
        .ok_or_else(|| eyre::eyre!("broadcast response missing tx_response.code"))?;
    if code != 0 {
        let raw_log = tx_response.raw_log.as_deref().unwrap_or("unknown error");
        return Err(eyre::eyre!("broadcast failed (code {code}): {raw_log}"));
    }
    Ok(resp)
}

/// Wait for a tx to be included in a block and return the full tx response with events.
pub(super) async fn lcd_wait_for_tx(lcd: &str, tx_hash: &str) -> Result<Value> {
    for _ in 0..crate::timing::LCD_WAIT_MAX_ATTEMPTS {
        tokio::time::sleep(crate::timing::LCD_WAIT_RETRY_INTERVAL).await;
        let url = format!("{lcd}/cosmos/tx/v1beta1/txs/{tx_hash}");
        let resp: Value = match reqwest::get(&url).await {
            Ok(r) => r.json().await.wrap_err("invalid tx lookup response")?,
            Err(_) => continue,
        };
        if resp.get("tx_response").is_some() {
            let envelope: TxResultEnvelope =
                serde_json::from_value(resp.clone()).wrap_err("invalid tx lookup response")?;
            let tx_response = envelope
                .tx_response
                .ok_or_else(|| eyre::eyre!("tx lookup response missing tx_response"))?;
            let code = tx_response
                .code
                .ok_or_else(|| eyre::eyre!("tx lookup response missing tx_response.code"))?;
            if code != 0 {
                let raw_log = tx_response.raw_log.as_deref().unwrap_or("unknown");
                return Err(eyre::eyre!("tx failed (code {code}): {raw_log}"));
            }
            return Ok(resp);
        }
    }
    Err(eyre::eyre!("timeout waiting for tx {tx_hash}"))
}

pub async fn lcd_query_proposal(lcd: &str, proposal_id: u64) -> Result<Value> {
    let url = format!("{lcd}/cosmos/gov/v1/proposals/{proposal_id}");
    let resp: Value = reqwest::get(&url).await?.json().await?;
    resp.get("proposal")
        .cloned()
        .ok_or_else(|| eyre::eyre!("no 'proposal' field in response"))
}

/// Public Axelar LCD endpoints used as silent fallbacks when the primary
/// (chain-config or env-override) endpoint is unreachable. Imperator's
/// mainnet LCD has been flapping with 502s; lavenderfive and polkachu are
/// healthy alternatives at the time of writing.
const LCD_FALLBACKS_MAINNET: &[&str] = &[
    "https://rest.lavenderfive.com/axelar",
    "https://axelar-rest.publicnode.com",
];

/// Public Axelar testnet LCD endpoints used the same way as
/// `LCD_FALLBACKS_MAINNET`. qubelabs (the default in chain configs) returns
/// HTTP 500 for "message not yet routed" instead of a proper empty response,
/// so a failover list is essential for testnet runs. Probe with curl before
/// adding entries — many advertised public testnet LCDs are unreachable,
/// geo-blocked (`403 administrative rules`), or behind paid API keys.
const LCD_FALLBACKS_TESTNET: &[&str] = &["https://axelar-testnet-api.polkachu.com"];

/// Returns the fallback LCD list appropriate for a given primary endpoint,
/// or an empty slice when no public fallbacks exist (stagenet/devnet).
fn lcd_fallbacks_for(primary: &str) -> &'static [&'static str] {
    if primary.contains("testnet") {
        LCD_FALLBACKS_TESTNET
    } else if primary.contains("stagenet") || primary.contains("devnet") {
        &[]
    } else {
        LCD_FALLBACKS_MAINNET
    }
}

/// `OnceLock` flag for the LCD fallback warning. We emit one ui::warn the
/// first time the primary LCD goes unhealthy in a process, then stay quiet
/// — repeating per-call would flood the load-test report log with the same
/// message hundreds of times.
static LCD_FALLBACK_WARNED: std::sync::OnceLock<()> = std::sync::OnceLock::new();

/// Print a one-time warning when an LCD response came from a fallback
/// endpoint instead of the user-configured primary. `idx` is the position
/// in the candidate list — `0` is the primary (no warning), anything ≥ 1 is
/// a fallback. The endpoint URL is intentionally NOT logged — it can be a
/// private/paid endpoint from a repo secret.
fn note_lcd_fallback_use(idx: usize, last_err: Option<&CosmwasmQueryError>) {
    if idx == 0 {
        return;
    }
    if LCD_FALLBACK_WARNED.set(()).is_err() {
        return;
    }
    let cause = last_err
        .map(|e| {
            let s = e.to_string();
            s.lines().next().unwrap_or("").to_string()
        })
        .unwrap_or_else(|| "primary unreachable".to_string());
    ui::warn(&format!(
        "Axelar LCD primary unhealthy ({cause}); using fallback LCD for the rest of this run"
    ));
}

#[derive(Debug, thiserror::Error)]
pub enum CosmwasmQueryError {
    #[error("failed to encode CosmWasm smart query: {0}")]
    Encode(#[source] serde_json::Error),
    #[error("{role} request failed: {source}")]
    Transport {
        role: &'static str,
        #[source]
        source: reqwest::Error,
    },
    #[error("{role} body read failed: {source}")]
    BodyRead {
        role: &'static str,
        #[source]
        source: reqwest::Error,
    },
    #[error("{role} returned HTTP {status}. First 200 chars of body: {body}")]
    Http {
        role: &'static str,
        status: reqwest::StatusCode,
        body: String,
    },
    #[error("{role} returned non-JSON body. First 200 chars: {body}\nParse error: {source}")]
    InvalidJson {
        role: &'static str,
        body: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("{role} response missing data field. First 200 chars: {body}")]
    MissingData { role: &'static str, body: String },
    #[error("LCD query exhausted all endpoints")]
    Exhausted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CosmwasmQueryPending {
    OutgoingMessage,
    ExecutableMessage,
}

impl CosmwasmQueryError {
    /// Contract-query error payload, if the server returned one. Verification
    /// code may classify known contract-level "not ready yet" responses
    /// without accidentally treating transport or malformed-data errors as
    /// normal pending states.
    pub fn contract_error_body(&self) -> Option<&str> {
        match self {
            Self::Http { body, .. } => Some(body),
            _ => None,
        }
    }

    pub fn is_pending(&self, query: CosmwasmQueryPending) -> bool {
        let Some(body) = self.contract_error_body() else {
            return false;
        };
        let body = body.to_lowercase();
        match query {
            CosmwasmQueryPending::OutgoingMessage => {
                body.contains("failed to query outgoing messages")
                    || body.contains("message with id") && body.contains("not found")
            }
            CosmwasmQueryPending::ExecutableMessage => {
                body.contains("not approved")
                    || body.contains("failed to query executable messages")
            }
        }
    }
}

pub async fn lcd_cosmwasm_smart_query(
    lcd: &str,
    contract: &str,
    query_msg: &Value,
) -> Result<Value> {
    lcd_cosmwasm_smart_query_typed(lcd, contract, query_msg)
        .await
        .map_err(eyre::Report::new)
        .wrap_err(
            "Tip: set AXELAR_LCD_URL to a working endpoint (e.g. \
             `https://rest.lavenderfive.com/axelar` for mainnet).",
        )
}

pub async fn lcd_cosmwasm_smart_query_typed(
    lcd: &str,
    contract: &str,
    query_msg: &Value,
) -> std::result::Result<Value, CosmwasmQueryError> {
    let user_override = std::env::var("AXELAR_LCD_URL").ok();
    let primary = user_override
        .clone()
        .unwrap_or_else(|| lcd.to_string())
        .trim_end_matches('/')
        .to_string();

    // Try the primary endpoint first; on transient failures (HTTP 5xx, network
    // error, non-JSON body) silently fall through to known-good public
    // endpoints. Only the user-set AXELAR_LCD_URL skips fallback — we honor
    // their explicit choice and surface the error directly.
    let mut candidates: Vec<String> = vec![primary.clone()];
    if user_override.is_none() {
        for fb in lcd_fallbacks_for(&primary) {
            if *fb != primary {
                candidates.push((*fb).to_string());
            }
        }
    }

    let query_json = serde_json::to_string(query_msg).map_err(CosmwasmQueryError::Encode)?;
    let query_b64 = base64::engine::general_purpose::STANDARD.encode(query_json.as_bytes());
    let mut last_err: Option<CosmwasmQueryError> = None;

    for (idx, endpoint) in candidates.iter().enumerate() {
        // Role label keeps logs / report files free of endpoint URLs (which
        // can be private / paid endpoints supplied via repo secrets).
        let role = if idx == 0 {
            "primary LCD"
        } else {
            "fallback LCD"
        };
        let url = format!("{endpoint}/cosmwasm/wasm/v1/contract/{contract}/smart/{query_b64}");
        match reqwest::get(&url).await {
            Ok(response) => {
                let status = response.status();
                match response.text().await {
                    Ok(body) => {
                        if !status.is_success() {
                            last_err = Some(CosmwasmQueryError::Http {
                                role,
                                status,
                                body: body.chars().take(200).collect(),
                            });
                            continue;
                        }
                        match serde_json::from_str::<Value>(&body) {
                            Ok(resp) => {
                                let data = resp.get("data").cloned().ok_or_else(|| {
                                    CosmwasmQueryError::MissingData {
                                        role,
                                        body: body.chars().take(200).collect(),
                                    }
                                });
                                let data = match data {
                                    Ok(data) => data,
                                    Err(e) => {
                                        last_err = Some(e);
                                        continue;
                                    }
                                };
                                note_lcd_fallback_use(idx, last_err.as_ref());
                                return Ok(data);
                            }
                            Err(e) => {
                                last_err = Some(CosmwasmQueryError::InvalidJson {
                                    role,
                                    body: body.chars().take(200).collect(),
                                    source: e,
                                });
                                continue;
                            }
                        }
                    }
                    Err(e) => {
                        last_err = Some(CosmwasmQueryError::BodyRead { role, source: e });
                        continue;
                    }
                }
            }
            Err(e) => {
                last_err = Some(CosmwasmQueryError::Transport { role, source: e });
                continue;
            }
        }
    }

    Err(last_err.unwrap_or(CosmwasmQueryError::Exhausted))
}

/// Fetch code IDs by matching storeCodeProposalCodeHash against on-chain checksums.
pub async fn lcd_fetch_code_id(lcd: &str, expected_checksum: &str) -> Result<u64> {
    let expected = expected_checksum.to_uppercase();
    let mut next_key: Option<String> = None;
    loop {
        let mut url =
            format!("{lcd}/cosmwasm/wasm/v1/code?pagination.limit=100&pagination.reverse=true");
        if let Some(ref key) = next_key {
            url.push_str(&format!("&pagination.key={key}"));
        }
        let raw: Value = reqwest::get(&url).await?.json().await?;
        let resp: CodeListResponse =
            serde_json::from_value(raw).wrap_err("invalid code list response")?;
        let codes = resp
            .code_infos
            .ok_or_else(|| eyre::eyre!("no code_infos in response"))?;
        for code in &codes {
            let checksum = code.data_hash.as_deref().unwrap_or("").to_uppercase();
            if checksum == expected {
                let code_id: u64 = parse_required("code_infos[].code_id", code.code_id.as_deref())?;
                return Ok(code_id);
            }
        }
        let nk = resp.pagination.and_then(|p| p.next_key).unwrap_or_default();
        if nk.is_empty() {
            break;
        }
        next_key = Some(nk);
    }
    Err(eyre::eyre!(
        "code not found for checksum {expected_checksum}"
    ))
}

/// Read Axelar LCD url, chain ID, fee denom, and gas price from target json.
/// Errors if any of the fields are missing — silent defaults previously
/// masked config drift (e.g. a missing `gasPrice` falling back to
/// `0.007uaxl`), so callers should always hit a real on-disk config.
pub async fn read_axelar_config(target_json: &Path) -> Result<(String, String, String, f64)> {
    crate::config::ChainsConfig::load(target_json)
        .await?
        .axelar
        .cosmos_tx_params()
}

/// Read a string field from axelar contracts config
pub async fn read_axelar_contract_field(target_json: &Path, pointer: &str) -> Result<String> {
    let content = tokio::fs::read_to_string(target_json).await?;
    let root: Value = serde_json::from_str(&content)?;
    root.pointer(pointer)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| eyre::eyre!("field not found: {pointer}"))
}

/// Read Axelar RPC url from target json (`/axelar/rpc`).
pub async fn read_axelar_rpc(target_json: &Path) -> Result<String> {
    let content = tokio::fs::read_to_string(target_json).await?;
    // Parse to `Value` first so a malformed-JSON file surfaces serde's parse
    // error verbatim, matching the original `from_str::<Value>(&content)?`.
    let raw: Value = serde_json::from_str(&content)?;
    let root: AxelarTargetJson =
        serde_json::from_value(raw).map_err(|_| eyre::eyre!("no axelar.rpc in target json"))?;
    root.axelar
        .rpc
        .ok_or_else(|| eyre::eyre!("no axelar.rpc in target json"))
}

/// Public Axelar Tendermint RPC endpoints used as silent fallbacks when the
/// primary endpoint (from chain config or `AXELAR_RPC_URL`) is unreachable.
/// Imperator's mainnet RPC has been flapping with 502s; these are healthy
/// alternatives at the time of writing.
const RPC_FALLBACKS_MAINNET: &[&str] = &[
    "https://axelar-rpc.publicnode.com",
    "https://rpc.cosmos.directory/axelar",
];

/// Public Axelar testnet Tendermint RPC fallbacks. qubelabs (default) flaps;
/// polkachu's testnet RPC is the only consistently healthy public endpoint at
/// the time of writing. Probe with curl before adding entries.
const RPC_FALLBACKS_TESTNET: &[&str] = &["https://axelar-testnet-rpc.polkachu.com"];

/// Returns the fallback RPC list appropriate for a given primary endpoint.
/// Stagenet/devnet have no public fallbacks.
fn rpc_fallbacks_for(primary: &str) -> &'static [&'static str] {
    if primary.contains("testnet") {
        RPC_FALLBACKS_TESTNET
    } else if primary.contains("stagenet") || primary.contains("devnet") {
        &[]
    } else {
        RPC_FALLBACKS_MAINNET
    }
}

/// `OnceLock` flag matching `LCD_FALLBACK_WARNED` for the Tendermint RPC
/// side — same once-per-process semantics so a flapping primary doesn't
/// flood the report log.
static RPC_FALLBACK_WARNED: std::sync::OnceLock<()> = std::sync::OnceLock::new();

fn note_rpc_fallback_use(idx: usize, last_err: Option<&eyre::Report>) {
    if idx == 0 {
        return;
    }
    if RPC_FALLBACK_WARNED.set(()).is_err() {
        return;
    }
    let cause = last_err
        .map(|e| {
            let s = e.to_string();
            s.lines().next().unwrap_or("").to_string()
        })
        .unwrap_or_else(|| "primary unreachable".to_string());
    ui::warn(&format!(
        "Axelar Tendermint RPC primary unhealthy ({cause}); using fallback for the rest of this run"
    ));
}

/// Query Tendermint RPC `tx_search` for a single event key/value pair.
/// Returns the parsed JSON response. Silently falls back to public RPCs if
/// the primary endpoint errors and `AXELAR_RPC_URL` is not explicitly set.
pub async fn rpc_tx_search_event(rpc: &str, event_key: &str, event_value: &str) -> Result<Value> {
    let user_override = std::env::var("AXELAR_RPC_URL").ok();
    let primary = user_override
        .clone()
        .unwrap_or_else(|| rpc.to_string())
        .trim_end_matches('/')
        .to_string();

    let mut candidates: Vec<String> = vec![primary.clone()];
    if user_override.is_none() {
        for fb in rpc_fallbacks_for(&primary) {
            if *fb != primary {
                candidates.push((*fb).to_string());
            }
        }
    }

    let query = format!("\"{event_key}='{event_value}'\"");
    let encoded = url_encode_query(&query);
    let mut last_err: Option<eyre::Report> = None;

    for (idx, endpoint) in candidates.iter().enumerate() {
        // Role label keeps URLs out of logs / report (see lcd_cosmwasm_smart_query).
        let role = if idx == 0 {
            "primary Tendermint RPC"
        } else {
            "fallback Tendermint RPC"
        };
        let url = format!("{endpoint}/tx_search?query={encoded}&per_page=1");
        match reqwest::get(&url).await {
            Ok(response) => {
                let status = response.status();
                if !status.is_success() {
                    last_err = Some(eyre::eyre!("{role} returned HTTP {status}"));
                    continue;
                }
                match response.json::<Value>().await {
                    Ok(v) => {
                        note_rpc_fallback_use(idx, last_err.as_ref());
                        return Ok(v);
                    }
                    Err(e) => {
                        last_err = Some(eyre::eyre!("{role} JSON decode failed: {e}"));
                        continue;
                    }
                }
            }
            Err(e) => {
                last_err = Some(eyre::eyre!("{role} request failed: {e}"));
                continue;
            }
        }
    }

    Err(last_err.unwrap_or_else(|| eyre::eyre!("Tendermint RPC query exhausted all endpoints")))
        .map_err(|e| {
            eyre::eyre!(
                "{e}\nTip: set AXELAR_RPC_URL to a working endpoint (e.g. \
                 `https://axelar-rpc.publicnode.com` for mainnet)."
            )
        })
}

fn url_encode_query(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Query Tendermint RPC `tx_search` with a raw query string (e.g.
/// `key1='value' AND key2='value'`). Returns the parsed `result` payload (with
/// keys `total_count`, `txs`).
pub async fn rpc_tx_search(
    rpc: &str,
    query: &str,
    per_page: u32,
    page: u32,
    order_desc: bool,
) -> Result<Value> {
    let json_quoted = serde_json::to_string(query)?;
    let order = if order_desc { "\"desc\"" } else { "\"asc\"" };
    let resp = reqwest::Client::new()
        .get(format!("{rpc}/tx_search"))
        .query(&[
            ("query", json_quoted.as_str()),
            ("per_page", &per_page.to_string()),
            ("page", &page.to_string()),
            ("order_by", order),
        ])
        .send()
        .await?
        .json::<Value>()
        .await?;
    resp.get("result")
        .cloned()
        .ok_or_else(|| eyre::eyre!("tx_search response missing result"))
}

/// Query Tendermint RPC `block` endpoint for a given height. Returns block.header.time.
pub async fn rpc_block_time(rpc: &str, height: u64) -> Result<String> {
    let url = format!("{rpc}/block?height={height}");
    let resp: BlockResponse = reqwest::get(&url).await?.json().await?;
    resp.result
        .and_then(|r| r.block)
        .and_then(|b| b.header)
        .and_then(|h| h.time)
        .ok_or_else(|| {
            eyre::eyre!("RPC response missing /result/block/header/time at height {height}")
        })
}

/// Query Tendermint RPC `block` endpoint. Pass `Some(N)` to fetch a specific
/// height, or `None` to fetch the latest block. Returns `(height, time)` where
/// `time` is the raw RFC3339 timestamp from the header. Silently falls back
/// to public RPCs when the configured primary errors and `AXELAR_RPC_URL` is
/// not explicitly set, matching `rpc_tx_search` behaviour.
pub async fn rpc_block_info(rpc: &str, height: Option<u64>) -> Result<(u64, String)> {
    let user_override = std::env::var("AXELAR_RPC_URL").ok();
    let primary = user_override
        .clone()
        .unwrap_or_else(|| rpc.to_string())
        .trim_end_matches('/')
        .to_string();

    let mut candidates: Vec<String> = vec![primary.clone()];
    if user_override.is_none() {
        for fb in rpc_fallbacks_for(&primary) {
            if *fb != primary {
                candidates.push((*fb).to_string());
            }
        }
    }

    let path = match height {
        Some(h) => format!("block?height={h}"),
        None => "block".to_string(),
    };

    let mut last_err: Option<eyre::Report> = None;
    for (idx, endpoint) in candidates.iter().enumerate() {
        let role = if idx == 0 {
            "primary Tendermint RPC"
        } else {
            "fallback Tendermint RPC"
        };
        let url = format!("{endpoint}/{path}");
        let response = match reqwest::get(&url).await {
            Ok(r) => r,
            Err(e) => {
                last_err = Some(eyre::eyre!("{role} request failed: {e}"));
                continue;
            }
        };
        let status = response.status();
        if !status.is_success() {
            last_err = Some(eyre::eyre!("{role} returned HTTP {status}"));
            continue;
        }
        let resp: BlockResponse = match response.json().await {
            Ok(v) => v,
            Err(e) => {
                last_err = Some(eyre::eyre!("{role} JSON decode failed: {e}"));
                continue;
            }
        };
        let Some(header) = resp.result.and_then(|r| r.block).and_then(|b| b.header) else {
            last_err = Some(eyre::eyre!("{role} response missing /result/block/header"));
            continue;
        };
        let Some(time) = header.time else {
            last_err = Some(eyre::eyre!("{role} response missing block header time"));
            continue;
        };
        let Some(height_str) = header.height else {
            last_err = Some(eyre::eyre!("{role} response missing block header height"));
            continue;
        };
        let height: u64 = parse_required("block.header.height", Some(&height_str))?;
        note_rpc_fallback_use(idx, last_err.as_ref());
        return Ok((height, time));
    }

    Err(last_err
        .unwrap_or_else(|| eyre::eyre!("Tendermint RPC block query exhausted all endpoints")))
    .map_err(|e| {
        eyre::eyre!(
            "{e}\nTip: set AXELAR_RPC_URL to a working endpoint (e.g. \
                 `https://axelar-rpc.publicnode.com` for mainnet)."
        )
    })
}

/// Fetch the current verifier set from Axelar chain via LCD REST endpoint.
/// Returns (signers sorted by address, threshold, nonce, verifierSetId)
pub async fn fetch_verifier_set(
    target_json: &Path,
    chain_axelar_id: &str,
) -> Result<(Vec<(Address, u128)>, u128, FixedBytes<32>, String)> {
    let content = tokio::fs::read_to_string(target_json).await?;
    // Parse to `Value` first so a malformed-JSON file surfaces serde's parse
    // error verbatim, matching the original `from_str::<Value>(&content)?`.
    let raw: Value = serde_json::from_str(&content)?;
    let root: AxelarTargetJson =
        serde_json::from_value(raw).map_err(|_| eyre::eyre!("no axelar.lcd in target json"))?;

    let lcd = root
        .axelar
        .lcd
        .as_deref()
        .ok_or_else(|| eyre::eyre!("no axelar.lcd in target json"))?;

    // The MultisigProver address is keyed by `chain_axelar_id` at runtime, so
    // its lookup stays a `Value::pointer` — the contracts map is dynamically
    // shaped (different contracts have different per-chain layouts).
    let contracts = root.axelar.contracts.unwrap_or(Value::Null);
    let prover_addr = contracts
        .pointer(&format!("/MultisigProver/{chain_axelar_id}/address"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| eyre::eyre!("no MultisigProver.{chain_axelar_id}.address in target json"))?;

    let query_msg = "\"current_verifier_set\"";
    let query_b64 = base64::engine::general_purpose::STANDARD.encode(query_msg.as_bytes());

    let url = format!("{lcd}/cosmwasm/wasm/v1/contract/{prover_addr}/smart/{query_b64}");
    ui::info(&format!("fetching verifier set from: {url}"));

    let resp: VerifierSetResponse = reqwest::get(&url).await?.json().await?;

    let data = resp
        .data
        .ok_or_else(|| eyre::eyre!("no id in verifier set response"))?;
    let verifier_set_id = data
        .id
        .ok_or_else(|| eyre::eyre!("no id in verifier set response"))?;

    let verifier_set = data
        .verifier_set
        .ok_or_else(|| eyre::eyre!("no signers object in verifier set"))?;
    let signers_obj = verifier_set
        .signers
        .ok_or_else(|| eyre::eyre!("no signers object in verifier set"))?;

    let threshold = if let Some(value) = verifier_set.threshold.as_str() {
        value
            .parse::<u128>()
            .map_err(|error| eyre::eyre!("invalid threshold: {error}"))?
    } else if let Some(value) = verifier_set.threshold.as_u64() {
        u128::from(value)
    } else {
        return Err(eyre::eyre!("no threshold in verifier set"));
    };

    let created_at = verifier_set
        .created_at
        .ok_or_else(|| eyre::eyre!("no created_at in verifier set"))?;

    let nonce = FixedBytes::<32>::from(U256::from(created_at).to_be_bytes::<32>());

    let mut weighted_signers: Vec<(Address, u128)> = Vec::new();

    for signer in signers_obj.values() {
        let pubkey_hex = signer
            .pub_key
            .as_ref()
            .and_then(|p| p.ecdsa.as_deref())
            .ok_or_else(|| eyre::eyre!("no pub_key.ecdsa for signer"))?;

        let weight: u128 = if let Some(raw) = signer.weight.as_str() {
            raw.parse::<u128>()
                .map_err(|e| eyre::eyre!("invalid weight {raw}: {e}"))?
        } else if let Some(raw) = signer.weight.as_u64() {
            raw as u128
        } else {
            return Err(eyre::eyre!("missing or invalid verifier signer weight"));
        };

        let pubkey_bytes = hex::decode(pubkey_hex.strip_prefix("0x").unwrap_or(pubkey_hex))?;
        let addr = pubkey_to_address(&pubkey_bytes)?;
        weighted_signers.push((addr, weight));
    }

    weighted_signers.sort_by_key(|(addr, _)| *addr);

    ui::kv(
        "verifier set",
        &format!(
            "{} signers, threshold={}, created_at={}, id={}",
            weighted_signers.len(),
            threshold,
            created_at,
            verifier_set_id
        ),
    );
    for (addr, weight) in &weighted_signers {
        ui::kv(&format!("{addr}"), &format!("weight={weight}"));
    }

    Ok((weighted_signers, threshold, nonce, verifier_set_id))
}

#[cfg(test)]
mod tests {
    use super::{CosmwasmQueryError, CosmwasmQueryPending};

    fn contract_error(body: &str) -> CosmwasmQueryError {
        CosmwasmQueryError::Http {
            role: "primary LCD",
            status: reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            body: body.to_string(),
        }
    }

    #[test]
    fn classifies_only_matching_contract_responses_as_pending() {
        assert!(
            contract_error("failed to query outgoing messages: message with ID abc not found")
                .is_pending(CosmwasmQueryPending::OutgoingMessage)
        );
        assert!(
            contract_error("failed to query executable messages: message is not approved")
                .is_pending(CosmwasmQueryPending::ExecutableMessage)
        );
        assert!(
            !contract_error("failed to query executable messages")
                .is_pending(CosmwasmQueryPending::OutgoingMessage)
        );
    }

    #[test]
    fn transport_and_malformed_failures_are_never_pending() {
        let malformed = CosmwasmQueryError::MissingData {
            role: "primary LCD",
            body: "failed to query outgoing messages".to_string(),
        };

        assert!(!malformed.is_pending(CosmwasmQueryPending::OutgoingMessage));
        assert!(!CosmwasmQueryError::Exhausted.is_pending(CosmwasmQueryPending::ExecutableMessage));
    }
}
