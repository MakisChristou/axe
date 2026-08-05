//! `CreateRewardPools` and `AddRewards` steps. The first creates two reward
//! pools (one for the multisig, one for the voting verifier) — wrapped in a
//! governance proposal where applicable. The second sends `add_rewards` from
//! the relayer wallet directly with `funds` attached, no governance.

use cosmos_sdk_proto::cosmos::base::v1beta1::Coin as ProtoCoin;
use eyre::Result;
use serde_json::{Value, json};

use super::StepTxContext;
use super::defaults::{DEFAULT_PROPOSAL_DEPOSIT_UAXL, DEFAULT_REWARD_AMOUNT_UAXL};
use crate::commands::deploy::DeployContext;
use crate::cosmos::{
    build_execute_msg_any, build_execute_msg_any_with_funds, build_submit_proposal_any,
    extract_proposal_id, read_axelar_contract_field, sign_and_broadcast_cosmos_tx,
};
use crate::ui;

fn reward_pool_messages(
    env: &str,
    chain: &str,
    voting_verifier: &str,
    multisig: &str,
) -> [Value; 2] {
    let (epoch_duration, participation_threshold, rewards_per_epoch) = match env {
        "devnet-amplifier" => ("100", json!(["7", "10"]), "100"),
        "mainnet" => ("14845", json!(["8", "10"]), "3424660000"),
        _ => ("600", json!(["7", "10"]), "100"),
    };
    let create = |contract: &str| {
        json!({
            "create_pool": {
                "params": {
                    "epoch_duration": epoch_duration,
                    "participation_threshold": participation_threshold,
                    "rewards_per_epoch": rewards_per_epoch
                },
                "pool_id": {
                    "chain_name": chain,
                    "contract": contract
                }
            }
        })
    };
    [create(voting_verifier), create(multisig)]
}

pub(super) async fn run_create_reward_pools(
    ctx: &mut DeployContext,
    tx: StepTxContext<'_>,
) -> Result<()> {
    let StepTxContext {
        signing_key,
        axelar_address,
        lcd,
        chain_id,
        fee_denom,
        gas_price,
        use_governance,
        chain_axelar_id,
        env,
        proposal_key,
    } = tx;
    ui::info(&format!("creating reward pools for {chain_axelar_id}..."));

    let rewards_addr =
        read_axelar_contract_field(&ctx.target_json, "/axelar/contracts/Rewards/address").await?;
    let governance_address =
        read_axelar_contract_field(&ctx.target_json, "/axelar/governanceAddress").await?;
    let multisig_addr =
        read_axelar_contract_field(&ctx.target_json, "/axelar/contracts/Multisig/address").await?;
    let voting_verifier_addr = read_axelar_contract_field(
        &ctx.target_json,
        &format!("/axelar/contracts/VotingVerifier/{chain_axelar_id}/address"),
    )
    .await?;

    let [msg1, msg2] =
        reward_pool_messages(env, chain_axelar_id, &voting_verifier_addr, &multisig_addr);

    let sender = if use_governance {
        &governance_address
    } else {
        axelar_address
    };
    let inner_msg1 = build_execute_msg_any(sender, &rewards_addr, &msg1)?;
    let inner_msg2 = build_execute_msg_any(sender, &rewards_addr, &msg2)?;

    let messages = if use_governance {
        let deposit_amount = read_axelar_contract_field(
            &ctx.target_json,
            "/axelar/govProposalExpeditedDepositAmount",
        )
        .await
        .unwrap_or_else(|_| DEFAULT_PROPOSAL_DEPOSIT_UAXL.to_string());
        let title = format!("Create reward pools for {chain_axelar_id}");
        let summary =
            format!("Create reward pools for {chain_axelar_id} voting verifier and multisig");
        vec![build_submit_proposal_any(
            axelar_address,
            vec![inner_msg1, inner_msg2],
            &title,
            &summary,
            &deposit_amount,
            fee_denom,
            true,
        )?]
    } else {
        vec![inner_msg1, inner_msg2]
    };

    let tx_resp = sign_and_broadcast_cosmos_tx(
        signing_key,
        axelar_address,
        lcd,
        chain_id,
        fee_denom,
        gas_price,
        messages,
    )
    .await?;

    if use_governance {
        let proposal_id = extract_proposal_id(&tx_resp)?;
        ui::kv("proposal submitted", &proposal_id.to_string());
        ui::action_required(&[
            "Vote on the proposal:",
            &format!("./vote_{env}_proposal.sh {env}-nodes {proposal_id}"),
        ]);
        ctx.state
            .proposals
            .insert(proposal_key.to_string(), proposal_id);
    } else {
        ui::success("direct execution completed");
    }

    Ok(())
}

pub(super) async fn run_add_rewards(ctx: &DeployContext, tx: StepTxContext<'_>) -> Result<()> {
    let StepTxContext {
        signing_key,
        axelar_address,
        lcd,
        chain_id,
        fee_denom,
        gas_price,
        chain_axelar_id,
        ..
    } = tx;
    ui::info(&format!("adding rewards for {chain_axelar_id}..."));

    let rewards_addr =
        read_axelar_contract_field(&ctx.target_json, "/axelar/contracts/Rewards/address").await?;
    let multisig_addr =
        read_axelar_contract_field(&ctx.target_json, "/axelar/contracts/Multisig/address").await?;
    let voting_verifier_addr = read_axelar_contract_field(
        &ctx.target_json,
        &format!("/axelar/contracts/VotingVerifier/{chain_axelar_id}/address"),
    )
    .await?;

    let reward_amount = DEFAULT_REWARD_AMOUNT_UAXL;
    let funds = vec![ProtoCoin {
        denom: fee_denom.to_string(),
        amount: reward_amount.to_string(),
    }];

    let msg1 = json!({
        "add_rewards": {
            "pool_id": {
                "chain_name": chain_axelar_id,
                "contract": multisig_addr
            }
        }
    });
    let msg2 = json!({
        "add_rewards": {
            "pool_id": {
                "chain_name": chain_axelar_id,
                "contract": voting_verifier_addr
            }
        }
    });

    let inner_msg1 =
        build_execute_msg_any_with_funds(axelar_address, &rewards_addr, &msg1, funds.clone())?;
    let inner_msg2 = build_execute_msg_any_with_funds(axelar_address, &rewards_addr, &msg2, funds)?;

    ui::info(&format!(
        "sending {reward_amount}{fee_denom} to each reward pool"
    ));
    let tx_resp = sign_and_broadcast_cosmos_tx(
        signing_key,
        axelar_address,
        lcd,
        chain_id,
        fee_denom,
        gas_price,
        vec![inner_msg1, inner_msg2],
    )
    .await?;

    let code = tx_resp
        .pointer("/tx_response/code")
        .and_then(|v: &Value| v.as_u64())
        .unwrap_or(0);
    if code != 0 {
        let raw_log = tx_resp
            .pointer("/tx_response/raw_log")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        return Err(eyre::eyre!(
            "add_rewards tx failed (code {code}): {raw_log}"
        ));
    }
    ui::success("rewards added to both pools");

    Ok(())
}
