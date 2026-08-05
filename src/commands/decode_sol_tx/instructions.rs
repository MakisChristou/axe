//! Instruction-level printers and the JSON sister of the human-readable
//! decoder. Top-level + inner CPI instruction display lives here, plus the
//! per-opcode arg printers and a separate `decode_instruction_args_json`
//! used by `decode_sol_activity` for machine-readable output.

use owo_colors::OwoColorize;
use serde_json::json;
use solana_sdk::pubkey::Pubkey;
use solana_transaction_status::UiInstruction;
use std::collections::HashMap;

use super::events::decode_anchor_event;
use super::format::{format_account, format_address_bytes, println_kv};
use super::parsing::{decode_borsh_bytes, decode_borsh_string, decode_payload};
use super::registry::{EVENT_IX_TAG_LE, account_labels, event_name, instruction_name};

pub(super) fn print_top_level_instructions(
    instructions: &[solana_transaction_status::UiCompiledInstruction],
    all_keys: &[Pubkey],
    known: &HashMap<Pubkey, &'static str>,
) {
    println!("\n{}", "━━ Instructions ━━".bold());
    for (i, ix) in instructions.iter().enumerate() {
        let program_idx = ix.program_id_index as usize;
        let program_id = all_keys.get(program_idx);
        let program_label = program_id.and_then(|pk| known.get(pk)).copied();
        let program_addr = program_id.map_or("unknown".to_string(), |pk| pk.to_string());

        if program_label == Some("ComputeBudget") {
            continue;
        }

        let data_bytes = bs58::decode(&ix.data).into_vec().unwrap_or_default();
        let ix_name = instruction_name(&data_bytes).unwrap_or("unknown");

        if let Some(label) = program_label {
            println!(
                "\n[{}] {} {} {}",
                i.to_string().dimmed(),
                label.cyan(),
                ix_name.bold(),
                format!("({})", program_addr).dimmed(),
            );
        } else {
            println!(
                "\n[{}] {} {}",
                i.to_string().dimmed(),
                program_addr.cyan(),
                ix_name.bold()
            );
        }

        decode_instruction_args(ix_name, &data_bytes, "  │ ");

        println!("  {}", "Accounts:".dimmed());
        let labels = account_labels(ix_name);
        for (j, &acc_idx) in ix.accounts.iter().enumerate() {
            let acc = all_keys.get(acc_idx as usize);
            let label = labels.get(j).copied();
            let acc_str = acc.map_or("?".to_string(), |pk| format_account(pk, known, label));
            println!("  │ {}: {}", j.to_string().dimmed(), acc_str);
        }
    }
}

pub(super) fn print_inner_instructions(
    inner_ixs: &[solana_transaction_status::UiInnerInstructions],
    all_keys: &[Pubkey],
    known: &HashMap<Pubkey, &'static str>,
) {
    if !inner_ixs.is_empty() {
        println!("\n{}", "━━ Inner Instructions (CPI) ━━".bold());
    }
    for group in inner_ixs {
        for (i, ix) in group.instructions.iter().enumerate() {
            if let UiInstruction::Compiled(ci) = ix {
                print_inner_compiled_instruction(group.index, i, ci, all_keys, known);
            }
        }
    }
}

fn print_inner_compiled_instruction(
    group_index: u8,
    i: usize,
    ci: &solana_transaction_status::UiCompiledInstruction,
    all_keys: &[Pubkey],
    known: &HashMap<Pubkey, &'static str>,
) {
    let program_id = all_keys.get(ci.program_id_index as usize);
    let program_name = program_id
        .and_then(|pk| known.get(pk))
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            program_id.map_or("?".to_string(), |pk| {
                let s = pk.to_string();
                if s.len() > 8 {
                    format!("{}..{}", &s[..4], &s[s.len() - 4..])
                } else {
                    s
                }
            })
        });

    let data_bytes = bs58::decode(&ci.data).into_vec().unwrap_or_default();

    let is_system_program = matches!(
        program_name.as_str(),
        "SystemProgram"
            | "Token2022"
            | "TokenProgram"
            | "AssociatedToken"
            | "MetaplexMetadata"
            | "ComputeBudget"
    );
    let is_event = data_bytes.len() >= 16 && data_bytes[..8] == *EVENT_IX_TAG_LE;

    if is_system_program && !is_event {
        return;
    }

    if is_event {
        print_inner_anchor_event(group_index, i, ci, &program_name, &data_bytes);
    } else {
        print_inner_regular_instruction(
            group_index,
            i,
            ci,
            &program_name,
            &data_bytes,
            all_keys,
            known,
        );
    }
}

