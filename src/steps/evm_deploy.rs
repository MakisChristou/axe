use alloy::{
    network::TransactionBuilder,
    primitives::{Bytes, keccak256},
    providers::{Provider, ProviderBuilder},
    rpc::types::TransactionRequest,
    signers::local::PrivateKeySigner,
    sol_types::SolValue,
};
use eyre::Result;
use serde_json::{Value, json};

use crate::commands::deploy::DeployContext;
use crate::config::ChainContract;
use crate::evm::{ConstAddressDeployer, get_salt_from_key, read_artifact_bytecode};
use crate::ui;
use crate::utils::{read_contract_address, update_target_json};

pub async fn run(
    ctx: &mut DeployContext,
    step_name: &str,
    step_kind: &str,
    private_key: &str,
    artifact_path: &str,
    salt: &Option<String>,
) -> Result<()> {
    let bytecode_raw = read_artifact_bytecode(artifact_path).await?;

    let signer: PrivateKeySigner = private_key.parse()?;
    let deployer_addr = signer.address();
    let provider = ProviderBuilder::new()
        .wallet(signer)
        .connect_http(ctx.rpc_url.parse()?);

    let (addr, deploy_method, salt_used) = if step_kind == "deploy-create" {
        let tx = TransactionRequest::default().with_deploy_code(Bytes::from(bytecode_raw.clone()));
        let receipt = provider.send_transaction(tx).await?.get_receipt().await?;
        ui::tx_hash("tx hash", &format!("{}", receipt.transaction_hash));
        let addr = receipt
            .contract_address
            .ok_or_else(|| eyre::eyre!("no contract address in receipt"))?;
        (addr, "create", None)
    } else {
        // deploy-create2
        let const_deployer_addr = read_contract_address(
            &ctx.target_json,
            &ctx.axelar_id,
            ChainContract::ConstAddressDeployer,
        )
        .await?;
        let salt_string = salt.clone().unwrap_or_else(|| ctx.state.cosm_salt.clone());
        let salt_bytes = get_salt_from_key(&salt_string);

        // For contracts with constructor args (e.g. Operators(address owner)),
        // append ABI-encoded args to bytecode
        let deploy_bytecode = match step_name {
            "Operators" => {
                let mut b = bytecode_raw.clone();
                b.extend_from_slice(&deployer_addr.abi_encode());
                b
            }
            _ => bytecode_raw.clone(),
        };

        let const_deployer = ConstAddressDeployer::new(const_deployer_addr, &provider);

        let deploy_bytes = Bytes::from(deploy_bytecode.clone());

        let addr = const_deployer
            .deployedAddress(deploy_bytes.clone(), deployer_addr, salt_bytes)
            .call()
            .await?;
        ui::address("predicted address", &format!("{addr}"));

        let pending = const_deployer
            .deploy_call(deploy_bytes, salt_bytes)
            .send()
            .await?;
        let tx_hash = *pending.tx_hash();
        ui::tx_hash("tx submitted", &format!("{tx_hash}"));
        ui::info("waiting for confirmation...");
        pending.get_receipt().await?;

        (addr, "create2", Some(salt_string))
    };

    ui::address("deployed at", &format!("{addr}"));

    let predeploy_codehash = keccak256(&bytecode_raw);
    let deployed_code = provider.get_code_at(addr).await?;
    let codehash = keccak256(&deployed_code);

    let mut contract_data = serde_json::Map::new();
    contract_data.insert("address".into(), json!(format!("{addr}")));
    contract_data.insert("deployer".into(), json!(format!("{deployer_addr}")));
    contract_data.insert("deploymentMethod".into(), json!(deploy_method));
    contract_data.insert("codehash".into(), json!(format!("{codehash}")));
    contract_data.insert(
        "predeployCodehash".into(),
        json!(format!("{predeploy_codehash}")),
    );
    if let Some(ref s) = salt_used {
        contract_data.insert("salt".into(), json!(s));
    }

    update_target_json(
        &ctx.target_json,
        &ctx.axelar_id,
        step_name,
        Value::Object(contract_data),
    )
    .await?;

    Ok(())
}
