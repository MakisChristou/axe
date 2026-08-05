use alloy::{
    hex,
    network::TransactionBuilder,
    primitives::{Bytes, U256, keccak256},
    providers::{Provider, ProviderBuilder},
    rpc::types::TransactionRequest,
    signers::local::PrivateKeySigner,
    sol_types::SolValue,
};
use eyre::Result;
use serde_json::{Value, json};

use crate::commands::deploy::DeployContext;
use crate::cosmos::fetch_verifier_set;
use crate::evm::{decode_evm_error, encode_gateway_setup_params, read_artifact_bytecode};
use crate::state::{Step, save_state};
use crate::ui;
use crate::utils::{compute_domain_separator, update_target_json};

struct GatewayDeploymentRecord {
    proxy: alloy::primitives::Address,
    implementation: alloy::primitives::Address,
    deployer: alloy::primitives::Address,
    implementation_codehash: alloy::primitives::B256,
    domain_separator: alloy::primitives::B256,
    verifier_set_id: String,
}

async fn write_gateway_config(ctx: &DeployContext, record: &GatewayDeploymentRecord) -> Result<()> {
    let mut data = serde_json::Map::new();
    data.insert("address".into(), json!(format!("{}", record.proxy)));
    data.insert(
        "implementation".into(),
        json!(format!("{}", record.implementation)),
    );
    data.insert("deployer".into(), json!(format!("{}", record.deployer)));
    data.insert("deploymentMethod".into(), json!("create"));
    data.insert(
        "implementationCodehash".into(),
        json!(format!("{}", record.implementation_codehash)),
    );
    data.insert("previousSignersRetention".into(), json!(15));
    data.insert(
        "domainSeparator".into(),
        json!(format!("{}", record.domain_separator)),
    );
    data.insert("minimumRotationDelay".into(), json!(3600));
    data.insert("operator".into(), json!(format!("{}", record.deployer)));
    data.insert("owner".into(), json!(format!("{}", record.deployer)));
    data.insert("connectionType".into(), json!("amplifier"));
    data.insert("initialVerifierSetId".into(), json!(record.verifier_set_id));
    update_target_json(
        &ctx.target_json,
        &ctx.axelar_id,
        "AxelarGateway",
        Value::Object(data),
    )
    .await
}

async fn deploy_gateway_proxy<P: Provider>(
    provider: &P,
    implementation: alloy::primitives::Address,
    owner: alloy::primitives::Address,
    setup_params: &Bytes,
    proxy_artifact: &str,
) -> Result<alloy::primitives::Address> {
    ui::info("deploying AxelarAmplifierGatewayProxy...");
    let mut deploy_code = read_artifact_bytecode(proxy_artifact).await?;
    deploy_code
        .extend_from_slice(&(implementation, owner, setup_params.clone()).abi_encode_params());
    let tx = TransactionRequest::default()
        .with_deploy_code(Bytes::from(deploy_code))
        .with_gas_limit(5_000_000);
    match provider.call(tx.clone()).await {
        Ok(_) => ui::success("eth_call simulation passed"),
        Err(error) => {
            ui::warn(&format!(
                "eth_call simulation failed: {}",
                decode_evm_error(&error)
            ));
            ui::warn("proceeding with send_transaction anyway...");
        }
    }
    let receipt = match provider.send_transaction(tx).await {
        Ok(pending) => pending.get_receipt().await?,
        Err(error) => {
            return Err(eyre::eyre!(
                "proxy deployment failed: {}",
                decode_evm_error(&error)
            ));
        }
    };
    ui::tx_hash("proxy tx hash", &format!("{}", receipt.transaction_hash));
    if !receipt.status() {
        return Err(eyre::eyre!(
            "proxy deployment tx {} reverted on-chain (status=0)",
            receipt.transaction_hash
        ));
    }
    let address = receipt
        .contract_address
        .ok_or_else(|| eyre::eyre!("no contract address in proxy receipt"))?;
    ui::address("proxy deployed at", &format!("{address}"));
    Ok(address)
}

