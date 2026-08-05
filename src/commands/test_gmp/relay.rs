use cosmrs::crypto::secp256k1::SigningKey;
use eyre::Result;
use serde_json::{Value, json};

use crate::commands::test_helpers::{
    CosmosTxContext, end_poll_with_retry, extract_event_attr, route_messages_with_retry,
    submit_verify_messages_amplifier, wait_for_poll_votes, wait_for_proof,
};
use crate::cosmos::{build_execute_msg_any, sign_and_broadcast_cosmos_tx};
use crate::ui;

/// Bundle of everything an Amplifier relay step needs to sign and
/// broadcast cosmos txs against the source-chain Gateway, VotingVerifier,
/// and destination-chain MultisigProver. `voting_verifier` is optional
/// because some chains route through gateways without a VotingVerifier; the
/// relay then errors only if a poll actually needs ending.
pub struct AmplifierContext {
    pub axelar_address: String,
    pub lcd: String,
    pub chain_id: String,
    pub fee_denom: String,
    pub gas_price: f64,
    pub cosm_gateway: String,
    pub voting_verifier: Option<String>,
    pub multisig_prover: String,
}

/// Run Steps 2-6 of the GMP test: `verify_messages` → wait for votes +
/// `end_poll` → `route_messages` → `construct_proof` → wait for the signed
/// proof. Returns the `execute_data` hex from the completed proof, ready to
/// hand to a destination-chain gateway.
pub async fn run_full_sequence(
    ctx: &AmplifierContext,
    signing_key: &SigningKey,
    gmp_msg: &Value,
    source_chain: &str,
    message_id: &str,
    total_steps: usize,
) -> Result<String> {
    let tx = CosmosTxContext {
        signing_key,
        axelar_address: &ctx.axelar_address,
        lcd: &ctx.lcd,
        chain_id: &ctx.chain_id,
        fee_denom: &ctx.fee_denom,
        gas_price: ctx.gas_price,
    };
    ui::step_header(2, total_steps, "verify_messages");
    let poll_id = submit_verify_messages_amplifier(gmp_msg, tx, &ctx.cosm_gateway).await?;

    if let Some(poll_id) = poll_id {
        ui::kv("poll_id", &poll_id);

        ui::step_header(3, total_steps, "Wait for poll votes + end poll");
        let vv = ctx
            .voting_verifier
            .as_deref()
            .ok_or_else(|| eyre::eyre!("voting verifier address required to end poll"))?;
        wait_for_poll_votes(&ctx.lcd, vv, &poll_id).await?;
        end_poll_with_retry(&poll_id, tx, vv).await?;
    } else {
        ui::info("no new poll created — message already being verified by active verifiers");
        ui::step_header(3, total_steps, "Wait for poll votes + end poll");
        ui::info("skipped (existing poll)");
    }

    ui::step_header(4, total_steps, "route_messages");
    route_messages_with_retry(gmp_msg, tx, &ctx.cosm_gateway).await?;

    ui::step_header(5, total_steps, "construct_proof");
    ui::address("multisig prover", &ctx.multisig_prover);
    let construct_proof_msg = json!({
        "construct_proof": [{
            "source_chain": source_chain,
            "message_id": message_id,
        }]
    });
    let construct_any = build_execute_msg_any(
        &ctx.axelar_address,
        &ctx.multisig_prover,
        &construct_proof_msg,
    )?;
    let construct_resp = sign_and_broadcast_cosmos_tx(
        signing_key,
        &ctx.axelar_address,
        &ctx.lcd,
        &ctx.chain_id,
        &ctx.fee_denom,
        ctx.gas_price,
        vec![construct_any],
    )
    .await?;

    let session_id = extract_event_attr(&construct_resp, "multisig_session_id")?;
    ui::kv("multisig_session_id", &session_id);

    ui::step_header(6, total_steps, "Wait for proof signing");
    let proof = wait_for_proof(&ctx.lcd, &ctx.multisig_prover, &session_id).await?;
    ui::success("proof ready");

    proof["status"]["completed"]["execute_data"]
        .as_str()
        .map(String::from)
        .ok_or_else(|| eyre::eyre!("no execute_data in proof response"))
}
