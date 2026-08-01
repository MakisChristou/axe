use alloy::{providers::ProviderBuilder, signers::local::PrivateKeySigner};
use eyre::Result;
use serde_json::{Map, json};

use crate::commands::deploy::DeployContext;
use crate::evm::Ownable;
use crate::state::{Step, StepKind};
use crate::ui;
use crate::utils::{patch_target_json, read_contract_address_by_name};

pub async fn run(ctx: &DeployContext, step: &Step, private_key: &str) -> Result<()> {
    let signer: PrivateKeySigner = private_key.parse()?;
    let provider = ProviderBuilder::new()
        .wallet(signer)
        .connect_http(ctx.rpc_url.parse()?);

    let (contract_name, new_owner) = match &step.kind {
        StepKind::TransferOwnership {
            contract,
            new_owner,
        } => (contract.as_str(), *new_owner),
        other => {
            return Err(eyre::eyre!(
                "transfer_ownership::run called on wrong kind: {other:?}"
            ));
        }
    };

    let contract_addr =
        read_contract_address_by_name(&ctx.target_json, &ctx.axelar_id, contract_name).await?;
    let ownable = Ownable::new(contract_addr, &provider);

    ui::info(&format!(
        "transferring {contract_name} ownership to {new_owner}"
    ));
    let tx_hash = ownable
        .transferOwnership(new_owner)
        .send()
        .await?
        .watch()
        .await?;
    ui::tx_hash("tx hash", &format!("{tx_hash}"));

    let current_owner = ownable.owner().call().await?;
    ui::address("verified owner", &format!("{current_owner}"));

    let mut patches = Map::new();
    patches.insert("owner".into(), json!(format!("{new_owner}")));
    patch_target_json(&ctx.target_json, &ctx.axelar_id, contract_name, &patches).await?;

    Ok(())
}
