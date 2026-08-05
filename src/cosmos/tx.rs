//! Cosmos transaction building, simulation, signing, and broadcast. The
//! high-level entry point is [`sign_and_broadcast_cosmos_tx`]; the lower-level
//! `build_*` helpers exist so callers that need to batch messages or post a
//! gov proposal can compose without re-implementing the sign/sim loop.

use cosmos_sdk_proto::cosmos::base::v1beta1::Coin as ProtoCoin;
use cosmos_sdk_proto::cosmos::gov::v1::MsgSubmitProposal;
use cosmos_sdk_proto::cosmwasm::wasm::v1::MsgExecuteContract as ProtoMsgExecuteContract;
use cosmrs::crypto::secp256k1::SigningKey;
use cosmrs::tx::{self, Fee, SignDoc, SignerInfo};
use eyre::Result;
use prost::Message;
use serde_json::Value;

use super::rpc::{lcd_broadcast_tx, lcd_query_account, lcd_simulate_tx, lcd_wait_for_tx};
use crate::ui;

/// Multiplier applied to the simulated gas to derive the broadcast gas limit.
/// Cosmwasm route/end_poll simulation can underestimate actual usage by a few
/// percent (we've seen ~4% over the 2.0× limit in the wild), so we keep a
/// generous buffer.
const GAS_MULTIPLIER: f64 = 3.0;

struct CosmosTxSignInput<'a> {
    chain_id: &'a str,
    account_number: u64,
    sequence: u64,
    gas_limit: u64,
    fee_amount: u128,
    fee_denom: &'a str,
    messages: Vec<cosmrs::Any>,
}

fn build_and_sign_cosmos_tx(
    signing_key: &SigningKey,
    input: CosmosTxSignInput<'_>,
) -> Result<Vec<u8>> {
    let tx_body = tx::Body::new(input.messages, "", 0u32);
    let signer_info = SignerInfo::single_direct(Some(signing_key.public_key()), input.sequence);
    let fee = Fee::from_amount_and_gas(
        cosmrs::Coin {
            denom: input
                .fee_denom
                .parse()
                .map_err(|e| eyre::eyre!("invalid denom: {e}"))?,
            amount: input.fee_amount,
        },
        input.gas_limit,
    );
    let auth_info = signer_info.auth_info(fee);
    let cosmos_chain_id: cosmrs::tendermint::chain::Id = input
        .chain_id
        .parse()
        .map_err(|e| eyre::eyre!("invalid chain id: {e}"))?;
    let sign_doc = SignDoc::new(&tx_body, &auth_info, &cosmos_chain_id, input.account_number)
        .map_err(|e| eyre::eyre!("sign doc error: {e}"))?;
    let tx_signed = sign_doc
        .sign(signing_key)
        .map_err(|e| eyre::eyre!("signing error: {e}"))?;
    let tx_bytes = tx_signed
        .to_bytes()
        .map_err(|e| eyre::eyre!("serialize error: {e}"))?;
    Ok(tx_bytes)
}

/// Build a MsgExecuteContract as protobuf Any
pub fn build_execute_msg_any(
    sender: &str,
    contract: &str,
    msg_json: &Value,
) -> Result<cosmrs::Any> {
    build_execute_msg_any_with_funds(sender, contract, msg_json, vec![])
}

pub fn build_execute_msg_any_with_funds(
    sender: &str,
    contract: &str,
    msg_json: &Value,
    funds: Vec<ProtoCoin>,
) -> Result<cosmrs::Any> {
    let msg_bytes = serde_json::to_vec(msg_json)?;
    let proto_msg = ProtoMsgExecuteContract {
        sender: sender.to_string(),
        contract: contract.to_string(),
        msg: msg_bytes,
        funds,
    };
    let mut buf = Vec::new();
    proto_msg.encode(&mut buf)?;
    Ok(cosmrs::Any {
        type_url: "/cosmwasm.wasm.v1.MsgExecuteContract".to_string(),
        value: buf,
    })
}

/// Wrap execute messages in a MsgSubmitProposal as protobuf Any
pub fn build_submit_proposal_any(
    proposer: &str,
    inner_messages: Vec<cosmrs::Any>,
    title: &str,
    summary: &str,
    deposit_amount: &str,
    deposit_denom: &str,
    expedited: bool,
) -> Result<cosmrs::Any> {
    let prost_messages: Vec<tendermint_proto::google::protobuf::Any> = inner_messages
        .into_iter()
        .map(|a| tendermint_proto::google::protobuf::Any {
            type_url: a.type_url,
            value: a.value,
        })
        .collect();

    let proposal = MsgSubmitProposal {
        messages: prost_messages,
        initial_deposit: vec![ProtoCoin {
            denom: deposit_denom.to_string(),
            amount: deposit_amount.to_string(),
        }],
        proposer: proposer.to_string(),
        metadata: String::new(),
        title: title.to_string(),
        summary: summary.to_string(),
        expedited,
    };
    let mut buf = Vec::new();
    proposal.encode(&mut buf)?;
    Ok(cosmrs::Any {
        type_url: "/cosmos.gov.v1.MsgSubmitProposal".to_string(),
        value: buf,
    })
}

/// Sign, simulate, re-sign with correct gas, broadcast, and wait for inclusion.
pub async fn sign_and_broadcast_cosmos_tx(
    signing_key: &SigningKey,
    address: &str,
    lcd: &str,
    chain_id: &str,
    fee_denom: &str,
    gas_price: f64,
    messages: Vec<cosmrs::Any>,
) -> Result<Value> {
    let (account_number, sequence) = lcd_query_account(lcd, address).await?;
    ui::kv(
        "account",
        &format!("{address}, number={account_number}, sequence={sequence}"),
    );

    let sim_tx = build_and_sign_cosmos_tx(
        signing_key,
        CosmosTxSignInput {
            chain_id,
            account_number,
            sequence,
            gas_limit: 10_000_000,
            fee_amount: 0,
            fee_denom,
            messages: messages.clone(),
        },
    )?;

    let gas_used = lcd_simulate_tx(lcd, &sim_tx).await?;
    let gas_limit = (gas_used as f64 * GAS_MULTIPLIER) as u64;
    let fee_amount = ((gas_limit as f64) * gas_price).ceil() as u128;
    ui::kv(
        "gas",
        &format!("used={gas_used}, limit={gas_limit}, fee={fee_amount}{fee_denom}"),
    );

    let tx_bytes = build_and_sign_cosmos_tx(
        signing_key,
        CosmosTxSignInput {
            chain_id,
            account_number,
            sequence,
            gas_limit,
            fee_amount,
            fee_denom,
            messages,
        },
    )?;

    let broadcast_resp = lcd_broadcast_tx(lcd, &tx_bytes).await?;
    let tx_hash = broadcast_resp
        .pointer("/tx_response/txhash")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    ui::tx_hash("tx hash", &tx_hash);

    ui::info("waiting for tx confirmation...");
    let tx_resp = lcd_wait_for_tx(lcd, &tx_hash).await?;
    Ok(tx_resp)
}
