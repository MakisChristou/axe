//! Borsh + payload decoding primitives. Functions here turn raw bytes into
//! typed values or human-readable strings, but never print directly.

use alloy::dyn_abi::DynSolType;
use eyre::{Result, bail};
use owo_colors::OwoColorize;
use solana_axelar_its::encoding::{HubMessage, Message};

use super::format::format_address_bytes;

/// Decode borsh-encoded bytes (4-byte little-endian length + raw bytes)
pub(super) fn decode_borsh_bytes(data: &[u8]) -> Result<(Vec<u8>, &[u8])> {
    if data.len() < 4 {
        bail!("not enough data for bytes length");
    }
    let len = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
    if data.len() < 4 + len {
        bail!("not enough data for bytes content");
    }
    Ok((data[4..4 + len].to_vec(), &data[4 + len..]))
}

/// Decode a borsh-encoded string (4-byte little-endian length + UTF-8 bytes)
pub(super) fn decode_borsh_string(data: &[u8]) -> Result<(String, &[u8])> {
    if data.len() < 4 {
        bail!("not enough data for string length");
    }
    let len = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
    if data.len() < 4 + len {
        bail!("not enough data for string content");
    }
    let s = String::from_utf8_lossy(&data[4..4 + len]).to_string();
    Ok((s, &data[4 + len..]))
}

/// Try to decode the payload and return a human-readable summary.
/// Returns (summary_line, Option<decoded_content>)
pub(super) fn decode_payload(payload: &[u8]) -> (String, Option<String>) {
    if payload.is_empty() {
        return ("empty".to_string(), None);
    }

    // Check if it's an Axelar encoded payload (first byte = encoding scheme)
    if matches!(payload[0], 0x00 | 0x01)
        && let Ok(decoded) = solana_axelar_gateway::payload::AxelarMessagePayload::decode(payload)
    {
        let inner = decoded.payload_without_accounts();

        // Try to decode the inner data
        let content = try_decode_payload_content(inner);
        let size = format!("{} bytes", payload.len());
        return (size, content);
    }

    // Try decoding directly (e.g. ITS HubMessage is Borsh without AxelarMessagePayload wrapper)
    if let Some(content) = try_decode_payload_content(payload) {
        return (format!("{} bytes", payload.len()), Some(content));
    }

    // Raw payload — try to interpret as UTF-8
    if let Ok(s) = std::str::from_utf8(payload)
        && s.is_ascii()
        && !s.is_empty()
    {
        return (format!("{} bytes", payload.len()), Some(format!("\"{s}\"")));
    }

    (format!("{} bytes", payload.len()), None)
}

/// Try to decode inner payload data as a known type.
fn try_decode_payload_content(data: &[u8]) -> Option<String> {
    if data.is_empty() {
        return None;
    }

    // Try as ITS HubMessage (Borsh-encoded)
    if let Some(s) = try_decode_its_hub_message(data) {
        return Some(s);
    }

    // Try as ABI-encoded (string) — common for GMP memo payloads
    if let Ok(val) = DynSolType::String.abi_decode(data)
        && let Some(s) = val.as_str()
        && !s.is_empty()
    {
        return Some(format!("\"{s}\""));
    }

    // Try as ABI-encoded tuple(string) — SenderReceiver._execute format
    if let Ok(val) = DynSolType::Tuple(vec![DynSolType::String]).abi_decode(data)
        && let Some(vals) = val.as_tuple()
        && let Some(s) = vals.first().and_then(|v| v.as_str())
        && !s.is_empty()
    {
        return Some(format!("\"{s}\""));
    }

    // Try as raw UTF-8
    if let Ok(s) = std::str::from_utf8(data)
        && s.is_ascii()
        && !s.is_empty()
    {
        return Some(format!("\"{s}\""));
    }

    None
}

fn try_decode_its_hub_message(data: &[u8]) -> Option<String> {
    let hub_msg: HubMessage = borsh::from_slice(data).ok()?;
    match hub_msg {
        HubMessage::SendToHub {
            destination_chain,
            message,
        } => {
            let mut lines = vec![format!("ITS SendToHub → \"{destination_chain}\"")];
            format_its_message(&message, &mut lines);
            Some(lines.join("\n"))
        }
        HubMessage::ReceiveFromHub {
            source_chain,
            message,
        } => {
            let mut lines = vec![format!("ITS ReceiveFromHub ← \"{source_chain}\"")];
            format_its_message(&message, &mut lines);
            Some(lines.join("\n"))
        }
        HubMessage::RegisterTokenMetadata(_) => Some("ITS RegisterTokenMetadata".to_string()),
    }
}

fn format_its_message(message: &solana_axelar_its::encoding::Message, lines: &mut Vec<String>) {
    let p = "      ┃ ";
    match message {
        Message::InterchainTransfer(t) => {
            lines.push(format!("      ┏━ {}", "InterchainTransfer".bold()));
            lines.push(format!(
                "{p}{} {}",
                "token_id:".dimmed(),
                hex::encode(t.token_id)
            ));
            lines.push(format!(
                "{p}{} {}",
                "source:".dimmed(),
                format_address_bytes(&t.source_address)
            ));
            lines.push(format!(
                "{p}{} {}",
                "destination:".dimmed(),
                format_address_bytes(&t.destination_address)
            ));
            lines.push(format!("{p}{} {}", "amount:".dimmed(), t.amount));
            lines.push("      ┗━".to_string());
        }
        Message::DeployInterchainToken(t) => {
            lines.push(format!("      ┏━ {}", "DeployInterchainToken".bold()));
            lines.push(format!(
                "{p}{} {}",
                "token_id:".dimmed(),
                hex::encode(t.token_id)
            ));
            lines.push(format!("{p}{} \"{}\"", "name:".dimmed(), t.name));
            lines.push(format!("{p}{} \"{}\"", "symbol:".dimmed(), t.symbol));
            lines.push(format!("{p}{} {}", "decimals:".dimmed(), t.decimals));
            lines.push("      ┗━".to_string());
        }
        Message::LinkToken(t) => {
            lines.push(format!("      ┏━ {}", "LinkToken".bold()));
            lines.push(format!(
                "{p}{} {}",
                "token_id:".dimmed(),
                hex::encode(t.token_id)
            ));
            lines.push(format!(
                "{p}{} {}",
                "token_manager_type:".dimmed(),
                t.token_manager_type
            ));
            lines.push(format!(
                "{p}{} {}",
                "source_token:".dimmed(),
                format_address_bytes(&t.source_token_address)
            ));
            lines.push(format!(
                "{p}{} {}",
                "destination_token:".dimmed(),
                format_address_bytes(&t.destination_token_address)
            ));
            lines.push("      ┗━".to_string());
        }
    }
}
