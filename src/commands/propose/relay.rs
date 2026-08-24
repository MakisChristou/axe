//! `--relay`: deliver a passed governance proposal to the edge chain and run it.
//!
//! Second leg of the GMP, the part the amplifier relayer often skips for
//! gov-originated messages: find the `wasm-contract_called` the gov module
//! emitted → `construct_proof` → wait for signing → submit the proof to the
//! edge gateway (approve) → `ASG.execute` (consume → schedule/approve) → then
//! `executeOperatorProposal` (operator fast-path) or wait-for-eta +
//! `executeProposal` (time-lock).

use std::str::FromStr;
use std::time::Duration;

use alloy::primitives::{Address, B256, Bytes, U256, keccak256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use cosmrs::crypto::secp256k1::SigningKey;
use eyre::{Result, WrapErr};
use serde_json::Value;

use crate::commands::test_helpers::{extract_event_attr, wait_for_proof};
use crate::cosmos::{build_execute_msg_any, sign_and_broadcast_cosmos_tx};
use crate::evm::{AxelarAmplifierGateway, AxelarServiceGovernance, broadcast_and_log};
use crate::http;
use crate::ui;

use super::helpers::AsgInfo;
use super::types::{ProposalType, ResolvedConfig};

/// How many recent blocks to scan for the gov GMP, and how many times to
/// re-poll (the exec block may not be indexed the instant the vote passes).
const SCAN_DEPTH: u64 = 40;
const FIND_ATTEMPTS: usize = 12;
const FIND_INTERVAL: Duration = Duration::from_secs(5);
const ETA_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// What the relay needs to know about the just-passed proposal.
pub struct RelayPlan {
    pub ptype: ProposalType,
    pub target: Address,
    pub calldata: Bytes,
    pub payload: Bytes,
}

/// The governance GMP the gov module emitted via `AxelarnetGateway.call_contract`.
struct GovMessage {
    message_id: String,
    source_chain: String,
    source_address: String,
}

pub async fn relay(
    cfg: &ResolvedConfig,
    asg_info: &AsgInfo,
    plan: &RelayPlan,
    signing_key: &SigningKey,
    axelar_address: &str,
) -> Result<()> {
    ui::section("relay to edge chain");
    let payload_hash = keccak256(&plan.payload);

    let msg = find_governance_message(&cfg.axelar_rpc, payload_hash).await?;
    ui::kv("message_id", &msg.message_id);

    let execute_data = build_proof(cfg, signing_key, axelar_address, &msg).await?;

    let signer = load_evm_signer()?;
    let relayer = signer.address();
    let provider = ProviderBuilder::new()
        .wallet(signer)
        .connect_http(cfg.edge_rpc.parse().wrap_err("invalid edge rpc url")?);
    ui::address("relayer", &relayer.to_string());
    ensure_funded(&provider, relayer).await?;

    let gateway = Address::from_str(&cfg.gateway_address)?;
    let asg_addr = Address::from_str(&cfg.asg_address)?;

    submit_proof(&provider, gateway, &execute_data).await?;
    assert_approved(&provider, gateway, asg_addr, &msg, payload_hash).await?;
    consume_on_asg(&provider, asg_addr, &msg, &plan.payload).await?;

    match plan.ptype {
        ProposalType::Operator => {
            execute_operator(&provider, asg_addr, asg_info, relayer, plan).await
        }
        ProposalType::Timelock => execute_timelock(&provider, asg_addr, plan).await,
    }
}

/// Scan recent `block_results` for the `wasm-contract_called` event whose
/// `payload_hash` matches ours (gov runs in the EndBlocker — no tx to look up).
async fn find_governance_message(rpc: &str, payload_hash: B256) -> Result<GovMessage> {
    let want = format!("{payload_hash:x}");
    let spinner = ui::wait_spinner("locating governance GMP in recent blocks...");
    for _ in 0..FIND_ATTEMPTS {
        let latest = latest_height(rpc).await?;
        for height in (latest.saturating_sub(SCAN_DEPTH)..=latest).rev() {
            if let Some(msg) = scan_block(rpc, height, &want).await? {
                spinner.finish_and_clear();
                ui::kv("exec block", &height.to_string());
                return Ok(msg);
            }
        }
        tokio::time::sleep(FIND_INTERVAL).await;
    }
    spinner.finish_and_clear();
    Err(eyre::eyre!(
        "could not find the governance GMP (wasm-contract_called, payload_hash 0x{want}) \
         in the last {SCAN_DEPTH} blocks — the relayer may have already consumed it"
    ))
}

async fn latest_height(rpc: &str) -> Result<u64> {
    let resp: Value = http::client()
        .get(format!("{}/status", rpc.trim_end_matches('/')))
        .send()
        .await?
        .json()
        .await?;
    resp.pointer("/result/sync_info/latest_block_height")
        .and_then(Value::as_str)
        .and_then(|h| h.parse().ok())
        .ok_or_else(|| eyre::eyre!("could not read latest_block_height from {rpc}/status"))
}

async fn scan_block(rpc: &str, height: u64, want_hash: &str) -> Result<Option<GovMessage>> {
    let url = format!(
        "{}/block_results?height={height}",
        rpc.trim_end_matches('/')
    );
    let resp: Value = http::client().get(&url).send().await?.json().await?;
    let events = resp
        .pointer("/result/finalize_block_events")
        .or_else(|| resp.pointer("/finalize_block_events"))
        .and_then(Value::as_array);
    let Some(events) = events else {
        return Ok(None);
    };
    Ok(events
        .iter()
        .filter(|e| {
            e.get("type")
                .and_then(Value::as_str)
                .is_some_and(|t| t.ends_with("contract_called"))
        })
        .find_map(|e| match_contract_called(e, want_hash)))
}

/// If this `contract_called` event's `payload_hash` matches, pull the fields
/// the relay needs. Attributes are plain strings on CometBFT 0.38.
fn match_contract_called(event: &Value, want_hash: &str) -> Option<GovMessage> {
    let attrs = event.get("attributes").and_then(Value::as_array)?;
    let get = |key: &str| -> Option<String> {
        attrs
            .iter()
            .find(|a| a.get("key").and_then(Value::as_str) == Some(key))
            .and_then(|a| a.get("value").and_then(Value::as_str))
            .map(str::to_string)
    };
    let hash = get("payload_hash")?;
    if !hash
        .trim_start_matches("0x")
        .eq_ignore_ascii_case(want_hash)
    {
        return None;
    }
    Some(GovMessage {
        message_id: get("message_id")?,
        source_chain: get("source_chain")?,
        source_address: get("source_address")?,
    })
}

async fn build_proof(
    cfg: &ResolvedConfig,
    signing_key: &SigningKey,
    axelar_address: &str,
    msg: &GovMessage,
) -> Result<String> {
    ui::address("multisig prover", &cfg.multisig_prover);
    let construct_proof_msg = serde_json::json!({
        "construct_proof": [{
            "source_chain": msg.source_chain,
            "message_id": msg.message_id,
        }]
    });
    let any = build_execute_msg_any(axelar_address, &cfg.multisig_prover, &construct_proof_msg)?;
    let resp = sign_and_broadcast_cosmos_tx(
        signing_key,
        axelar_address,
        &cfg.lcd,
        &cfg.chain_id,
        &cfg.fee_denom,
        cfg.gas_price,
        vec![any],
    )
    .await?;
    let session_id = extract_event_attr(&resp, "multisig_session_id")?;
    ui::kv("multisig_session_id", &session_id);

    let proof = wait_for_proof(&cfg.lcd, &cfg.multisig_prover, &session_id).await?;
    proof["status"]["completed"]["execute_data"]
        .as_str()
        .map(String::from)
        .ok_or_else(|| eyre::eyre!("no execute_data in completed proof"))
}

async fn submit_proof<P: Provider>(
    provider: &P,
    gateway: Address,
    execute_data_hex: &str,
) -> Result<()> {
    let execute_data = alloy::hex::decode(execute_data_hex)?;
    let tx = TransactionRequest::default()
        .to(gateway)
        .input(Bytes::from(execute_data).into());
    let pending = provider.send_transaction(tx).await?;
    broadcast_and_log(pending, "submitProof tx").await?;
    Ok(())
}

async fn assert_approved<P: Provider>(
    provider: &P,
    gateway: Address,
    asg: Address,
    msg: &GovMessage,
    payload_hash: B256,
) -> Result<()> {
    let approved = AxelarAmplifierGateway::new(gateway, provider)
        .isMessageApproved(
            msg.source_chain.clone(),
            msg.message_id.clone(),
            msg.source_address.clone(),
            asg,
            payload_hash,
        )
        .call()
        .await?;
    if !approved {
        return Err(eyre::eyre!(
            "message not approved on the edge gateway after submitProof"
        ));
    }
    ui::success("message approved on edge gateway");
    Ok(())
}

async fn consume_on_asg<P: Provider>(
    provider: &P,
    asg: Address,
    msg: &GovMessage,
    payload: &Bytes,
) -> Result<()> {
    let command_id = command_id(&msg.source_chain, &msg.message_id);
    let asg_contract = AxelarServiceGovernance::new(asg, provider);
    let call = asg_contract.execute(
        command_id,
        msg.source_chain.clone(),
        msg.source_address.clone(),
        payload.clone(),
    );
    let pending = call.send().await?;
    broadcast_and_log(pending, "ASG.execute (consume) tx").await?;
    ui::success("governance message consumed on ASG");
    Ok(())
}

/// EVM gateway approval key: `keccak256(sourceChain + "_" + messageId)`.
fn command_id(source_chain: &str, message_id: &str) -> B256 {
    let mut buf = Vec::with_capacity(source_chain.len() + 1 + message_id.len());
    buf.extend_from_slice(source_chain.as_bytes());
    buf.push(b'_');
    buf.extend_from_slice(message_id.as_bytes());
    keccak256(&buf)
}

async fn execute_operator<P: Provider>(
    provider: &P,
    asg: Address,
    asg_info: &AsgInfo,
    relayer: Address,
    plan: &RelayPlan,
) -> Result<()> {
    if relayer != asg_info.operator {
        ui::warn(&format!(
            "relayer {relayer} is not the ASG operator {} — the operator proposal is APPROVED \
             on-chain but NOT executed (we won't sign as a non-operator).",
            asg_info.operator
        ));
        ui::action_required(&[
            "Run this with the operator key to finish (executeOperatorProposal):",
            &format!(
                "cast send {asg} \"executeOperatorProposal(address,bytes,uint256)\" {} 0x{} 0 \
                 --rpc-url <bera-rpc> --private-key \"$EVM_GOVERNANCE_OPERATOR_KEY\"",
                plan.target,
                alloy::hex::encode(&plan.calldata),
            ),
        ]);
        return Ok(());
    }
    let asg_contract = AxelarServiceGovernance::new(asg, provider);
    let approved = asg_contract
        .isOperatorProposalApproved(plan.target, plan.calldata.clone(), U256::ZERO)
        .call()
        .await?;
    if !approved {
        return Err(eyre::eyre!(
            "operator proposal not approved after consume — aborting before execute"
        ));
    }
    let pending = asg_contract
        .executeOperatorProposal(plan.target, plan.calldata.clone(), U256::ZERO)
        .send()
        .await?;
    broadcast_and_log(pending, "executeOperatorProposal tx").await?;
    ui::success("operator proposal executed — target call ran");
    Ok(())
}

async fn execute_timelock<P: Provider>(provider: &P, asg: Address, plan: &RelayPlan) -> Result<()> {
    let asg_contract = AxelarServiceGovernance::new(asg, provider);
    let spinner = ui::wait_spinner("waiting for time-lock eta...");
    loop {
        let eta = asg_contract
            .getProposalEta(plan.target, plan.calldata.clone(), U256::ZERO)
            .call()
            .await?;
        let eta = u64::try_from(eta).unwrap_or(u64::MAX);
        if eta == 0 {
            spinner.finish_and_clear();
            return Err(eyre::eyre!("proposal not scheduled (eta=0) after consume"));
        }
        let now = chrono::Utc::now().timestamp().max(0) as u64;
        if now >= eta {
            spinner.finish_and_clear();
            break;
        }
        spinner.set_message(format!("time-lock eta in {}s", eta - now));
        tokio::time::sleep(ETA_POLL_INTERVAL).await;
    }
    let pending = asg_contract
        .executeProposal(plan.target, plan.calldata.clone(), U256::ZERO)
        .send()
        .await?;
    broadcast_and_log(pending, "executeProposal tx").await?;
    ui::success("time-lock proposal executed — target call ran");
    Ok(())
}

/// Whether the environment holds a usable key for the operator execute step,
/// resolved before submitting so the confirm prompt can warn (and refuse the
/// operator-execute we can't sign).
pub enum OperatorKey {
    /// Resolved key whose address *is* the ASG operator — can executeOperatorProposal.
    Operator(Address),
    /// Resolved key, but not the operator — can relay + approve only.
    NotOperator(Address),
    /// No EVM key in the environment at all.
    Missing,
}

/// Classify the EVM key in the environment against the ASG operator address.
pub fn operator_key_status(operator: Address) -> OperatorKey {
    match env_signer() {
        Some(signer) if signer.address() == operator => OperatorKey::Operator(operator),
        Some(signer) => OperatorKey::NotOperator(signer.address()),
        None => OperatorKey::Missing,
    }
}

/// `EVM_GOVERNANCE_OPERATOR_KEY` (preferred) or `EVM_PRIVATE_KEY`, parsed.
fn env_signer() -> Option<PrivateKeySigner> {
    let key = std::env::var("EVM_GOVERNANCE_OPERATOR_KEY")
        .or_else(|_| std::env::var("EVM_PRIVATE_KEY"))
        .ok()?;
    key.parse().ok()
}

/// Pre-submit guard (when `--relay`): resolve the EVM relayer key and confirm
/// it's funded on the edge chain, so we refuse *before* spending a proposal we
/// couldn't relay. Errors if no key is set or its balance is zero.
pub async fn preflight_relayer(cfg: &ResolvedConfig) -> Result<()> {
    let signer = load_evm_signer()?;
    let relayer = signer.address();
    let provider =
        ProviderBuilder::new().connect_http(cfg.edge_rpc.parse().wrap_err("invalid edge rpc url")?);
    ensure_funded(&provider, relayer).await?;
    ui::kv(
        "relay preflight",
        &format!("{relayer} funded on {} ✓", cfg.edge_axelar_id),
    );
    Ok(())
}

fn load_evm_signer() -> Result<PrivateKeySigner> {
    env_signer().ok_or_else(|| {
        eyre::eyre!(
            "set EVM_GOVERNANCE_OPERATOR_KEY (preferred) or EVM_PRIVATE_KEY to relay/execute on the edge chain"
        )
    })
}

async fn ensure_funded<P: Provider>(provider: &P, addr: Address) -> Result<()> {
    let balance = provider.get_balance(addr).await?;
    if balance.is_zero() {
        return Err(eyre::eyre!(
            "relayer {addr} has zero balance on the edge chain — fund it for gas"
        ));
    }
    Ok(())
}