fn print_inner_anchor_event(
    group_index: u8,
    i: usize,
    ci: &solana_transaction_status::UiCompiledInstruction,
    program_name: &str,
    data_bytes: &[u8],
) {
    let event_disc = &data_bytes[8..16];
    let event = event_name(event_disc).unwrap_or("UnknownEvent");
    println!(
        "\n  [{}:{}] {} {} (depth {})",
        group_index,
        i,
        program_name.cyan(),
        format!("EVENT: {event}").yellow().bold(),
        ci.stack_height.unwrap_or(0),
    );

    let decoded = decode_anchor_event(event, &data_bytes[16..]);
    if let Some(decoded) = decoded {
        for line in decoded.lines() {
            println!("      {line}");
        }
    }
}

fn print_inner_regular_instruction(
    group_index: u8,
    i: usize,
    ci: &solana_transaction_status::UiCompiledInstruction,
    program_name: &str,
    data_bytes: &[u8],
    all_keys: &[Pubkey],
    known: &HashMap<Pubkey, &'static str>,
) {
    let ix_name = instruction_name(data_bytes).unwrap_or("unknown");
    println!(
        "\n  [{}:{}] {} {} (depth {})",
        group_index,
        i,
        program_name.cyan(),
        ix_name.bold(),
        ci.stack_height.unwrap_or(0),
    );

    decode_instruction_args(ix_name, data_bytes, "    │ ");

    println!("    {}", "Accounts:".dimmed());
    let inner_labels = account_labels(ix_name);
    for (j, &acc_idx) in ci.accounts.iter().enumerate() {
        let acc = all_keys.get(acc_idx as usize);
        let label = inner_labels.get(j).copied();
        let acc_str = acc.map_or("?".to_string(), |pk| format_account(pk, known, label));
        println!("    │ {}: {}", j.to_string().dimmed(), acc_str);
    }
}

/// Decode instruction arguments based on the instruction name. Each arm
/// dispatches to a per-instruction printer so the dispatcher itself stays
/// simple (one arm per opcode).
fn decode_instruction_args(ix_name: &str, data: &[u8], indent: &str) {
    if data.len() <= 8 {
        return;
    }
    let args = &data[8..]; // skip 8-byte discriminator

    match ix_name {
        "CallContract" => print_call_contract_args(args, indent),
        "PayGas" => print_pay_gas_args(args, indent),
        "InitializePayloadVerificationSession" => print_init_payload_session_args(args, indent),
        "VerifySignature" => print_verify_signature_args(args, indent),
        "SendMemo" => print_send_memo_args(args, indent),
        "InterchainTransfer" => print_interchain_transfer_args(args, indent),
        "Execute" => print_execute_args(args, indent),
        "ValidateMessage" | "ApproveMessage" => {
            print_validate_or_approve_message_args(args, indent)
        }
        "ExecuteDeployInterchainToken" => print_execute_deploy_interchain_token_args(args, indent),
        "ExecuteInterchainTransfer" => print_execute_interchain_transfer_args(args, indent),
        _ => print_unknown_args(args, indent),
    }
}

