use std::fs;
use std::path::PathBuf;

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

struct ItsDeploymentPlan {
    step: Step,
    deployer: Address,
    const_deployer: Address,
    create3_deployer: Address,
    gateway: Address,
    gas_service: Address,
    chain_axelar_id: String,
    hub_address: String,
    its_salt_key: String,
    proxy_salt_key: String,
    helper_salt: FixedBytes<32>,
    implementation_salt: FixedBytes<32>,
    proxy_salt: FixedBytes<32>,
    factory_salt: FixedBytes<32>,
    artifact_base: PathBuf,
    its_proxy: Address,
    factory_proxy: Address,
}

impl ItsDeploymentPlan {
    fn artifact(&self, relative: &str) -> String {
        self.artifact_base
            .join(relative)
            .to_string_lossy()
            .into_owned()
    }
}

struct ItsHelpers {
    token_manager_deployer: Address,
    interchain_token: Address,
    interchain_token_deployer: Address,
    token_manager: Address,
    token_handler: Address,
}

struct ItsServiceDeployment {
    implementation: Address,
    proxy: Address,
}

struct ItsFactoryDeployment {
    implementation: Address,
    proxy: Address,
}

fn record_its_deployer(
    ctx: &mut DeployContext,
    step_idx: usize,
    step: &Step,
    deployer: Address,
) -> Result<Step> {
    if let Some(previous) = step.its_address("itsDeployer") {
        if previous != deployer {
            ui::warn(&format!(
                "ITS deployer changed from {previous} to {deployer}"
            ));
            ui::info("clearing stale helper addresses from step state...");
            ctx.state.steps[step_idx].clear_its_helper_addresses()?;
            ctx.state.steps[step_idx].set_its_address("itsDeployer", deployer)?;
            save_state(&ctx.state)?;
        }
    } else {
        ctx.state.steps[step_idx].set_its_address("itsDeployer", deployer)?;
        save_state(&ctx.state)?;
    }
    Ok(ctx.state.steps[step_idx].clone())
}

async fn prepare_its_plan<P: Provider>(
    ctx: &mut DeployContext,
    step_idx: usize,
    step: &Step,
    deployer: Address,
    provider: &P,
) -> Result<ItsDeploymentPlan> {
    let step = record_its_deployer(ctx, step_idx, step, deployer)?;
    let const_deployer =
        read_contract_address(&ctx.target_json, &ctx.axelar_id, "ConstAddressDeployer")?;
    let create3_deployer =
        read_contract_address(&ctx.target_json, &ctx.axelar_id, "Create3Deployer")?;
    let gateway = read_contract_address(&ctx.target_json, &ctx.axelar_id, "AxelarGateway")?;
    let gas_service = read_contract_address(&ctx.target_json, &ctx.axelar_id, "AxelarGasService")?;
    let root: Value = serde_json::from_str(&fs::read_to_string(&ctx.target_json)?)?;
    let chain_axelar_id = root
        .pointer(&format!("/chains/{}/axelarId", ctx.axelar_id))
        .and_then(Value::as_str)
        .unwrap_or(&ctx.axelar_id)
        .to_string();
    let hub_address = root
        .pointer("/axelar/contracts/InterchainTokenService/address")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            eyre::eyre!("no axelar.contracts.InterchainTokenService.address in target JSON")
        })?
        .to_string();
    let its_salt_key =
        ctx.state.its_salt.clone().ok_or_else(|| {
            eyre::eyre!("no itsSalt in state. Set ITS_SALT in .env and re-run init")
        })?;
    let proxy_salt_key = ctx.state.its_proxy_salt.clone().ok_or_else(|| {
        eyre::eyre!("no itsProxySalt in state. Set ITS_PROXY_SALT in .env and re-run init")
    })?;
    let helper_salt = get_salt_from_key(&format!("ITS {its_salt_key}"));
    let implementation_salt = get_salt_from_key(&format!("ITS {its_salt_key} Implementation"));
    let proxy_salt = get_salt_from_key(&format!("ITS {proxy_salt_key}"));
    let factory_salt = get_salt_from_key(&format!("ITS Factory {proxy_salt_key}"));
    ui::kv(
        "ITS salt",
        &format!("'ITS {its_salt_key}', proxy salt: 'ITS {proxy_salt_key}'"),
    );
    let artifact_base = deployments_root(&ctx.target_json)?
        .join("node_modules/@axelar-network/interchain-token-service/artifacts/contracts");
    let create3 = Create3Deployer::new(create3_deployer, provider);
    let its_proxy = create3
        .deployedAddress(Bytes::new(), deployer, proxy_salt)
        .call()
        .await?;
    let factory_proxy = create3
        .deployedAddress(Bytes::new(), deployer, factory_salt)
        .call()
        .await?;
    ui::address("predicted ITS proxy", &format!("{its_proxy}"));
    ui::address("predicted Factory proxy", &format!("{factory_proxy}"));
    Ok(ItsDeploymentPlan {
        step,
        deployer,
        const_deployer,
        create3_deployer,
        gateway,
        gas_service,
        chain_axelar_id,
        hub_address,
        its_salt_key,
        proxy_salt_key,
        helper_salt,
        implementation_salt,
        proxy_salt,
        factory_salt,
        artifact_base,
        its_proxy,
        factory_proxy,
    })
}

