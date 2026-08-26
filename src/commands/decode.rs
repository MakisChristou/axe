use alloy::dyn_abi::{DynSolType, DynSolValue, JsonAbiExt, Specifier};
use alloy::hex;
use alloy::json_abi::JsonAbi;
use alloy::primitives::{Address, B256, U256};
use eyre::{Result, bail};
use owo_colors::OwoColorize;
use std::collections::HashMap;
use std::sync::LazyLock;

const ABI_JSON: &str = include_str!("../../abi-db.json");

static FUNC_DB: LazyLock<std::result::Result<HashMap<[u8; 4], alloy::json_abi::Function>, String>> =
    LazyLock::new(|| {
        let abi: JsonAbi = serde_json::from_str(ABI_JSON).map_err(|error| error.to_string())?;
        let mut map = HashMap::new();
        for funcs in abi.functions.values() {
            for func in funcs {
                map.insert(func.selector().0, func.clone());
            }
        }
        Ok(map)
    });

static EVENT_DB: LazyLock<std::result::Result<HashMap<B256, alloy::json_abi::Event>, String>> =
    LazyLock::new(|| {
        let abi: JsonAbi = serde_json::from_str(ABI_JSON).map_err(|error| error.to_string())?;
        let mut map = HashMap::new();
        for events in abi.events.values() {
            for event in events {
                map.insert(event.selector(), event.clone());
            }
        }
        Ok(map)
    });

fn function_db() -> Result<&'static HashMap<[u8; 4], alloy::json_abi::Function>> {
    FUNC_DB
        .as_ref()
        .map_err(|error| eyre::eyre!("embedded ABI is invalid: {error}"))
}

const MAX_DEPTH: usize = 5;

pub fn run(calldata_hex: &str) -> Result<()> {
    let data = parse_hex(calldata_hex)?;
    decode_bytes(&data, "")
}

/// A decoded function call: what it is, and its named arguments.
///
/// Only the function-selector branch of the decoder is represented here. The
/// exotic payload shapes -- ITS hub frames, governance proposals, plain ITS
/// payloads and the fallback patterns -- are still printer-only, and giving
/// them structure is the remainder of the decoder restructuring work.
#[derive(Debug, serde::Serialize)]
pub(crate) struct DecodedCall {
    /// The full solidity signature, for example `transfer(address,uint256)`.
    pub signature: String,
    pub arguments: Vec<DecodedArgument>,
}

/// One argument of a decoded call.
#[derive(Debug, serde::Serialize)]
pub(crate) struct DecodedArgument {
    /// The parameter name, or `argN` when the ABI does not name it.
    pub name: String,
    /// The solidity type, for example `uint256`.
    #[serde(rename = "type")]
    pub ty: String,
    pub value: serde_json::Value,
}

/// Decode calldata as a known function call, without printing.
///
/// Returns `None` when the leading four bytes are not a selector in the
/// embedded ABI database, which is also the case for every non-call payload
/// shape. A caller that gets `None` has learned that this is not a plain
/// function call, not that the bytes are undecodable.
///
/// Deliberately additive: the printer above is untouched, so the CLI's output
/// cannot change as a result of this existing.
pub(crate) fn decode_call(data: &[u8]) -> Result<Option<DecodedCall>> {
    if data.len() < 4 {
        return Ok(None);
    }

    let selector = <[u8; 4]>::try_from(&data[..4])?;
    let Some(func) = function_db()?.get(&selector) else {
        return Ok(None);
    };

    let values = func.abi_decode_input(&data[4..])?;
    let arguments = func
        .inputs
        .iter()
        .zip(values.iter())
        .enumerate()
        .map(|(i, (param, value))| DecodedArgument {
            name: if param.name.is_empty() {
                format!("arg{i}")
            } else {
                param.name.clone()
            },
            ty: param.ty.clone(),
            value: super::decode_evm_activity::sol_value_to_json(value),
        })
        .collect();

    Ok(Some(DecodedCall {
        signature: func.signature(),
        arguments,
    }))
}

/// What a payload turned out to be.
///
/// The variants mirror the shapes the printer recognises, and are tried in the
/// same order it tries them, so a structured answer always agrees with what
/// the CLI would print for the same bytes.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub(crate) enum DecodedPayload {
    /// A call whose selector is in the embedded ABI database.
    FunctionCall(DecodedCall),
    /// An Interchain Token Service message. Hub frames are ITS message types
    /// 3 and 4, so they land here too.
    ItsMessage {
        name: &'static str,
        fields: Vec<DecodedArgument>,
    },
    /// An AxelarServiceGovernance proposal payload.
    GovernanceProposal {
        command: u64,
        command_name: &'static str,
        target: String,
        native_value: String,
        eta: u64,
        /// The inner call, when its selector is known.
        call: Option<DecodedCall>,
    },
    /// Printable text.
    Text { text: String },
    /// Recognised by none of the above. The CLI has further fallback patterns
    /// that are still printer-only, so `axe decode` may still say something
    /// useful about these bytes.
    Unrecognised { reason: &'static str },
}

