use std::path::PathBuf;
use std::time::Instant;

use alloy::signers::local::PrivateKeySigner;
use eyre::Result;

use crate::cli::resolve_axelar_id;
use crate::commands;
use crate::preflight;
use crate::state::{
    State, Step, StepKind, mark_step_completed, migrate_steps, next_pending_step, read_state,
    save_state, state_path,
};
use crate::steps;
use crate::ui;
use crate::utils::{artifact_paths_for_step, deployments_root};

pub struct DeployContext {
    pub axelar_id: String,
    pub state: State,
    pub rpc_url: String,
    pub target_json: PathBuf,
}

fn load_its_environment(state: &mut State) {
    if state.its_deployer_private_key.is_none()
        && let Ok(value) = std::env::var("ITS_DEPLOYER_PRIVATE_KEY")
    {
        state.its_deployer_private_key = Some(value);
        ui::info("loaded ITS_DEPLOYER_PRIVATE_KEY from env");
    }
    if state.its_salt.is_none()
        && let Ok(value) = std::env::var("ITS_SALT")
    {
        ui::info(&format!("loaded ITS_SALT from env: {value}"));
        state.its_salt = Some(value);
    }
    if state.its_proxy_salt.is_none()
        && let Ok(value) = std::env::var("ITS_PROXY_SALT")
    {
        ui::info(&format!("loaded ITS_PROXY_SALT from env: {value}"));
        state.its_proxy_salt = Some(value);
    }
}

async fn check_deployment_balances(ctx: &DeployContext) -> Result<()> {
    let mut wallets = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (label, private_key) in [
        ("deployer", ctx.state.deployer_private_key.as_deref()),
        (
            "gateway deployer",
            ctx.state.gateway_deployer_private_key.as_deref(),
        ),
        (
            "gas service deployer",
            ctx.state.gas_service_deployer_private_key.as_deref(),
        ),
        (
            "ITS deployer",
            ctx.state.its_deployer_private_key.as_deref(),
        ),
    ] {
        if let Some(private_key) = private_key
            && let Ok(signer) = private_key.parse::<PrivateKeySigner>()
            && seen.insert(signer.address())
        {
            wallets.push((label, signer.address()));
        }
    }
    let config = crate::config::ChainsConfig::load(&ctx.target_json).await?;
    let token_symbol = config
        .chains
        .get(&ctx.axelar_id)
        .and_then(|chain| chain.token_symbol.as_deref())
        .ok_or_else(|| {
            eyre::eyre!(
                "no tokenSymbol for chain '{}' in target json",
                ctx.axelar_id
            )
        })?;
    preflight::check_evm_balances(&ctx.rpc_url, &wallets, token_symbol).await
}

fn resolve_evm_key(state: &State, override_key: Option<&str>, step_name: &str) -> Result<String> {
    if let Some(private_key) = override_key {
        return Ok(private_key.to_string());
    }
    let (label, private_key) = match step_name {
        "EvmCompatibilityCheck" | "ConstAddressDeployer" | "Create3Deployer" => {
            ("deployerPrivateKey", state.deployer_private_key.as_deref())
        }
        "DeployInterchainTokenService" => (
            "itsDeployerPrivateKey",
            state.its_deployer_private_key.as_deref(),
        ),
        "AxelarGateway"
        | "Operators"
        | "RegisterOperators"
        | "TransferOperatorsOwnership"
        | "TransferGatewayOwnership" => (
            "gatewayDeployerPrivateKey",
            state.gateway_deployer_private_key.as_deref(),
        ),
        "TransferGasServiceOwnership" | "AxelarGasService" => (
            "gasServiceDeployerPrivateKey",
            state.gas_service_deployer_private_key.as_deref(),
        ),
        _ => return Err(eyre::eyre!("--private-key required for step {step_name}")),
    };
    private_key.map(str::to_string).ok_or_else(|| {
        eyre::eyre!(
            "no {label} in state and --private-key not provided. Run init with the key or pass --private-key"
        )
    })
}

struct StepExecution<'a> {
    step_idx: usize,
    step: &'a Step,
    private_key: Option<&'a str>,
    artifact: Option<&'a String>,
    proxy_artifact: Option<&'a String>,
    salt: &'a Option<String>,
}