struct NamedHelperDeployment<'a> {
    name: &'a str,
    artifact: &'a str,
    constructor_args: Option<Vec<u8>>,
}

async fn deploy_named_helper<P: Provider>(
    ctx: &mut DeployContext,
    step_idx: usize,
    plan: &ItsDeploymentPlan,
    provider: &P,
    deployment: NamedHelperDeployment<'_>,
) -> Result<Address> {
    let address = deploy_via_create2(
        &ConstAddressDeployer::new(plan.const_deployer, provider),
        provider,
        Create2Deployment {
            deployer: plan.deployer,
            name: deployment.name,
            bytecode: read_artifact_bytecode(&plan.artifact(deployment.artifact))?,
            constructor_args: deployment.constructor_args,
            salt: plan.helper_salt,
            step: &plan.step,
        },
    )
    .await?;
    save_its_address(ctx, step_idx, deployment.name, address)?;
    Ok(address)
}

async fn deploy_its_helpers<P: Provider>(
    ctx: &mut DeployContext,
    step_idx: usize,
    plan: &ItsDeploymentPlan,
    provider: &P,
) -> Result<ItsHelpers> {
    ui::section("deploying ITS helper contracts");
    let token_manager_deployer = deploy_named_helper(
        ctx,
        step_idx,
        plan,
        provider,
        NamedHelperDeployment {
            name: "TokenManagerDeployer",
            artifact: "utils/TokenManagerDeployer.sol/TokenManagerDeployer.json",
            constructor_args: None,
        },
    )
    .await?;
    let interchain_token = deploy_named_helper(
        ctx,
        step_idx,
        plan,
        provider,
        NamedHelperDeployment {
            name: "InterchainToken",
            artifact: "interchain-token/InterchainToken.sol/InterchainToken.json",
            constructor_args: Some(plan.its_proxy.abi_encode()),
        },
    )
    .await?;
    let interchain_token_deployer = deploy_named_helper(
        ctx,
        step_idx,
        plan,
        provider,
        NamedHelperDeployment {
            name: "InterchainTokenDeployer",
            artifact: "utils/InterchainTokenDeployer.sol/InterchainTokenDeployer.json",
            constructor_args: Some(interchain_token.abi_encode()),
        },
    )
    .await?;
    let token_manager = deploy_named_helper(
        ctx,
        step_idx,
        plan,
        provider,
        NamedHelperDeployment {
            name: "TokenManager",
            artifact: "token-manager/TokenManager.sol/TokenManager.json",
            constructor_args: Some(plan.its_proxy.abi_encode()),
        },
    )
    .await?;
    let token_handler = deploy_named_helper(
        ctx,
        step_idx,
        plan,
        provider,
        NamedHelperDeployment {
            name: "TokenHandler",
            artifact: "TokenHandler.sol/TokenHandler.json",
            constructor_args: None,
        },
    )
    .await?;
    Ok(ItsHelpers {
        token_manager_deployer,
        interchain_token,
        interchain_token_deployer,
        token_manager,
        token_handler,
    })
}

async fn deploy_its_service<P: Provider>(
    ctx: &mut DeployContext,
    step_idx: usize,
    plan: &ItsDeploymentPlan,
    helpers: &ItsHelpers,
    provider: &P,
) -> Result<ItsServiceDeployment> {
    ui::section("deploying InterchainTokenService implementation");
    let constructor_args = (
        helpers.token_manager_deployer,
        helpers.interchain_token_deployer,
        plan.gateway,
        plan.gas_service,
        plan.factory_proxy,
        plan.chain_axelar_id.clone(),
        plan.hub_address.clone(),
        helpers.token_manager,
        helpers.token_handler,
    )
        .abi_encode_params();
    let implementation = deploy_via_create2(
        &ConstAddressDeployer::new(plan.const_deployer, provider),
        provider,
        Create2Deployment {
            deployer: plan.deployer,
            name: "InterchainTokenServiceImpl",
            bytecode: read_artifact_bytecode(
                &plan.artifact("InterchainTokenService.sol/InterchainTokenService.json"),
            )?,
            constructor_args: Some(constructor_args),
            salt: plan.implementation_salt,
            step: &plan.step,
        },
    )
    .await?;
    save_its_address(ctx, step_idx, "InterchainTokenServiceImpl", implementation)?;

    ui::section("deploying InterchainTokenService proxy");
    let proxy_bytecode =
        read_artifact_bytecode(&plan.artifact("proxies/InterchainProxy.sol/InterchainProxy.json"))?;
    let setup_params: Bytes = Bytes::from(
        (
            plan.deployer,
            plan.chain_axelar_id.clone(),
            Vec::<String>::new(),
        )
            .abi_encode_params(),
    );
    let constructor_args = (implementation, plan.deployer, setup_params).abi_encode_params();
    let proxy = deploy_via_create3(
        &Create3Deployer::new(plan.create3_deployer, provider),
        provider,
        "InterchainTokenServiceProxy",
        proxy_bytecode,
        constructor_args,
        plan.proxy_salt,
        plan.its_proxy,
    )
    .await?;
    assert_eq!(proxy, plan.its_proxy);
    Ok(ItsServiceDeployment {
        implementation,
        proxy,
    })
}