fn print_call_contract_args(args: &[u8], indent: &str) {
    let Ok((dest_chain, rest)) = decode_borsh_string(args) else {
        return;
    };
    println_kv(indent, "destination_chain", format!("\"{dest_chain}\""));
    let Ok((dest_addr, rest)) = decode_borsh_string(rest) else {
        return;
    };
    println_kv(indent, "destination_address", format!("\"{dest_addr}\""));
    if let Ok((payload_str, _)) = decode_borsh_bytes(rest) {
        let (size, content) = decode_payload(&payload_str);
        match content {
            Some(decoded) => println_kv(indent, "payload", format!("{size} → {decoded}")),
            None => println_kv(indent, "payload", size),
        }
    }
}

fn print_pay_gas_args(args: &[u8], indent: &str) {
    let Ok((dest_chain, rest)) = decode_borsh_string(args) else {
        return;
    };
    println_kv(indent, "destination_chain", format!("\"{dest_chain}\""));
    let Ok((dest_addr, rest)) = decode_borsh_string(rest) else {
        return;
    };
    println_kv(indent, "destination_address", format!("\"{dest_addr}\""));
    if rest.len() < 32 {
        return;
    }
    println_kv(indent, "payload_hash", hex::encode(&rest[..32]));
    let rest = &rest[32..];
    if rest.len() >= 8 {
        let gas = u64::from_le_bytes(rest[..8].try_into().unwrap_or_default());
        println_kv(indent, "gas_amount", format!("{gas} lamports"));
    }
}

fn print_init_payload_session_args(args: &[u8], indent: &str) {
    if args.len() < 33 {
        return;
    }
    println_kv(indent, "merkle_root", hex::encode(&args[..32]));
    let payload_type = match args[32] {
        0 => "ApproveMessages",
        1 => "RotateSigners",
        _ => "Unknown",
    };
    println_kv(indent, "payload_type", payload_type);
}

// payload_merkle_root: [u8;32]
// verifier_info: SigningVerifierSetInfo {
//   signature: [u8;65], leaf: VerifierSetLeaf { nonce: u64, quorum: u128,
//   signer_pubkey: [u8;33], signer_weight: u128, position: u16, set_size: u16,
//   domain_separator: [u8;32] }, merkle_proof: Vec<u8>, payload_type: u8 }
fn print_verify_signature_args(args: &[u8], indent: &str) {
    if args.len() < 32 {
        return;
    }
    println_kv(indent, "payload_merkle_root", hex::encode(&args[..32]));
    let rest = &args[32..];
    if rest.len() < 65 + 8 + 16 + 33 + 16 + 2 + 2 + 32 {
        return;
    }
    let sig = &rest[..65];
    println_kv(indent, "signature", format!("0x{}", hex::encode(sig)));
    let leaf = &rest[65..];
    let nonce = u64::from_le_bytes(leaf[..8].try_into().unwrap_or_default());
    let quorum = u128::from_le_bytes(leaf[8..24].try_into().unwrap_or_default());
    let signer_pubkey = hex::encode(&leaf[24..57]);
    let signer_weight = u128::from_le_bytes(leaf[57..73].try_into().unwrap_or_default());
    let position = u16::from_le_bytes(leaf[73..75].try_into().unwrap_or_default());
    let set_size = u16::from_le_bytes(leaf[75..77].try_into().unwrap_or_default());
    println_kv(indent, "signer", format!("0x{signer_pubkey}"));
    println_kv(
        indent,
        "weight",
        format!("{signer_weight} (quorum: {quorum})"),
    );
    println_kv(
        indent,
        "position",
        format!("{position}/{set_size} (nonce: {nonce})"),
    );
    let rest = &leaf[77 + 32..]; // skip domain_separator
    if let Ok((proof, rest)) = decode_borsh_bytes(rest) {
        println_kv(indent, "merkle_proof", format!("{} bytes", proof.len()));
        if !rest.is_empty() {
            let payload_type = match rest[0] {
                0 => "ApproveMessages",
                1 => "RotateSigners",
                _ => "Unknown",
            };
            println_kv(indent, "payload_type", payload_type);
        }
    }
}