async fn execute_step(ctx: &mut DeployContext, execution: StepExecution<'_>) -> Result<()> {
    let step_name = &execution.step.name;
    let private_key = |state: &State| resolve_evm_key(state, execution.private_key, step_name);
    let artifact = || {
        execution
            .artifact
            .ok_or_else(|| eyre::eyre!("--artifact-path required for deploy steps"))
    };
    match &execution.step.kind {
        StepKind::EvmCompat => steps::evm_compat::run(ctx, &private_key(&ctx.state)?).await,
        StepKind::DeployCreate => {
            steps::evm_deploy::run(
                ctx,
                step_name,
                "deploy-create",
                &private_key(&ctx.state)?,
                artifact()?,
                execution.salt,
            )
            .await
        }
        StepKind::DeployCreate2 => {
            steps::evm_deploy::run(
                ctx,
                step_name,
                "deploy-create2",
                &private_key(&ctx.state)?,
                artifact()?,
                execution.salt,
            )
            .await
        }
        StepKind::RegisterOperators => {
            steps::register_operators::run(ctx, &private_key(&ctx.state)?).await
        }
        StepKind::TransferOwnership { .. } => {
            steps::transfer_ownership::run(ctx, execution.step, &private_key(&ctx.state)?).await
        }
        StepKind::DeployGateway { .. } => {
            let proxy = execution
                .proxy_artifact
                .ok_or_else(|| eyre::eyre!("--proxy-artifact-path required (proxy artifact)"))?;
            steps::deploy_gateway::run(
                ctx,
                execution.step_idx,
                execution.step,
                &private_key(&ctx.state)?,
                artifact()?,
                proxy,
            )
            .await
        }
        StepKind::PredictAddress => steps::predict_address::run(ctx).await,
        StepKind::ConfigEdit => steps::config_edit::run(ctx).await,
        StepKind::CosmosTx { .. } => steps::cosmos_tx::run(ctx, execution.step, step_name).await,
        StepKind::CosmosPoll { .. } => steps::cosmos_poll::run(ctx, execution.step).await,
        StepKind::CosmosQuery => steps::cosmos_query::run(ctx).await,
        StepKind::WaitVerifierSet => steps::wait_verifier_set::run(ctx).await,
        StepKind::DeployUpgradable { .. } => {
            let implementation = execution
                .artifact
                .ok_or_else(|| eyre::eyre!("--artifact-path required (implementation artifact)"))?;
            let proxy = execution
                .proxy_artifact
                .ok_or_else(|| eyre::eyre!("--proxy-artifact-path required (proxy artifact)"))?;
            steps::deploy_upgradable::run(
                ctx,
                execution.step_idx,
                execution.step,
                step_name,
                &private_key(&ctx.state)?,
                implementation,
                proxy,
            )
            .await
        }
        StepKind::DeployIts { .. } => {
            steps::deploy_its::run(
                ctx,
                execution.step_idx,
                execution.step,
                &private_key(&ctx.state)?,
            )
            .await
        }
    }
}

pub async fn run(
    axelar_id: Option<String>,
    private_key: Option<String>,
    artifact_path: Option<String>,
    salt: Option<String>,
    proxy_artifact_path: Option<String>,
) -> Result<()> {
    let axelar_id = resolve_axelar_id(axelar_id)?;

    if !state_path(&axelar_id)?.exists() {
        ui::info("no state file found, running init…");
        commands::init::run().await?;
    }

    let mut state = read_state(&axelar_id).await?;

    // Migrate: append any new steps added since this state was created
    migrate_steps(&mut state);

    load_its_environment(&mut state);
    save_state(&state).await?;

    let rpc_url = state.rpc_url.clone();
    let target_json = state.target_json.clone();
    let env = state.env;
    let total_steps = state.steps.len();
    let deploy_start = Instant::now();

    ui::section(&format!("Deploy {axelar_id}"));
    ui::kv("environment", env.as_str());
    ui::kv("rpc", &rpc_url);
    ui::kv("steps", &total_steps.to_string());

    let mut ctx = DeployContext {
        axelar_id,
        state,
        rpc_url,
        target_json,
    };

    check_deployment_balances(&ctx).await?;

    loop {
        let Some((step_idx, step_ref)) = next_pending_step(&ctx.state) else {
            print_completion_message(&ctx.axelar_id, deploy_start);
            break;
        };

        // Clone the step so step handlers can mutate `ctx.state` (which
        // contains the same step) without holding an immutable borrow.
        let step: Step = step_ref.clone();
        let step_name = step.name.clone();
        let step_start = Instant::now();

        ui::step_header(step_idx + 1, total_steps, &step_name);

        // Resolve artifact paths: CLI flags override built-in defaults
        let repo_root = deployments_root(&ctx.target_json)?;
        let (resolved_artifact, resolved_proxy_artifact) = {
            let defaults = artifact_paths_for_step(&step_name, &repo_root);
            let art = artifact_path
                .clone()
                .or_else(|| defaults.as_ref().map(|(a, _)| a.clone()));
            let proxy_art = proxy_artifact_path
                .clone()
                .or_else(|| defaults.and_then(|(_, p)| p));
            (art, proxy_art)
        };

        execute_step(
            &mut ctx,
            StepExecution {
                step_idx,
                step: &step,
                private_key: private_key.as_deref(),
                artifact: resolved_artifact.as_ref(),
                proxy_artifact: resolved_proxy_artifact.as_ref(),
                salt: &salt,
            },
        )
        .await?;

        mark_step_completed(&mut ctx.state, step_idx);
        save_state(&ctx.state).await?;
        ui::success(&format!(
            "{step_name} completed ({})",
            ui::format_elapsed(step_start)
        ));
    }

    Ok(())
}

fn print_completion_message(axelar_id: &str, deploy_start: Instant) {
    ui::section("Deployment Complete");
    ui::success(&format!(
        "All steps completed for {axelar_id} ({})",
        ui::format_elapsed(deploy_start)
    ));
    println!();
    ui::info(&format!(
        "Run an end-to-end GMP test: cargo run -- test gmp --axelar-id {axelar_id}"
    ));
}
