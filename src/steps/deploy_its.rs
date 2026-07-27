use std::fs;

use alloy::{
    primitives::{Address, Bytes, FixedBytes, keccak256},
    providers::{Provider, ProviderBuilder},
    signers::local::PrivateKeySigner,
    sol_types::SolValue,
};
use eyre::Result;
use serde_json::{Value, json};

use crate::commands::deploy::DeployContext;
use crate::evm::{
    ConstAddressDeployer, Create3Deployer, broadcast_and_log, get_salt_from_key,
    read_artifact_bytecode,
};
use crate::state::{Step, save_state};
use crate::ui;
use crate::utils::{deployments_root, read_contract_address, update_target_json};

/// Deploy all ITS contracts (9 total) in a single step.
pub async fn run(
    ctx: &mut DeployContext,
    step_idx: usize,
    step: &Step,
    private_key: &str,
) -> Result<()> {
    let signer: PrivateKeySigner = private_key.parse()?;
    let deployer_addr = signer.address();
    let provider = ProviderBuilder::new()
        .wallet(signer)
        .connect_http(ctx.rpc_url.parse()?);

    // --- Detect deployer change and clear stale helper addresses ---
    let saved_deployer = step.its_address("itsDeployer");
    if let Some(prev) = saved_deployer {
        if prev != deployer_addr {
            ui::warn(&format!(
                "ITS deployer changed from {prev} to {deployer_addr}"
            ));
            ui::info("clearing stale helper addresses from step state...");
            ctx.state.steps[step_idx].clear_its_helper_addresses();
            ctx.state.steps[step_idx].set_its_address("itsDeployer", deployer_addr);
            save_state(&ctx.state)?;
        }
    } else {
        // First run — save deployer address
        ctx.state.steps[step_idx].set_its_address("itsDeployer", deployer_addr);
        save_state(&ctx.state)?;
    }
    // Re-read step after potential mutation
    let step = ctx.state.steps[step_idx].clone();

    // --- Read prerequisites ---
    let const_deployer_addr =
        read_contract_address(&ctx.target_json, &ctx.axelar_id, "ConstAddressDeployer")?;
    let create3_deployer_addr =
        read_contract_address(&ctx.target_json, &ctx.axelar_id, "Create3Deployer")?;
    let gateway_addr = read_contract_address(&ctx.target_json, &ctx.axelar_id, "AxelarGateway")?;
    let gas_service_addr =
        read_contract_address(&ctx.target_json, &ctx.axelar_id, "AxelarGasService")?;

    let content = fs::read_to_string(&ctx.target_json)?;
    let root: Value = serde_json::from_str(&content)?;
    let chain_axelar_id = root
        .pointer(&format!("/chains/{}/axelarId", ctx.axelar_id))
        .and_then(|v| v.as_str())
        .unwrap_or(&ctx.axelar_id)
        .to_string();

    let its_hub_address = root
        .pointer("/axelar/contracts/InterchainTokenService/address")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            eyre::eyre!("no axelar.contracts.InterchainTokenService.address in target JSON")
        })?
        .to_string();

    // --- Compute salts ---
    let its_salt =
        ctx.state.its_salt.clone().ok_or_else(|| {
            eyre::eyre!("no itsSalt in state. Set ITS_SALT in .env and re-run init")
        })?;
    let its_proxy_salt = ctx.state.its_proxy_salt.clone().ok_or_else(|| {
        eyre::eyre!("no itsProxySalt in state. Set ITS_PROXY_SALT in .env and re-run init")
    })?;

    let helper_salt = get_salt_from_key(&format!("ITS {its_salt}"));
    let impl_salt = get_salt_from_key(&format!("ITS {its_salt} Implementation"));
    let proxy_salt = get_salt_from_key(&format!("ITS {its_proxy_salt}"));
    let factory_salt = get_salt_from_key(&format!("ITS Factory {its_proxy_salt}"));

    ui::kv(
        "ITS salt",
        &format!("'ITS {its_salt}', proxy salt: 'ITS {its_proxy_salt}'"),
    );

    // --- Resolve artifact paths ---
    let repo_root = deployments_root(&ctx.target_json)?;
    let its_base =
        repo_root.join("node_modules/@axelar-network/interchain-token-service/artifacts/contracts");
    let artifact = |rel: &str| its_base.join(rel).to_string_lossy().into_owned();

    // --- Predict proxy addresses via CREATE3 (before deploying anything) ---
    let create3 = Create3Deployer::new(create3_deployer_addr, &provider);
    let its_proxy_addr = create3
        .deployedAddress(Bytes::new(), deployer_addr, proxy_salt)
        .call()
        .await?;
    let factory_proxy_addr = create3
        .deployedAddress(Bytes::new(), deployer_addr, factory_salt)
        .call()
        .await?;
    ui::address("predicted ITS proxy", &format!("{its_proxy_addr}"));
    ui::address("predicted Factory proxy", &format!("{factory_proxy_addr}"));

    let const_deployer = ConstAddressDeployer::new(const_deployer_addr, &provider);

    // ========= 1. Deploy helper contracts via CREATE2 =========

    ui::section("deploying ITS helper contracts");

    let tmdeployer_bytecode = read_artifact_bytecode(&artifact(
        "utils/TokenManagerDeployer.sol/TokenManagerDeployer.json",
    ))?;
    let token_manager_deployer_addr = deploy_via_create2(
        &const_deployer,
        &provider,
        Create2Deployment {
            deployer: deployer_addr,
            name: "TokenManagerDeployer",
            bytecode: tmdeployer_bytecode,
            constructor_args: None,
            salt: helper_salt,
            step: &step,
        },
    )
    .await?;
    save_its_address(
        ctx,
        step_idx,
        "TokenManagerDeployer",
        token_manager_deployer_addr,
    )?;

    let it_bytecode = read_artifact_bytecode(&artifact(
        "interchain-token/InterchainToken.sol/InterchainToken.json",
    ))?;
    let interchain_token_addr = deploy_via_create2(
        &const_deployer,
        &provider,
        Create2Deployment {
            deployer: deployer_addr,
            name: "InterchainToken",
            bytecode: it_bytecode,
            constructor_args: Some(its_proxy_addr.abi_encode()),
            salt: helper_salt,
            step: &step,
        },
    )
    .await?;
    save_its_address(ctx, step_idx, "InterchainToken", interchain_token_addr)?;

    let itd_bytecode = read_artifact_bytecode(&artifact(
        "utils/InterchainTokenDeployer.sol/InterchainTokenDeployer.json",
    ))?;
    let interchain_token_deployer_addr = deploy_via_create2(
        &const_deployer,
        &provider,
        Create2Deployment {
            deployer: deployer_addr,
            name: "InterchainTokenDeployer",
            bytecode: itd_bytecode,
            constructor_args: Some(interchain_token_addr.abi_encode()),
            salt: helper_salt,
            step: &step,
        },
    )
    .await?;
    save_its_address(
        ctx,
        step_idx,
        "InterchainTokenDeployer",
        interchain_token_deployer_addr,
    )?;

    let tm_bytecode = read_artifact_bytecode(&artifact(
        "token-manager/TokenManager.sol/TokenManager.json",
    ))?;
    let token_manager_addr = deploy_via_create2(
        &const_deployer,
        &provider,
        Create2Deployment {
            deployer: deployer_addr,
            name: "TokenManager",
            bytecode: tm_bytecode,
            constructor_args: Some(its_proxy_addr.abi_encode()),
            salt: helper_salt,
            step: &step,
        },
    )
    .await?;
    save_its_address(ctx, step_idx, "TokenManager", token_manager_addr)?;

    let th_bytecode = read_artifact_bytecode(&artifact("TokenHandler.sol/TokenHandler.json"))?;
    let token_handler_addr = deploy_via_create2(
        &const_deployer,
        &provider,
        Create2Deployment {
            deployer: deployer_addr,
            name: "TokenHandler",
            bytecode: th_bytecode,
            constructor_args: None,
            salt: helper_salt,
            step: &step,
        },
    )
    .await?;
    save_its_address(ctx, step_idx, "TokenHandler", token_handler_addr)?;

    // ========= 2. Deploy ITS Implementation via CREATE2 =========

    ui::section("deploying InterchainTokenService implementation");

    let its_impl_bytecode = read_artifact_bytecode(&artifact(
        "InterchainTokenService.sol/InterchainTokenService.json",
    ))?;
    let its_impl_constructor_args = (
        token_manager_deployer_addr,
        interchain_token_deployer_addr,
        gateway_addr,
        gas_service_addr,
        factory_proxy_addr,
        chain_axelar_id.clone(),
        its_hub_address,
        token_manager_addr,
        token_handler_addr,
    )
        .abi_encode_params();

    let its_impl_addr = deploy_via_create2(
        &const_deployer,
        &provider,
        Create2Deployment {
            deployer: deployer_addr,
            name: "InterchainTokenServiceImpl",
            bytecode: its_impl_bytecode,
            constructor_args: Some(its_impl_constructor_args),
            salt: impl_salt,
            step: &step,
        },
    )
    .await?;
    save_its_address(ctx, step_idx, "InterchainTokenServiceImpl", its_impl_addr)?;

    // ========= 3. Deploy ITS Proxy via CREATE3 =========

    ui::section("deploying InterchainTokenService proxy");

    let proxy_bytecode = read_artifact_bytecode(&artifact(
        "proxies/InterchainProxy.sol/InterchainProxy.json",
    ))?;

    // setupParams = abi.encode(operator, chainAxelarId, trustedChains[])
    let setup_params: Bytes = Bytes::from(
        (deployer_addr, chain_axelar_id.clone(), Vec::<String>::new()).abi_encode_params(),
    );
    let its_proxy_constructor_args =
        (its_impl_addr, deployer_addr, setup_params).abi_encode_params();

    let its_proxy_deployed = deploy_via_create3(
        &create3,
        &provider,
        "InterchainTokenServiceProxy",
        proxy_bytecode.clone(),
        its_proxy_constructor_args,
        proxy_salt,
        its_proxy_addr,
    )
    .await?;
    assert_eq!(its_proxy_deployed, its_proxy_addr);

    // ========= 4. Deploy Factory Implementation via CREATE2 =========

    ui::section("deploying InterchainTokenFactory implementation");

    let factory_impl_bytecode = read_artifact_bytecode(&artifact(
        "InterchainTokenFactory.sol/InterchainTokenFactory.json",
    ))?;
    let factory_impl_addr = deploy_via_create2(
        &const_deployer,
        &provider,
        Create2Deployment {
            deployer: deployer_addr,
            name: "InterchainTokenFactoryImpl",
            bytecode: factory_impl_bytecode,
            constructor_args: Some(its_proxy_addr.abi_encode()),
            salt: impl_salt,
            step: &step,
        },
    )
    .await?;
    save_its_address(
        ctx,
        step_idx,
        "InterchainTokenFactoryImpl",
        factory_impl_addr,
    )?;

    // ========= 5. Deploy Factory Proxy via CREATE3 =========

    ui::section("deploying InterchainTokenFactory proxy");

    let factory_proxy_constructor_args =
        (factory_impl_addr, deployer_addr, Bytes::new()).abi_encode_params();

    let factory_proxy_deployed = deploy_via_create3(
        &create3,
        &provider,
        "InterchainTokenFactoryProxy",
        proxy_bytecode,
        factory_proxy_constructor_args,
        factory_salt,
        factory_proxy_addr,
    )
    .await?;
    assert_eq!(factory_proxy_deployed, factory_proxy_addr);

    // ========= 6. Write to target JSON =========

    ui::section("saving ITS contract data to target JSON");

    let its_bytecode_raw = read_artifact_bytecode(&artifact(
        "InterchainTokenService.sol/InterchainTokenService.json",
    ))?;
    let predeploy_codehash = keccak256(&its_bytecode_raw);

    let its_data = json!({
        "salt": format!("ITS {its_salt}"),
        "proxySalt": format!("ITS {its_proxy_salt}"),
        "deployer": format!("{deployer_addr}"),
        "tokenManagerDeployer": format!("{token_manager_deployer_addr}"),
        "interchainToken": format!("{interchain_token_addr}"),
        "interchainTokenDeployer": format!("{interchain_token_deployer_addr}"),
        "tokenManager": format!("{token_manager_addr}"),
        "tokenHandler": format!("{token_handler_addr}"),
        "implementation": format!("{its_impl_addr}"),
        "address": format!("{its_proxy_addr}"),
        "predeployCodehash": format!("{predeploy_codehash}"),
        "owner": format!("{deployer_addr}"),
    });

    update_target_json(
        &ctx.target_json,
        &ctx.axelar_id,
        "InterchainTokenService",
        its_data,
    )?;

    let factory_data = json!({
        "salt": format!("ITS Factory {its_proxy_salt}"),
        "deployer": format!("{deployer_addr}"),
        "implementation": format!("{factory_impl_addr}"),
        "address": format!("{factory_proxy_addr}"),
    });

    update_target_json(
        &ctx.target_json,
        &ctx.axelar_id,
        "InterchainTokenFactory",
        factory_data,
    )?;

    ui::success("ITS deployment complete!");
    ui::address("InterchainTokenService", &format!("{its_proxy_addr}"));
    ui::address("InterchainTokenFactory", &format!("{factory_proxy_addr}"));

    Ok(())
}