fn print_send_memo_args(args: &[u8], indent: &str) {
    let Ok((dest_chain, rest)) = decode_borsh_string(args) else {
        return;
    };
    println_kv(indent, "destination_chain", format!("\"{dest_chain}\""));
    let Ok((dest_addr, rest)) = decode_borsh_string(rest) else {
        return;
    };
    println_kv(indent, "destination_address", format!("\"{dest_addr}\""));
    if let Ok((memo, _)) = decode_borsh_string(rest) {
        println_kv(indent, "memo", format!("\"{memo}\""));
    }
}

fn print_interchain_transfer_args(args: &[u8], indent: &str) {
    if args.len() < 32 {
        return;
    }
    println_kv(indent, "token_id", hex::encode(&args[..32]));
    let rest = &args[32..];
    let Ok((dest_chain, rest)) = decode_borsh_string(rest) else {
        return;
    };
    println_kv(indent, "destination_chain", format!("\"{dest_chain}\""));
    let Ok((dest_addr_bytes, rest)) = decode_borsh_bytes(rest) else {
        return;
    };
    let dest_addr = format_address_bytes(&dest_addr_bytes);
    println_kv(indent, "destination_address", dest_addr);
    if rest.len() >= 16 {
        let amount = u64::from_le_bytes(rest[..8].try_into().unwrap_or_default());
        let gas_value = u64::from_le_bytes(rest[8..16].try_into().unwrap_or_default());
        println_kv(indent, "amount", amount);
        println_kv(indent, "gas_value", format!("{gas_value} lamports"));
    }
}

// Message { cc_id: CrossChainId { chain: String, id: String },
//   source_address: String, destination_chain: String,
//   destination_address: String, payload_hash: [u8;32] }
// payload: Vec<u8>
fn print_execute_args(args: &[u8], indent: &str) {
    let Ok((chain, rest)) = decode_borsh_string(args) else {
        return;
    };
    let Ok((id, rest)) = decode_borsh_string(rest) else {
        return;
    };
    println_kv(indent, "cc_id", format!("{chain}-{id}"));
    let Ok((source_addr, rest)) = decode_borsh_string(rest) else {
        return;
    };
    println_kv(indent, "source_address", format!("\"{source_addr}\""));
    let Ok((dest_chain, rest)) = decode_borsh_string(rest) else {
        return;
    };
    let Ok((dest_addr, rest)) = decode_borsh_string(rest) else {
        return;
    };
    println_kv(indent, "destination_chain", format!("\"{dest_chain}\""));
    println_kv(indent, "destination_address", format!("\"{dest_addr}\""));
    if rest.len() < 32 {
        return;
    }
    println_kv(indent, "payload_hash", hex::encode(&rest[..32]));
    let rest = &rest[32..];
    if let Ok((payload_bytes, _)) = decode_borsh_bytes(rest) {
        let (size, content) = decode_payload(&payload_bytes);
        let payload_line = match content {
            Some(decoded) => format!("{size} → {decoded}"),
            None => size,
        };
        for (j, line) in payload_line.lines().enumerate() {
            if j == 0 {
                println_kv(indent, "payload", line);
            } else {
                println!("{indent}{line}");
            }
        }
    }
}

// Both ValidateMessage and ApproveMessage take a Message (or MerklizedMessage
// containing a Message) as first arg.
// MerklizedMessage: MessageLeaf { Message, position, set_size, domain_sep } + proof
// Message: CrossChainId { chain, id }, source_address, dest_chain, dest_addr, payload_hash
fn print_validate_or_approve_message_args(args: &[u8], indent: &str) {
    let Ok((chain, rest)) = decode_borsh_string(args) else {
        return;
    };
    let Ok((id, rest)) = decode_borsh_string(rest) else {
        return;
    };
    println_kv(indent, "cc_id", format!("{chain}-{id}"));
    let Ok((source_addr, rest)) = decode_borsh_string(rest) else {
        return;
    };
    let Ok((dest_chain, rest)) = decode_borsh_string(rest) else {
        return;
    };
    let Ok((dest_addr, rest)) = decode_borsh_string(rest) else {
        return;
    };
    println_kv(indent, "source_address", format!("\"{source_addr}\""));
    println_kv(indent, "destination_chain", format!("\"{dest_chain}\""));
    println_kv(indent, "destination_address", format!("\"{dest_addr}\""));
    if rest.len() >= 32 {
        println_kv(indent, "payload_hash", hex::encode(&rest[..32]));
    }
}

