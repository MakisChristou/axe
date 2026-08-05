use eyre::Result;
use serde_json::{Value, json};

use crate::commands::deploy::DeployContext;
use crate::cosmos::{lcd_cosmwasm_smart_query, read_axelar_config, read_axelar_contract_field};
use crate::ui;

pub async fn run(ctx: &DeployContext) -> Result<()> {
    let (lcd, _, _, _) = read_axelar_config(&ctx.target_json).await?;
    let coordinator_addr =
        read_axelar_contract_field(&ctx.target_json, "/axelar/contracts/Coordinator/address")
            .await?;

    let chain_axelar_id = {
        let content = tokio::fs::read_to_string(&ctx.target_json).await?;
        let root: Value = serde_json::from_str(&content)?;
        root.pointer(&format!("/chains/{}/axelarId", ctx.axelar_id))
            .and_then(|v| v.as_str())
            .unwrap_or(&ctx.axelar_id)
            .to_string()
    };

    let deployment_name = read_axelar_contract_field(
        &ctx.target_json,
        &format!("/axelar/contracts/Coordinator/deployments/{chain_axelar_id}/deploymentName"),
    )
    .await?;

    ui::info(&format!(
        "querying deployed contracts for {chain_axelar_id} (deployment: {deployment_name})..."
    ));

    let query = json!({
        "deployment": {
            "deployment_name": deployment_name
        }
    });
    let result = lcd_cosmwasm_smart_query(&lcd, &coordinator_addr, &query).await?;

    let verifier_address = result["verifier_address"]
        .as_str()
        .ok_or_else(|| eyre::eyre!("no verifier_address in response"))?;
    let prover_address = result["prover_address"]
        .as_str()
        .ok_or_else(|| eyre::eyre!("no prover_address in response"))?;
    let gateway_address = result["gateway_address"]
        .as_str()
        .ok_or_else(|| eyre::eyre!("no gateway_address in response"))?;

    ui::address("VotingVerifier", verifier_address);
    ui::address("MultisigProver", prover_address);
    ui::address("Gateway", gateway_address);

    let content = tokio::fs::read_to_string(&ctx.target_json).await?;
    let mut root: Value = serde_json::from_str(&content)?;

    if let Some(vv) = root.pointer_mut(&format!(
        "/axelar/contracts/VotingVerifier/{chain_axelar_id}"
    )) {
        vv["address"] = json!(verifier_address);
    }
    if let Some(mp) = root.pointer_mut(&format!(
        "/axelar/contracts/MultisigProver/{chain_axelar_id}"
    )) {
        mp["address"] = json!(prover_address);
    }
    if let Some(gw) = root.pointer_mut(&format!("/axelar/contracts/Gateway/{chain_axelar_id}")) {
        gw["address"] = json!(gateway_address);
    }

    tokio::fs::write(
        &ctx.target_json,
        serde_json::to_string_pretty(&root)? + "\n",
    )
    .await?;
    ui::success(&format!("updated {}", ctx.target_json.display()));

    Ok(())
}