pub async fn run(
    ctx: &mut DeployContext,
    step_idx: usize,
    step: &Step,
    private_key: &str,
    impl_artifact: &str,
    proxy_artifact: &str,
) -> Result<()> {
    let signer: PrivateKeySigner = private_key.parse()?;
    let deployer_addr = signer.address();
    let provider = ProviderBuilder::new()
        .wallet(signer)
        .connect_http(ctx.rpc_url.parse()?);

    let domain_separator = compute_domain_separator(&ctx.target_json, &ctx.axelar_id).await?;

    // How many past verifier sets the gateway accepts proofs from after a
    // rotation. 15 means a rotation is reversible for 15 cycles before the
    // old set goes cold.
    const PREVIOUS_SIGNERS_RETENTION: u64 = 15;
    // Minimum seconds between rotations. 1h matches Axelar's published
    // gateway deployment defaults.
    const MIN_ROTATION_DELAY_SECS: u64 = 3600;

    let previous_signers_retention = U256::from(PREVIOUS_SIGNERS_RETENTION);
    let minimum_rotation_delay = U256::from(MIN_ROTATION_DELAY_SECS);

    // --- Tx 1: Deploy implementation (skip if already deployed) ---
    let (impl_addr, impl_codehash) = if let Some(addr) = step.implementation_address() {
        let code = provider.get_code_at(addr).await?;
        if code.is_empty() {
            return Err(eyre::eyre!(
                "saved implementation {addr} has no code on-chain"
            ));
        }
        ui::info(&format!(
            "reusing previously deployed implementation: {addr}"
        ));
        (addr, keccak256(&code))
    } else {
        ui::info("deploying AxelarAmplifierGateway implementation...");
        let impl_bytecode = read_artifact_bytecode(impl_artifact).await?;
        let mut impl_deploy_code = impl_bytecode.clone();
        impl_deploy_code.extend_from_slice(
            &(
                previous_signers_retention,
                domain_separator,
                minimum_rotation_delay,
            )
                .abi_encode(),
        );

        let tx = TransactionRequest::default().with_deploy_code(Bytes::from(impl_deploy_code));
        let receipt = provider.send_transaction(tx).await?.get_receipt().await?;
        ui::tx_hash(
            "implementation tx hash",
            &format!("{}", receipt.transaction_hash),
        );
        let addr = receipt
            .contract_address
            .ok_or_else(|| eyre::eyre!("no contract address in implementation receipt"))?;
        ui::address("implementation deployed at", &format!("{addr}"));

        let code = provider.get_code_at(addr).await?;
        let codehash = keccak256(&code);

        // Save implementation address to step so retries skip re-deployment
        ctx.state.steps[step_idx].set_implementation_address(addr)?;
        save_state(&ctx.state).await?;

        (addr, codehash)
    };

    // --- Fetch verifier set from Axelar chain ---
    let chain_axelar_id = {
        let content = tokio::fs::read_to_string(&ctx.target_json).await?;
        let root: Value = serde_json::from_str(&content)?;
        root.pointer(&format!("/chains/{}/axelarId", ctx.axelar_id))
            .and_then(|v| v.as_str())
            .unwrap_or(&ctx.axelar_id)
            .to_string()
    };
    let (signers, threshold, nonce, verifier_set_id) =
        fetch_verifier_set(&ctx.target_json, &chain_axelar_id).await?;

    // --- Encode setup params ---
    let operator = deployer_addr;
    let owner = deployer_addr;
    let setup_params = encode_gateway_setup_params(operator, &signers, threshold, nonce);
    ui::kv(
        "setup params",
        &format!(
            "{} bytes: 0x{}",
            setup_params.len(),
            hex::encode(&setup_params)
        ),
    );

    let proxy_addr =
        deploy_gateway_proxy(&provider, impl_addr, owner, &setup_params, proxy_artifact).await?;

    write_gateway_config(
        ctx,
        &GatewayDeploymentRecord {
            proxy: proxy_addr,
            implementation: impl_addr,
            deployer: deployer_addr,
            implementation_codehash: impl_codehash,
            domain_separator,
            verifier_set_id,
        },
    )
    .await
}