// token_id: [u8;32], name: String, symbol: String, decimals: u8, minter: Vec<u8>
fn print_execute_deploy_interchain_token_args(args: &[u8], indent: &str) {
    if args.len() < 32 {
        return;
    }
    println_kv(indent, "token_id", hex::encode(&args[..32]));
    let rest = &args[32..];
    let Ok((name, rest)) = decode_borsh_string(rest) else {
        return;
    };
    let Ok((symbol, rest)) = decode_borsh_string(rest) else {
        return;
    };
    if rest.is_empty() {
        return;
    }
    println_kv(indent, "name", format!("\"{name}\""));
    println_kv(indent, "symbol", format!("\"{symbol}\""));
    println_kv(indent, "decimals", rest[0]);
}

// Anchor #[instruction] order: message: Message, source_chain: String,
// source_address: Vec<u8>, destination_address: Pubkey,
// token_id: [u8;32], amount: u64, data: Vec<u8>
// Skip the Message struct, parse from source_chain onward.
fn print_execute_interchain_transfer_args(args: &[u8], indent: &str) {
    let Ok((_chain, rest)) = decode_borsh_string(args) else {
        return;
    };
    let Ok((_id, rest)) = decode_borsh_string(rest) else {
        return;
    };
    let Ok((_src_addr, rest)) = decode_borsh_string(rest) else {
        return;
    };
    let Ok((_dest_chain, rest)) = decode_borsh_string(rest) else {
        return;
    };
    let Ok((_dest_addr, rest)) = decode_borsh_string(rest) else {
        return;
    };
    if rest.len() < 32 {
        return;
    }
    let rest = &rest[32..]; // skip payload_hash
    let Ok((source_chain, rest)) = decode_borsh_string(rest) else {
        return;
    };
    println_kv(indent, "source_chain", format!("\"{source_chain}\""));
    let Ok((source_addr, rest)) = decode_borsh_bytes(rest) else {
        return;
    };
    println_kv(indent, "source_address", format_address_bytes(&source_addr));
    if rest.len() < 72 {
        return;
    }
    let dest = Pubkey::try_from(&rest[..32]).ok();
    let token_id = hex::encode(&rest[32..64]);
    let amount = u64::from_le_bytes(rest[64..72].try_into().unwrap_or_default());
    println_kv(indent, "token_id", token_id);
    if let Some(dest) = dest {
        println_kv(indent, "destination", dest);
    }
    println_kv(indent, "amount", amount);
}

fn print_unknown_args(args: &[u8], indent: &str) {
    if args.len() <= 64 {
        println_kv(indent, "data", hex::encode(args));
    } else {
        println_kv(indent, "data", format!("{} bytes", args.len()));
    }
}

fn decode_call_contract_json(args: &[u8]) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    if let Ok((destination_chain, rest)) = decode_borsh_string(args) {
        map.insert(
            "destination_chain".into(),
            serde_json::json!(destination_chain),
        );
        if let Ok((destination_address, rest)) = decode_borsh_string(rest) {
            map.insert(
                "destination_address".into(),
                serde_json::json!(destination_address),
            );
            if let Ok((payload, _)) = decode_borsh_bytes(rest) {
                map.insert("payload_size".into(), serde_json::json!(payload.len()));
            }
        }
    }
    serde_json::Value::Object(map)
}

fn decode_pay_gas_json(args: &[u8]) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    if let Ok((destination_chain, rest)) = decode_borsh_string(args) {
        map.insert(
            "destination_chain".into(),
            serde_json::json!(destination_chain),
        );
        if let Ok((destination_address, rest)) = decode_borsh_string(rest) {
            map.insert(
                "destination_address".into(),
                serde_json::json!(destination_address),
            );
            if rest.len() >= 40 {
                map.insert(
                    "payload_hash".into(),
                    serde_json::json!(hex::encode(&rest[..32])),
                );
                let gas = u64::from_le_bytes(rest[32..40].try_into().unwrap_or_default());
                map.insert("gas_amount".into(), serde_json::json!(gas));
            }
        }
    }
    serde_json::Value::Object(map)
}

