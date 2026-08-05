use eyre::Result;
use serde_json::Value;

use crate::cli::resolve_axelar_id;
use crate::state::{read_state, state_path};
use crate::ui;

pub async fn run(axelar_id: Option<String>) -> Result<()> {
    let axelar_id = resolve_axelar_id(axelar_id)?;
    let state = read_state(&axelar_id).await?;
    let target_json = state.target_json;

    let sf = state_path(&axelar_id)?;
    tokio::fs::remove_file(&sf).await?;
    ui::success(&format!("deleted {}", sf.display()));

    if !tokio::fs::try_exists(&target_json).await? {
        ui::warn(&format!(
            "target json {} does not exist, skipping",
            target_json.display()
        ));
        return Ok(());
    }

    let content = tokio::fs::read_to_string(&target_json).await?;
    let mut root: Value = serde_json::from_str(&content)?;

    if let Some(chains) = root.get_mut("chains").and_then(|v| v.as_object_mut())
        && chains.remove(&axelar_id).is_some()
    {
        ui::info(&format!("removed chains.{axelar_id}"));
    }

    if let Some(vv) = root
        .pointer_mut("/axelar/contracts/VotingVerifier")
        .and_then(|v| v.as_object_mut())
        && vv.remove(&axelar_id).is_some()
    {
        ui::info(&format!("removed VotingVerifier.{axelar_id}"));
    }

    if let Some(mp) = root
        .pointer_mut("/axelar/contracts/MultisigProver")
        .and_then(|v| v.as_object_mut())
        && mp.remove(&axelar_id).is_some()
    {
        ui::info(&format!("removed MultisigProver.{axelar_id}"));
    }

    if let Some(gw) = root
        .pointer_mut("/axelar/contracts/Gateway")
        .and_then(|v| v.as_object_mut())
        && gw.remove(&axelar_id).is_some()
    {
        ui::info(&format!("removed Gateway.{axelar_id}"));
    }

    if let Some(deployments) = root
        .pointer_mut("/axelar/contracts/Coordinator/deployments")
        .and_then(|v| v.as_object_mut())
        && deployments.remove(&axelar_id).is_some()
    {
        ui::info(&format!("removed Coordinator.deployments.{axelar_id}"));
    }

    tokio::fs::write(&target_json, serde_json::to_string_pretty(&root)? + "\n").await?;
    ui::success(&format!("cleaned up {}", target_json.display()));

    Ok(())
}