/// Deploy a contract via CREATE2 using ConstAddressDeployer.
/// Checks step state and on-chain code to skip already-deployed contracts.
struct Create2Deployment<'a> {
    deployer: Address,
    name: &'a str,
    bytecode: Vec<u8>,
    constructor_args: Option<Vec<u8>>,
    salt: FixedBytes<32>,
    step: &'a Step,
}

async fn deploy_via_create2<P: Provider>(
    const_deployer: &ConstAddressDeployer::ConstAddressDeployerInstance<P>,
    provider: P,
    deployment: Create2Deployment<'_>,
) -> Result<Address> {
    let Create2Deployment {
        deployer: deployer_addr,
        name,
        bytecode,
        constructor_args,
        salt,
        step,
    } = deployment;
    let mut deploy_code = bytecode;
    if let Some(args) = constructor_args {
        deploy_code.extend_from_slice(&args);
    }
    let deploy_bytes = Bytes::from(deploy_code);

    // Compute the correct predicted address for the current deployer + bytecode + salt
    let predicted = const_deployer
        .deployedAddress(deploy_bytes.clone(), deployer_addr, salt)
        .call()
        .await?;

    // Check step state — only trust it if the saved address matches the predicted one
    if let Some(saved) = step.its_address(name)
        && saved != predicted
    {
        ui::warn(&format!(
            "{name}: stale address {saved} in step state (predicted {predicted}), ignoring"
        ));
    }

    // Check if already deployed at the correct predicted address
    let existing_code = provider.get_code_at(predicted).await?;
    if !existing_code.is_empty() {
        ui::info(&format!("{name}: already deployed at {predicted}"));
        return Ok(predicted);
    }

    ui::info(&format!("{name}: deploying via CREATE2..."));
    let pending = const_deployer
        .deploy_call(deploy_bytes, salt)
        .send()
        .await
        .map_err(|e| {
            let msg = e.to_string();
            if msg.contains("0x4102e83a") {
                eyre::eyre!("{name}: ConstAddressDeployer.FailedDeploy() — constructor reverted. \
                    This usually means the constructor args are invalid or stale (e.g. deployer key changed). \
                    Try resetting the ITS step state.")
            } else {
                eyre::eyre!("{name}: send failed: {e}")
            }
        })?;
    broadcast_and_log(pending, &format!("{name}: tx")).await?;
    ui::kv(&format!("{name} deployed at"), &format!("{predicted}"));
    Ok(predicted)
}