fn decode_verification_json(name: &str, args: &[u8]) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    if name == "InitializePayloadVerificationSession" && args.len() >= 33 {
        map.insert(
            "merkle_root".into(),
            serde_json::json!(hex::encode(&args[..32])),
        );
        let payload_type = match args[32] {
            0 => "ApproveMessages",
            1 => "RotateSigners",
            _ => "Unknown",
        };
        map.insert("payload_type".into(), serde_json::json!(payload_type));
    } else if name == "VerifySignature" && args.len() >= 32 {
        map.insert(
            "payload_merkle_root".into(),
            serde_json::json!(hex::encode(&args[..32])),
        );
        if args.len() >= 32 + 65 + 24 + 33 {
            let leaf = &args[32 + 65..];
            if leaf.len() >= 57 {
                map.insert(
                    "signer".into(),
                    serde_json::json!(hex::encode(&leaf[24..57])),
                );
            }
        }
    }
    serde_json::Value::Object(map)
}

fn decode_interchain_transfer_json(args: &[u8]) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    if args.len() >= 32 {
        map.insert(
            "token_id".into(),
            serde_json::json!(hex::encode(&args[..32])),
        );
        if let Ok((destination_chain, rest)) = decode_borsh_string(&args[32..]) {
            map.insert(
                "destination_chain".into(),
                serde_json::json!(destination_chain),
            );
            if let Ok((destination_address, rest)) = decode_borsh_bytes(rest) {
                map.insert(
                    "destination_address".into(),
                    serde_json::json!(format_address_bytes(&destination_address)),
                );
                if rest.len() >= 16 {
                    let amount = u64::from_le_bytes(rest[..8].try_into().unwrap_or_default());
                    map.insert("amount".into(), serde_json::json!(amount));
                }
            }
        }
    }
    serde_json::Value::Object(map)
}

fn decode_message_json(args: &[u8], include_destination: bool) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    if let Ok((chain, rest)) = decode_borsh_string(args)
        && let Ok((id, rest)) = decode_borsh_string(rest)
    {
        map.insert("cc_id".into(), serde_json::json!(format!("{chain}-{id}")));
        if let Ok((source_address, rest)) = decode_borsh_string(rest) {
            map.insert("source_address".into(), serde_json::json!(source_address));
            if include_destination
                && let Ok((destination_chain, rest)) = decode_borsh_string(rest)
                && let Ok((destination_address, rest)) = decode_borsh_string(rest)
            {
                map.insert(
                    "destination_chain".into(),
                    serde_json::json!(destination_chain),
                );
                map.insert(
                    "destination_address".into(),
                    serde_json::json!(destination_address),
                );
                if rest.len() >= 32 {
                    map.insert(
                        "payload_hash".into(),
                        serde_json::json!(hex::encode(&rest[..32])),
                    );
                }
            }
        }
    }
    serde_json::Value::Object(map)
}

/// Decode instruction arguments into a JSON map (for machine-readable output).
pub fn decode_instruction_args_json(ix_name: &str, data: &[u8]) -> serde_json::Value {
    if data.len() <= 8 {
        return json!({});
    }
    let args = &data[8..];

    match ix_name {
        "CallContract" => decode_call_contract_json(args),
        "PayGas" => decode_pay_gas_json(args),
        "InitializePayloadVerificationSession" | "VerifySignature" => {
            decode_verification_json(ix_name, args)
        }
        "InterchainTransfer" => decode_interchain_transfer_json(args),
        "ApproveMessage" | "ValidateMessage" => decode_message_json(args, true),
        "Execute" => decode_message_json(args, false),
        _ => {
            let mut m = serde_json::Map::new();
            m.insert("raw_size".into(), json!(args.len()));
            json!(m)
        }
    }
}