async fn deploy_its_factory<P: Provider>(
    ctx: &mut DeployContext,
    step_idx: usize,
    plan: &ItsDeploymentPlan,
    provider: &P,
) -> Result<ItsFactoryDeployment> {
    ui::section("deploying InterchainTokenFactory implementation");
    let implementation = deploy_via_create2(
        &ConstAddressDeployer::new(plan.const_deployer, provider),
        provider,
        Create2Deployment {
            deployer: plan.deployer,
            name: "InterchainTokenFactoryImpl",
            bytecode: read_artifact_bytecode(
                &plan.artifact("InterchainTokenFactory.sol/InterchainTokenFactory.json"),
            )?,
            constructor_args: Some(plan.its_proxy.abi_encode()),
            salt: plan.implementation_salt,
            step: &plan.step,
        },
    )
    .await?;
    save_its_address(ctx, step_idx, "InterchainTokenFactoryImpl", implementation)?;

    ui::section("deploying InterchainTokenFactory proxy");
    let proxy = deploy_via_create3(
        &Create3Deployer::new(plan.create3_deployer, provider),
        provider,
        "InterchainTokenFactoryProxy",
        read_artifact_bytecode(&plan.artifact("proxies/InterchainProxy.sol/InterchainProxy.json"))?,
        (implementation, plan.deployer, Bytes::new()).abi_encode_params(),
        plan.factory_salt,
        plan.factory_proxy,
    )
    .await?;
    assert_eq!(proxy, plan.factory_proxy);
    Ok(ItsFactoryDeployment {
        implementation,
        proxy,
    })
}

fn save_its_deployment(
    ctx: &DeployContext,
    plan: &ItsDeploymentPlan,
    helpers: &ItsHelpers,
    service: &ItsServiceDeployment,
    factory: &ItsFactoryDeployment,
) -> Result<()> {
    ui::section("saving ITS contract data to target JSON");
    let predeploy_codehash = keccak256(read_artifact_bytecode(
        &plan.artifact("InterchainTokenService.sol/InterchainTokenService.json"),
    )?);
    update_target_json(
        &ctx.target_json,
        &ctx.axelar_id,
        "InterchainTokenService",
        json!({
            "salt": format!("ITS {}", plan.its_salt_key),
            "proxySalt": format!("ITS {}", plan.proxy_salt_key),
            "deployer": format!("{}", plan.deployer),
            "tokenManagerDeployer": format!("{}", helpers.token_manager_deployer),
            "interchainToken": format!("{}", helpers.interchain_token),
            "interchainTokenDeployer": format!("{}", helpers.interchain_token_deployer),
            "tokenManager": format!("{}", helpers.token_manager),
            "tokenHandler": format!("{}", helpers.token_handler),
            "implementation": format!("{}", service.implementation),
            "address": format!("{}", service.proxy),
            "predeployCodehash": format!("{predeploy_codehash}"),
            "owner": format!("{}", plan.deployer),
        }),
    )?;
    update_target_json(
        &ctx.target_json,
        &ctx.axelar_id,
        "InterchainTokenFactory",
        json!({
            "salt": format!("ITS Factory {}", plan.proxy_salt_key),
            "deployer": format!("{}", plan.deployer),
            "implementation": format!("{}", factory.implementation),
            "address": format!("{}", factory.proxy),
        }),
    )?;
    ui::success("ITS deployment complete!");
    ui::address("InterchainTokenService", &format!("{}", service.proxy));
    ui::address("InterchainTokenFactory", &format!("{}", factory.proxy));
    Ok(())
}

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
    let plan = prepare_its_plan(ctx, step_idx, step, deployer_addr, &provider).await?;

    let helpers = deploy_its_helpers(ctx, step_idx, &plan, &provider).await?;
    let service = deploy_its_service(ctx, step_idx, &plan, &helpers, &provider).await?;

    let factory = deploy_its_factory(ctx, step_idx, &plan, &provider).await?;
    save_its_deployment(ctx, &plan, &helpers, &service, &factory)
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
            if e.as_revert_data()
                .is_some_and(|data| data.starts_with(&[0x41, 0x02, 0xe8, 0x3a]))
            {
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
            if e.as_revert_data()
                .is_some_and(|data| data.starts_with(&[0x41, 0x02, 0xe8, 0x3a]))
            {
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
    ctx.state.steps[step_idx].set_its_address(name, addr)?;
    save_state(&ctx.state)?;
    Ok(())
}