/// Deploy a contract via CREATE3 using Create3Deployer.
/// Checks on-chain code at the predicted address to skip if already deployed.
async fn deploy_via_create3<P: Provider>(
    create3: &Create3Deployer::Create3DeployerInstance<P>,
    provider: P,
    name: &str,
    proxy_bytecode: Vec<u8>,
    constructor_args: Vec<u8>,
    salt: FixedBytes<32>,
    predicted: Address,
) -> Result<Address> {
    let existing_code = provider.get_code_at(predicted).await?;
    if !existing_code.is_empty() {
        ui::info(&format!("{name}: already deployed at {predicted}"));
        return Ok(predicted);
    }

    let mut deploy_code = proxy_bytecode;
    deploy_code.extend_from_slice(&constructor_args);

    ui::info(&format!("{name}: deploying via CREATE3..."));
    let pending = create3
        .deploy_call(Bytes::from(deploy_code), salt)
        .send()
        .await
        .map_err(|e| {
            let msg = e.to_string();
            if msg.contains("0x4102e83a") {
                eyre::eyre!("{name}: FailedDeploy() — constructor reverted")
            } else {
                eyre::eyre!("{name}: send failed: {e}")
            }
        })?;
    broadcast_and_log(pending, &format!("{name}: tx")).await?;
    ui::kv(&format!("{name} deployed at"), &format!("{predicted}"));
    Ok(predicted)
}

/// Save an intermediate address to the step state for idempotent retries.
fn save_its_address(
    ctx: &mut DeployContext,
    step_idx: usize,
    name: &str,
    addr: Address,
) -> Result<()> {
    ctx.state.steps[step_idx].set_its_address(name, addr);
    save_state(&ctx.state)?;
    Ok(())
}