/// Decode a payload without printing, trying the shapes in the printer's order.
pub(crate) fn decode_payload(data: &[u8]) -> Result<DecodedPayload> {
    if let Some(call) = decode_call(data)? {
        return Ok(DecodedPayload::FunctionCall(call));
    }

    // Hub frames first: a hub frame's bytes offset also lands at 0xa0, so
    // governance would otherwise claim it.
    let is_hub_frame = matches!(its_message_type(data), Some(3 | 4));
    if is_hub_frame && let Some(its) = decode_its(data) {
        return Ok(its);
    }

    // Governance before plain ITS: command 0 collides with ITS msgType 0.
    if let Some(proposal) = parse_governance(data) {
        return Ok(DecodedPayload::GovernanceProposal {
            command: proposal.command,
            command_name: proposal.command_name,
            target: format!("0x{:x}", proposal.target),
            native_value: proposal.native_value.to_string(),
            eta: proposal.eta.saturating_to(),
            call: decode_call(&proposal.call_data)?,
        });
    }

    if let Some(its) = decode_its(data) {
        return Ok(its);
    }

    if let Ok(text) = std::str::from_utf8(data)
        && !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_graphic() || c.is_ascii_whitespace())
    {
        return Ok(DecodedPayload::Text {
            text: text.to_string(),
        });
    }

    Ok(DecodedPayload::Unrecognised {
        reason: "not a known selector, ITS message, governance proposal or text",
    })
}

/// An ITS message as structured fields, reusing the printer's own parser.
fn decode_its(data: &[u8]) -> Option<DecodedPayload> {
    let ItsMessage {
        name,
        types,
        labels,
        values,
    } = parse_its(data)?;

    let fields = values
        .iter()
        .zip(labels.iter().zip(types.iter()))
        .map(|(value, (label, ty))| DecodedArgument {
            name: (*label).to_string(),
            ty: ty.to_string(),
            value: super::decode_evm_activity::sol_value_to_json(value),
        })
        .collect();

    Some(DecodedPayload::ItsMessage { name, fields })
}

/// Parse a hex string and decode whatever shape it turns out to be.
pub(crate) fn decode_payload_hex(payload_hex: &str) -> Result<DecodedPayload> {
    decode_payload(&parse_hex(payload_hex)?)
}

pub(crate) fn decode_bytes(data: &[u8], indent: &str) -> Result<()> {
    // Try as function call (4-byte selector lookup)
    if data.len() >= 4
        && let Some(func) = function_db()?.get(&<[u8; 4]>::try_from(&data[..4])?)
    {
        let values = func.abi_decode_input(&data[4..])?;
        println!("{indent}{}", func.signature().bold());
        print_decoded(&func.inputs, &values, &format!("{indent}  "), 0);
        return Ok(());
    }

    // Try as ITS hub frame (before governance: a hub frame's bytes offset also lands at 0xa0)
    if try_print_its_hub(data, indent, 0) {
        return Ok(());
    }

    // Try as a governance proposal payload (before plain ITS: command 0 collides with ITS msgType 0)
    if try_print_governance(data, indent, 0) {
        return Ok(());
    }

    // Try as ITS payload
    if try_print_its(data, indent, 0) {
        return Ok(());
    }

    // Try fallback patterns
    if try_print_fallback(data, indent) {
        return Ok(());
    }

    if data.len() >= 4 {
        bail!(
            "unknown selector 0x{}, not recognized",
            hex::encode(&data[..4])
        );
    }
    bail!("could not decode data, not recognized")
}

pub(crate) fn decode_log(
    topics: &[B256],
    data: &[u8],
) -> Option<(String, Vec<(String, DynSolValue)>)> {
    let topic0 = topics.first()?;
    let event = EVENT_DB.as_ref().ok()?.get(topic0)?;

    let non_indexed_params: Vec<_> = event.inputs.iter().filter(|p| !p.indexed).collect();

    // Decode non-indexed from data
    let data_types: Vec<DynSolType> = non_indexed_params
        .iter()
        .filter_map(|p| p.resolve().ok())
        .collect();

    let data_tuple = DynSolType::Tuple(data_types);
    let DynSolValue::Tuple(data_values) = data_tuple.abi_decode_params(data).ok()? else {
        return None;
    };

    // Build result combining indexed (from topics) and non-indexed (from data)
    let mut result = Vec::new();
    let mut topic_idx = 1; // skip topic0
    let mut data_idx = 0;

    for param in &event.inputs {
        let name = if param.name.is_empty() {
            format!("arg{}", result.len())
        } else {
            param.name.clone()
        };

        if param.indexed {
            if topic_idx < topics.len() {
                let topic = topics[topic_idx];
                topic_idx += 1;
                // Try to decode the topic based on type
                let resolved = param.resolve().ok();
                let value = match resolved.as_ref() {
                    Some(DynSolType::Address) => {
                        DynSolValue::Address(alloy::primitives::Address::from_word(topic))
                    }
                    Some(DynSolType::Bool) => DynSolValue::Bool(topic[31] != 0),
                    Some(DynSolType::Uint(bits)) => {
                        DynSolValue::Uint(alloy::primitives::Uint::from_be_bytes(topic.0), *bits)
                    }
                    Some(DynSolType::Int(bits)) => {
                        DynSolValue::Int(alloy::primitives::Signed::from_be_bytes(topic.0), *bits)
                    }
                    _ => {
                        // For dynamic types (string, bytes, arrays), topic is keccak hash
                        DynSolValue::FixedBytes(topic.0.into(), 32)
                    }
                };
                result.push((name, value));
            }
        } else if data_idx < data_values.len() {
            result.push((name, data_values[data_idx].clone()));
            data_idx += 1;
        }
    }

    Some((event.signature(), result))
}

