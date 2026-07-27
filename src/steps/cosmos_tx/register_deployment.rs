//! `RegisterDeployment` step. After the chain contracts are instantiated,
//! we ask the Coordinator to mark the deployment as registered. Same
//! direct-vs-governance branching as the other cosmos-tx steps.

use eyre::Result;
use serde_json::json;

use super::StepTxContext;
use super::defaults::DEFAULT_PROPOSAL_DEPOSIT_UAXL;
use crate::commands::deploy::DeployContext;
use crate::cosmos::{
    build_execute_msg_any, build_submit_proposal_any, extract_proposal_id,
    read_axelar_contract_field, sign_and_broadcast_cosmos_tx,
};
use crate::ui;

pub(super) async fn run_register_deployment(
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
    ui::info(&format!("registering deployment for {chain_axelar_id}..."));

    let coordinator_addr =
        read_axelar_contract_field(&ctx.target_json, "/axelar/contracts/Coordinator/address")?;
    let governance_address =
        read_axelar_contract_field(&ctx.target_json, "/axelar/governanceAddress")?;

    let deployment_name = read_axelar_contract_field(
        &ctx.target_json,
        &format!("/axelar/contracts/Coordinator/deployments/{chain_axelar_id}/deploymentName"),
    )?;

    let execute_msg = json!({
        "register_deployment": {
            "deployment_name": deployment_name
        }
    });

    let sender = if use_governance {
        &governance_address
    } else {
        axelar_address
    };
    let inner_msg = build_execute_msg_any(sender, &coordinator_addr, &execute_msg)?;

    let messages = if use_governance {
        let deposit_amount = read_axelar_contract_field(
            &ctx.target_json,
            "/axelar/govProposalExpeditedDepositAmount",
        )
        .unwrap_or_else(|_| DEFAULT_PROPOSAL_DEPOSIT_UAXL.to_string());
        let title = format!("Register {chain_axelar_id} deployment on Coordinator");
        vec![build_submit_proposal_any(
            axelar_address,
            vec![inner_msg],
            &title,
            &title,
            &deposit_amount,
            fee_denom,
            true,
        )?]
    } else {
        vec![inner_msg]
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
