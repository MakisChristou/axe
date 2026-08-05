//! `RegisterItsOnHub` step. Tells the InterchainTokenService Hub about the
//! chain's ITS edge contract + ABI translator. Wrapped in a governance
//! proposal on non-devnet networks like the other cosmos-tx steps.

use eyre::Result;
use serde_json::{Value, json};

use super::StepTxContext;
use super::defaults::DEFAULT_PROPOSAL_DEPOSIT_UAXL;
use crate::commands::deploy::DeployContext;
use crate::cosmos::{
    build_execute_msg_any, build_submit_proposal_any, extract_proposal_id,
    read_axelar_contract_field, sign_and_broadcast_cosmos_tx,
};
use crate::ui;

struct ItsHubRegistration {
    hub_address: String,
    governance_address: String,
    edge_contract: String,
    message_translator: String,
}

async fn read_registration(ctx: &DeployContext) -> Result<ItsHubRegistration> {
    let content = tokio::fs::read_to_string(&ctx.target_json).await?;
    let root: Value = serde_json::from_str(&content)?;
    let edge_contract = root
        .pointer(&format!(
            "/chains/{}/contracts/InterchainTokenService/address",
            ctx.axelar_id
        ))
        .and_then(|value| value.as_str())
        .ok_or_else(|| {
            eyre::eyre!(
                "no InterchainTokenService address for {} — run DeployInterchainTokenService first",
                ctx.axelar_id
            )
        })?
        .to_string();
    let message_translator = root
        .pointer("/axelar/contracts/ItsAbiTranslator/address")
        .and_then(|value| value.as_str())
        .ok_or_else(|| eyre::eyre!("no axelar.contracts.ItsAbiTranslator.address in target JSON"))?
        .to_string();
    Ok(ItsHubRegistration {
        hub_address: read_axelar_contract_field(
            &ctx.target_json,
            "/axelar/contracts/InterchainTokenService/address",
        )
        .await?,
        governance_address: read_axelar_contract_field(
            &ctx.target_json,
            "/axelar/governanceAddress",
        )
        .await?,
        edge_contract,
        message_translator,
    })
}

pub(super) async fn run_register_its_on_hub(
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
    ui::info(&format!("registering {chain_axelar_id} on ITS Hub..."));

    let registration = read_registration(ctx).await?;

    let execute_msg = json!({
        "register_chains": {
            "chains": [{
                "chain": chain_axelar_id,
                "its_edge_contract": registration.edge_contract,
                "msg_translator": registration.message_translator,
                "truncation": {
                    "max_uint_bits": 256,
                    "max_decimals_when_truncating": 255
                }
            }]
        }
    });

    let json_str = serde_json::to_string_pretty(&execute_msg)?;
    ui::info(&format!(
        "execute msg: {}",
        ui::truncated_json(&json_str, 3)
    ));

    let sender = if use_governance {
        &registration.governance_address
    } else {
        axelar_address
    };
    let inner_msg = build_execute_msg_any(sender, &registration.hub_address, &execute_msg)?;

    let messages = if use_governance {
        let deposit_amount = read_axelar_contract_field(
            &ctx.target_json,
            "/axelar/govProposalExpeditedDepositAmount",
        )
        .await
        .unwrap_or_else(|_| DEFAULT_PROPOSAL_DEPOSIT_UAXL.to_string());
        let title = format!("Register {chain_axelar_id} on ITS Hub");
        let summary = format!(
            "Register {chain_axelar_id} ITS edge contract ({}) on InterchainTokenService Hub",
            registration.edge_contract
        );
        vec![build_submit_proposal_any(
            axelar_address,
            vec![inner_msg],
            &title,
            &summary,
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