pub(crate) fn format_value_pub(v: &DynSolValue) -> String {
    format_value(v)
}

fn parse_hex(s: &str) -> Result<Vec<u8>> {
    let clean: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    let clean = clean.strip_prefix("0x").unwrap_or(&clean);
    hex::decode(clean).map_err(|e| eyre::eyre!("invalid hex: {e}"))
}

fn format_value(v: &DynSolValue) -> String {
    match v {
        DynSolValue::Bool(b) => b.to_string(),
        DynSolValue::Int(n, _) => n.to_string(),
        DynSolValue::Uint(n, _) => n.to_string(),
        DynSolValue::FixedBytes(word, size) => format!("0x{}", hex::encode(&word[..*size]))
            .dimmed()
            .to_string(),
        DynSolValue::Address(a) => format!("{a}").cyan().to_string(),
        DynSolValue::Bytes(b) => {
            let h = hex::encode(b);
            let s = if h.len() > 128 {
                format!("0x{}… ({} bytes)", &h[..64], b.len())
            } else {
                format!("0x{h}")
            };
            s.dimmed().to_string()
        }
        DynSolValue::String(s) => format!("\"{s}\""),
        DynSolValue::Array(arr) | DynSolValue::FixedArray(arr) => {
            format!(
                "[{}]",
                arr.iter().map(format_value).collect::<Vec<_>>().join(", ")
            )
        }
        DynSolValue::Tuple(items) => {
            format!(
                "({})",
                items
                    .iter()
                    .map(format_value)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
        _ => format!("{v:?}"),
    }
}

fn value_as_u64(v: &DynSolValue) -> Option<u64> {
    if let DynSolValue::Uint(n, _) = v {
        let limbs = n.as_limbs();
        if limbs[1] == 0 && limbs[2] == 0 && limbs[3] == 0 {
            return Some(limbs[0]);
        }
    }
    None
}

fn enum_label(field: &str, value: &DynSolValue) -> String {
    let Some(v) = value_as_u64(value) else {
        return String::new();
    };
    match field {
        "messageType" => its_message_type_name(v).map(|s| format!(" ({})", s.bold())),
        "tokenManagerType" => token_manager_type_name(v).map(|s| format!(" ({})", s.bold())),
        _ => None,
    }
    .unwrap_or_default()
}

fn print_decoded(
    inputs: &[alloy::json_abi::Param],
    values: &[DynSolValue],
    indent: &str,
    depth: usize,
) {
    for (i, (param, value)) in inputs.iter().zip(values.iter()).enumerate() {
        let name = if param.name.is_empty() {
            format!("arg{i}")
        } else {
            param.name.clone()
        };
        println!(
            "{indent}{name} {}: {}{}",
            format!("({})", param.ty).dimmed(),
            format_value(value),
            enum_label(&name, value)
        );

        if depth < MAX_DEPTH {
            match (&*param.ty, value) {
                ("bytes", DynSolValue::Bytes(b)) if b.len() >= 4 => {
                    try_print_nested(b, &format!("{indent}  "), depth + 1);
                }
                ("bytes[]", DynSolValue::Array(arr)) => {
                    let nested = format!("{indent}  ");
                    for (j, item) in arr.iter().enumerate() {
                        if let DynSolValue::Bytes(b) = item
                            && b.len() >= 4
                        {
                            println!("\n{indent}[{j}]: 0x{} ", hex::encode(&b[..4]));
                            try_print_nested(b, &format!("{nested}  "), depth + 1);
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

fn try_print_nested(data: &[u8], indent: &str, depth: usize) -> bool {
    if depth > MAX_DEPTH || data.is_empty() {
        return false;
    }

    // Try as function call
    if data.len() >= 4
        && let Ok(sel) = <[u8; 4]>::try_from(&data[..4])
        && let Some(func) = FUNC_DB.as_ref().ok().and_then(|db| db.get(&sel))
        && let Ok(values) = func.abi_decode_input(&data[4..])
    {
        println!("{indent}{}", func.signature().bold());
        print_decoded(&func.inputs, &values, &format!("{indent}  "), depth);
        return true;
    }

    // Try ITS hub frame (before governance: a hub frame's bytes offset also lands at 0xa0)
    if try_print_its_hub(data, indent, depth) {
        return true;
    }

    // Try governance proposal payload (before plain ITS: command 0 collides with ITS msgType 0)
    if try_print_governance(data, indent, depth) {
        return true;
    }

    // Try ITS payload
    if try_print_its(data, indent, depth) {
        return true;
    }

    // Try as printable UTF-8
    if let Ok(s) = std::str::from_utf8(data)
        && !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_graphic() || c.is_ascii_whitespace())
    {
        println!("{indent}→ \"{s}\"");
        return true;
    }

    false
}

/// A decoded ITS message: the schema definition for its message type plus the decoded values.
struct ItsMessage {
    name: &'static str,
    types: Vec<DynSolType>,
    labels: Vec<&'static str>,
    values: Vec<DynSolValue>,
}

fn its_message_type(data: &[u8]) -> Option<u64> {
    // Message type is first uint256 — upper 24 bytes must be zero
    if data.len() < 32 || data[..24].iter().any(|&b| b != 0) {
        return None;
    }
    let bytes = <[u8; 8]>::try_from(&data[24..32]).ok()?;
    Some(u64::from_be_bytes(bytes))
}

fn parse_its(data: &[u8]) -> Option<ItsMessage> {
    let msg_type = its_message_type(data)?;
    let (name, types, labels) = its_message_def(msg_type)?;

    let tuple_type = DynSolType::Tuple(types.clone());
    let DynSolValue::Tuple(values) = tuple_type.abi_decode_params(data).ok()? else {
        return None;
    };

    Some(ItsMessage {
        name,
        types,
        labels,
        values,
    })
}

/// Hub-routing frames only (SEND_TO_HUB=3, RECEIVE_FROM_HUB=4) — must run before the
/// governance schema, which they'd otherwise satisfy with garbage values.
fn try_print_its_hub(data: &[u8], indent: &str, depth: usize) -> bool {
    matches!(its_message_type(data), Some(3 | 4)) && try_print_its(data, indent, depth)
}

fn try_print_its(data: &[u8], indent: &str, depth: usize) -> bool {
    if depth > MAX_DEPTH {
        return false;
    }
    let Some(ItsMessage {
        name,
        types,
        labels,
        values,
    }) = parse_its(data)
    else {
        return false;
    };

    println!("{indent}{}", format!("[ITS {name}]").bold());
    for (value, (label, ty)) in values.iter().zip(labels.iter().zip(types.iter())) {
        println!(
            "{indent}  {label} {}: {}{}",
            format!("({ty})").dimmed(),
            format_value(value),
            enum_label(label, value)
        );

        if depth < MAX_DEPTH
            && let DynSolValue::Bytes(b) = value
            && b.len() >= 4
        {
            try_print_nested(b, &format!("{indent}    "), depth + 1);
        }
    }
    true
}

fn try_print_fallback(data: &[u8], indent: &str) -> bool {
    let patterns: &[&[DynSolType]] = &[
        &[DynSolType::String, DynSolType::Bytes, DynSolType::Bytes],
        &[DynSolType::String, DynSolType::String, DynSolType::Bytes],
        &[DynSolType::String],
        &[DynSolType::Bytes],
        &[
            DynSolType::Address,
            DynSolType::Uint(256),
            DynSolType::Bytes,
        ],
        &[DynSolType::FixedBytes(32), DynSolType::Bytes],
    ];

    for pattern in patterns {
        let tuple = DynSolType::Tuple(pattern.to_vec());
        if let Ok(DynSolValue::Tuple(values)) = tuple.abi_decode_params(data) {
            let type_str = pattern
                .iter()
                .map(|t| t.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            println!("{indent}{}", format!("Decoded as ({type_str}):").bold());
            for (i, (value, ty)) in values.iter().zip(pattern.iter()).enumerate() {
                println!(
                    "{indent}  arg{i} {}: {}",
                    format!("({ty})").dimmed(),
                    format_value(value)
                );
                if let (DynSolType::Bytes, DynSolValue::Bytes(b)) = (ty, value)
                    && b.len() >= 4
                {
                    try_print_nested(b, &format!("{indent}    "), 1);
                }
            }
            return true;
        }
    }
    false
}

fn its_message_def(msg_type: u64) -> Option<(&'static str, Vec<DynSolType>, Vec<&'static str>)> {
    Some(match msg_type {
        0 => (
            "INTERCHAIN_TRANSFER",
            vec![
                DynSolType::Uint(256),
                DynSolType::FixedBytes(32),
                DynSolType::Bytes,
                DynSolType::Bytes,
                DynSolType::Uint(256),
                DynSolType::Bytes,
            ],
            vec![
                "messageType",
                "tokenId",
                "sourceAddress",
                "destinationAddress",
                "amount",
                "data",
            ],
        ),
        1 => (
            "DEPLOY_INTERCHAIN_TOKEN",
            vec![
                DynSolType::Uint(256),
                DynSolType::FixedBytes(32),
                DynSolType::String,
                DynSolType::String,
                DynSolType::Uint(8),
                DynSolType::Bytes,
            ],
            vec![
                "messageType",
                "tokenId",
                "name",
                "symbol",
                "decimals",
                "minter",
            ],
        ),
        3 => (
            "SEND_TO_HUB",
            vec![DynSolType::Uint(256), DynSolType::String, DynSolType::Bytes],
            vec!["messageType", "destinationChain", "payload"],
        ),
        4 => (
            "RECEIVE_FROM_HUB",
            vec![DynSolType::Uint(256), DynSolType::String, DynSolType::Bytes],
            vec!["messageType", "sourceChain", "payload"],
        ),
        5 => (
            "LINK_TOKEN",
            vec![
                DynSolType::Uint(256),
                DynSolType::FixedBytes(32),
                DynSolType::Uint(8),
                DynSolType::Bytes,
                DynSolType::Bytes,
                DynSolType::Bytes,
            ],
            vec![
                "messageType",
                "tokenId",
                "tokenManagerType",
                "sourceTokenAddress",
                "destinationTokenAddress",
                "linkParams",
            ],
        ),
        6 => (
            "REGISTER_TOKEN_METADATA",
            vec![
                DynSolType::Uint(256),
                DynSolType::Bytes,
                DynSolType::Uint(8),
            ],
            vec!["messageType", "tokenAddress", "decimals"],
        ),
        _ => return None,
    })
}

fn its_message_type_name(v: u64) -> Option<&'static str> {
    match v {
        0 => Some("INTERCHAIN_TRANSFER"),
        1 => Some("DEPLOY_INTERCHAIN_TOKEN"),
        3 => Some("SEND_TO_HUB"),
        4 => Some("RECEIVE_FROM_HUB"),
        5 => Some("LINK_TOKEN"),
        6 => Some("REGISTER_TOKEN_METADATA"),
        _ => None,
    }
}

fn token_manager_type_name(v: u64) -> Option<&'static str> {
    match v {
        0 => Some("NATIVE_INTERCHAIN_TOKEN"),
        1 => Some("MINT_BURN_FROM"),
        2 => Some("LOCK_UNLOCK"),
        3 => Some("LOCK_UNLOCK_FEE"),
        4 => Some("MINT_BURN"),
        5 => Some("GATEWAY"),
        _ => None,
    }
}

fn format_eta(eta: U256) -> String {
    // eta is a unix timestamp (seconds). Keep the raw value for debugging and append a human-readable
    // time in the host's local timezone (Europe/Athens-equivalent on shallot).
    let dt = u64::try_from(eta)
        .ok()
        .and_then(|secs| i64::try_from(secs).ok())
        .and_then(|secs| chrono::DateTime::from_timestamp(secs, 0));
    match dt {
        Some(dt) => format!(
            "{eta} ({})",
            dt.with_timezone(&chrono::Local).format("%H:%M %d %B %y")
        ),
        None => eta.to_string(),
    }
}

fn governance_command_name(command: u64) -> Option<&'static str> {
    // InterchainGovernance defines 0/1; AxelarServiceGovernance adds 2/3.
    match command {
        0 => Some("ScheduleTimeLockProposal"),
        1 => Some("CancelTimeLockProposal"),
        2 => Some("ApproveOperatorProposal"),
        3 => Some("CancelOperatorApproval"),
        _ => None,
    }
}

/// A decoded Interchain/Service-Governance GMP proposal payload, i.e.
/// `abi.encode(uint256 command, address target, bytes callData, uint256 nativeValue, uint256 eta)`
/// — what the gov module sends (via `AxelarnetGateway.call_contract`) to an edge chain's
/// `InterchainGovernance` / `AxelarServiceGovernance`.
struct GovernanceProposal {
    command: u64,
    command_name: &'static str,
    target: Address,
    call_data: Vec<u8>,
    native_value: U256,
    eta: U256,
}

/// Unix seconds for 3000-01-01 — any eta beyond this is another schema's word misread as a
/// timestamp, not a real timelock.
const MAX_SANE_ETA_SECS: u64 = 32_503_680_000;

/// A genuine proposal target is a deployed contract address; a tiny value or a word-aligned
/// value inside the calldata is almost certainly a dynamic-field offset from a different schema.
fn target_looks_like_abi_offset(target: Address, data_len: usize) -> bool {
    let word = U256::from_be_slice(target.as_slice());
    word < U256::from(0x10000_u64)
        || (word % U256::from(32) == U256::ZERO && word < U256::from(data_len))
}

fn parse_governance(data: &[u8]) -> Option<GovernanceProposal> {
    // Static head is 5 words (command, target, callData offset, nativeValue, eta); the bytes
    // field needs at least its length word, so 6 words minimum.
    if data.len() < 6 * 32 {
        return None;
    }
    // command: a small uint (0..=3) — the upper 31 bytes of the word must be zero.
    if data[..31].iter().any(|&b| b != 0) {
        return None;
    }
    let command = u64::from(data[31]);
    let command_name = governance_command_name(command)?;
    // target: an address — the upper 12 bytes of the word must be zero.
    if data[32..44].iter().any(|&b| b != 0) {
        return None;
    }
    // callData offset must be the canonical 0xa0 (160) for this static 5-field layout. This is the
    // key discriminator from an ITS message (whose first dynamic field sits at 0xc0), since a
    // ScheduleTimeLock command (0) otherwise collides with the ITS INTERCHAIN_TRANSFER type (0).
    if data[64..95].iter().any(|&b| b != 0) || data[95] != 0xa0 {
        return None;
    }

    let schema = DynSolType::Tuple(vec![
        DynSolType::Uint(256),
        DynSolType::Address,
        DynSolType::Bytes,
        DynSolType::Uint(256),
        DynSolType::Uint(256),
    ]);
    let DynSolValue::Tuple(values) = schema.abi_decode_params(data).ok()? else {
        return None;
    };

    let target = match &values[1] {
        DynSolValue::Address(a) => *a,
        _ => return None,
    };
    if target_looks_like_abi_offset(target, data.len()) {
        return None;
    }
    let call_data = match &values[2] {
        DynSolValue::Bytes(b) => b.clone(),
        _ => return None,
    };
    let native_value = match &values[3] {
        DynSolValue::Uint(n, _) => *n,
        _ => return None,
    };
    let eta = match &values[4] {
        DynSolValue::Uint(n, _) => *n,
        _ => return None,
    };
    if eta > U256::from(MAX_SANE_ETA_SECS) {
        return None;
    }

    Some(GovernanceProposal {
        command,
        command_name,
        target,
        call_data,
        native_value,
        eta,
    })
}

fn try_print_governance(data: &[u8], indent: &str, depth: usize) -> bool {
    if depth > MAX_DEPTH {
        return false;
    }
    let Some(proposal) = parse_governance(data) else {
        return false;
    };

    println!(
        "{indent}{}",
        format!("[Governance {}]", proposal.command_name).bold()
    );
    println!(
        "{indent}  command {}: {} ({})",
        "(uint256)".dimmed(),
        proposal.command,
        proposal.command_name.bold()
    );
    println!(
        "{indent}  target {}: {}",
        "(address)".dimmed(),
        proposal.target.cyan()
    );
    println!(
        "{indent}  callData {}: {}",
        "(bytes)".dimmed(),
        format!("0x{}", hex::encode(&proposal.call_data)).dimmed()
    );
    if depth < MAX_DEPTH && proposal.call_data.len() >= 4 {
        try_print_nested(&proposal.call_data, &format!("{indent}    "), depth + 1);
    }
    println!(
        "{indent}  nativeValue {}: {}",
        "(uint256)".dimmed(),
        proposal.native_value
    );
    println!(
        "{indent}  eta {}: {}",
        "(uint256)".dimmed(),
        format_eta(proposal.eta)
    );
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real testnet governance proposals (ScheduleTimeLockProposal → gateway.setPauseStatus(false)),
    // from the AxelarnetGateway.call_contract payloads of gov proposals 590 and 588. They differ only in eta.
    const SCHEDULE_590: &str = "0x0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000e432150cce91c13a887f7d836923d5597add8e3100000000000000000000000000000000000000000000000000000000000000a00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000006a1f13090000000000000000000000000000000000000000000000000000000000000024c38bb537000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000";
    const SCHEDULE_588: &str = "0x0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000e432150cce91c13a887f7d836923d5597add8e3100000000000000000000000000000000000000000000000000000000000000a00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000006a1ef11c0000000000000000000000000000000000000000000000000000000000000024c38bb537000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000";

    #[test]
    fn governance_parses_real_schedule_timelock_payloads() {
        for payload in [SCHEDULE_590, SCHEDULE_588] {
            let data = parse_hex(payload).unwrap();
            let p = parse_governance(&data).expect("should parse as a governance proposal");
            assert_eq!(p.command, 0);
            assert_eq!(p.command_name, "ScheduleTimeLockProposal");
            assert_eq!(
                p.target,
                "0xe432150cce91c13a887f7D836923d5597adD8E31"
                    .parse::<Address>()
                    .unwrap()
            );
            assert_eq!(p.native_value, U256::ZERO);
            // inner call = setPauseStatus(false): selector 0xc38bb537 + 32-byte bool arg
            assert_eq!(p.call_data.len(), 36);
            assert_eq!(&p.call_data[..4], &[0xc3, 0x8b, 0xb5, 0x37]);
        }
    }

    #[test]
    fn governance_eta_distinguishes_proposals() {
        let a = parse_governance(&parse_hex(SCHEDULE_590).unwrap()).unwrap();
        let b = parse_governance(&parse_hex(SCHEDULE_588).unwrap()).unwrap();
        assert_eq!(a.eta, U256::from(0x6a1f_1309_u64));
        assert_eq!(b.eta, U256::from(0x6a1e_f11c_u64));
        assert_ne!(a.eta, b.eta);
    }

    #[test]
    fn format_eta_keeps_unix_and_appends_local_time() {
        // 0x6a1f1309 = 1780421385 (2026-06-02 17:29:45 UTC)
        let s = format_eta(U256::from(0x6a1f_1309_u64));
        assert!(s.starts_with("1780421385 ("), "got {s}");
        assert!(s.ends_with(')'), "got {s}");
        // the date part is timezone-stable for this timestamp on any realistic host tz
        assert!(s.contains("June 26"), "got {s}");
    }

    #[test]
    fn governance_maps_all_command_variants() {
        let base = parse_hex(SCHEDULE_590).unwrap();
        for (command, name) in [
            (1u8, "CancelTimeLockProposal"),
            (2, "ApproveOperatorProposal"),
            (3, "CancelOperatorApproval"),
        ] {
            let mut data = base.clone();
            data[31] = command; // flip just the command word
            let p = parse_governance(&data).expect("variant should parse");
            assert_eq!(p.command, u64::from(command));
            assert_eq!(p.command_name, name);
        }
    }

    #[test]
    fn governance_rejects_unknown_command() {
        let mut data = parse_hex(SCHEDULE_590).unwrap();
        data[31] = 4; // out of the 0..=3 range
        assert!(parse_governance(&data).is_none());
    }

    #[test]
    fn governance_rejects_non_canonical_offset() {
        // 0xc0 offset (an ITS-shaped layout) must NOT be read as a governance proposal
        let mut data = parse_hex(SCHEDULE_590).unwrap();
        data[95] = 0xc0;
        assert!(parse_governance(&data).is_none());
    }

    #[test]
    fn governance_rejects_dirty_address_word() {
        let mut data = parse_hex(SCHEDULE_590).unwrap();
        data[32] = 0xff; // address word's upper bytes must be zero
        assert!(parse_governance(&data).is_none());
    }

    // Real ITS hub SEND_TO_HUB payload (Stellar → Ethereum SolvBTC transfer). Its head words
    // (3, 0x60, 0xa0, …) also satisfy the governance schema's shape checks, so it used to
    // misdecode as [Governance CancelOperatorApproval] with garbage values.
    const SEND_TO_HUB_STELLAR_ETH: &str = "0x0000000000000000000000000000000000000000000000000000000000000003000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000a00000000000000000000000000000000000000000000000000000000000000008457468657265756d00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001a00000000000000000000000000000000000000000000000000000000000000000edb4e6c60750463f5e3970bd7bdbf4af48ffdcb8ac4e40b680e491ca338dd1ba00000000000000000000000000000000000000000000000000000000000000c0000000000000000000000000000000000000000000000000000000000000012000000000000000000000000000000000000000000000000000000000000027100000000000000000000000000000000000000000000000000000000000000160000000000000000000000000000000000000000000000000000000000000003847413653594f424134334c5441554f424e4c32344f365a355746355a484f4746514d4933504e57324d4c56493535424f49334c4544344e5000000000000000000000000000000000000000000000000000000000000000000000000000000014c902c442dc4d8c39c33c7cdd648ef44c986c395700000000000000000000000000000000000000000000000000000000000000000000000000000000000000033689de0000000000000000000000000000000000000000000000000000000000";

    #[test]
    fn send_to_hub_is_not_claimed_by_governance() {
        let data = parse_hex(SEND_TO_HUB_STELLAR_ETH).unwrap();
        assert!(parse_governance(&data).is_none());
    }

    #[test]
    fn send_to_hub_decodes_as_its_hub_frame_with_inner_transfer() {
        let data = parse_hex(SEND_TO_HUB_STELLAR_ETH).unwrap();
        let outer = parse_its(&data).expect("should parse as ITS");
        assert_eq!(outer.name, "SEND_TO_HUB");
        assert_eq!(outer.values[1], DynSolValue::String("Ethereum".to_string()));

        let DynSolValue::Bytes(inner) = &outer.values[2] else {
            panic!("payload field should be bytes");
        };
        let transfer = parse_its(inner).expect("inner payload should parse as ITS");
        assert_eq!(transfer.name, "INTERCHAIN_TRANSFER");
        assert_eq!(
            transfer.values[3],
            DynSolValue::Bytes(hex::decode("c902c442dc4d8c39c33c7cdd648ef44c986c3957").unwrap())
        );
        assert_eq!(
            transfer.values[4],
            DynSolValue::Uint(U256::from(10000), 256)
        );
        assert_eq!(
            transfer.values[5],
            DynSolValue::Bytes(hex::decode("3689de").unwrap())
        );
    }

    #[test]
    fn decode_send_to_hub_end_to_end() {
        run(SEND_TO_HUB_STELLAR_ETH).unwrap();
    }

    #[test]
    fn governance_rejects_offset_like_target() {
        let mut data = parse_hex(SCHEDULE_590).unwrap();
        // overwrite the target word with 0x60 — a string offset, not an address
        data[32..64].fill(0);
        data[63] = 0x60;
        assert!(parse_governance(&data).is_none());
    }

    #[test]
    fn governance_rejects_implausible_eta() {
        let mut data = parse_hex(SCHEDULE_590).unwrap();
        // overwrite the eta word with ASCII garbage far beyond year 3000
        data[128..160].copy_from_slice(b"Ethereum\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0");
        assert!(parse_governance(&data).is_none());
    }

    #[test]
    fn decode_governance_schedule_timelock_end_to_end() {
        // full path, including recursion into the inner setPauseStatus(bool) call
        run(SCHEDULE_590).unwrap();
    }

    #[test]
    fn decode_register_custom_token() {
        run(
            "0xd8c032689e52713efc11c03e5a032d47a49317e5322bd17fe623afee2cbf25603e3fb340000000000000000000000000fc450df8c19670b6a7f18092fc4aed43e9b8bf5600000000000000000000000000000000000000000000000000000000000000010000000000000000000000005ae7ec463b0b97635fc0e57a0129a386a34cccb5",
        ).unwrap();
    }

    #[test]
    fn decode_link_token() {
        run(
            "0x0f4433d39e52713efc11c03e5a032d47a49317e5322bd17fe623afee2cbf25603e3fb34000000000000000000000000000000000000000000000000000000000000000c0000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000140000000000000000000000000000000000000000000000000002386f26fc100000000000000000000000000000000000000000000000000000000000000000010657468657265756d2d7365706f6c6961000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000014450facbddc1a261bd9e29ccb476ff370a3f448fc00000000000000000000000000000000000000000000000000000000000000000000000000000000000000145ae7ec463b0b97635fc0e57a0129a386a34cccb5000000000000000000000000",
        ).unwrap();
    }

    #[test]
    fn decode_execute_with_payload() {
        run(
            "0x49160658f739a5a827c0d9a97fa5b40c56c638785355777fbb135b8ffb54457b10094b65000000000000000000000000000000000000000000000000000000000000008000000000000000000000000000000000000000000000000000000000000000c000000000000000000000000000000000000000000000000000000000000001200000000000000000000000000000000000000000000000000000000000000009736f6c616e612d31380000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000002c4356384c626b36595457376a7453686838634c7939346b6d574665357545686847555a6e335a78686255654c000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000800000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000000000000000003968656c6c6f2066726f6d20617865206c6f6164207465737420393465336130343363363833313836323130663132383631633563376664646600000000000000",
        ).unwrap();
    }

    #[test]
    fn decode_deploy_interchain_token() {
        run(
            "0x3e12f8c533b7f5b5f2a8c055cd44c8671d2ba3c10f1e00506232d8f4b1095ef9988257fc00000000000000000000000000000000000000000000000000000000000000c00000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000001200000000000000000000000000000000000000000000003635c9adc5dea0000000000000000000000000000081e63ea8f64fedb9858eb6e2176b431fbd10d1ec00000000000000000000000000000000000000000000000000000000000000074d79546f6b656e0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000034d544b0000000000000000000000000000000000000000000000000000000000",
        ).unwrap();
    }

    #[test]
    fn decode_handles_whitespace_in_hex() {
        // Same as deploy_interchain_token but with spaces
        run(
            "0x3e12f8c5 33b7f5b5f2a8c055cd44c8671d2ba3c10f1e00506232d8f4b1095ef9988257fc 00000000000000000000000000000000000000000000000000000000000000c0 0000000000000000000000000000000000000000000000000000000000000100 0000000000000000000000000000000000000000000000000000000000000012 00000000000000000000000000000000000000000000003635c9adc5dea00000 00000000000000000000000081e63ea8f64fedb9858eb6e2176b431fbd10d1ec 0000000000000000000000000000000000000000000000000000000000000007 4d79546f6b656e00000000000000000000000000000000000000000000000000 0000000000000000000000000000000000000000000000000000000000000003 4d544b0000000000000000000000000000000000000000000000000000000000",
        ).unwrap();
    }

    #[test]
    fn decode_set_trusted_address() {
        run(
            "0x9f409d77000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000000800000000000000000000000000000000000000000000000000000000000000011736f6c616e612d73746167656e65742d33000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000368756200000000000000000000000000000000000000000000000000000000",
        ).unwrap();
    }

    #[test]
    fn decode_multicall_with_nested_calls() {
        run(
            "0xac9650d8000000000000000000000000000000000000000000000000000000000000002000000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000000000000000040000000000000000000000000000000000000000000000000000000000000014000000000000000000000000000000000000000000000000000000000000000c49f409d77000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000000800000000000000000000000000000000000000000000000000000000000000011736f6c616e612d73746167656e65742d33000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000368756200000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000c49f409d7700000000000000000000000000000000000000000000000000000000000000400000000000000000000000000000000000000000000000000000000000000080000000000000000000000000000000000000000000000000000000000000000368756200000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000003687562000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
        ).unwrap();
    }

    #[test]
    fn decode_unknown_selector_errors() {
        let result = run("0xdeadbeef00000000000000000000000000000000000000000000000000000000");
        assert!(result.is_err());
    }
}
