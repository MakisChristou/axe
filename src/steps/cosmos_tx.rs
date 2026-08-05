//! Cosmos-side `Step` runners that interact with Axelar amplifier contracts
//! by submitting tx — either directly from the relayer wallet or wrapped in
//! a governance proposal on non-devnet networks. Each handler matches one
//! `step_name` and lives in its own submodule.

mod defaults;
mod instantiate;
mod register_deployment;
mod register_its;
mod reward_pools;

use eyre::Result;
use serde_json::Value;

use crate::commands::deploy::DeployContext;
use crate::cosmos::{derive_axelar_wallet, read_axelar_config};
use crate::state::Step;
use crate::types::Network;

#[derive(Clone, Copy)]
pub(super) struct StepTxContext<'a> {
    signing_key: &'a cosmrs::crypto::secp256k1::SigningKey,
    axelar_address: &'a str,
    lcd: &'a str,
    chain_id: &'a str,
    fee_denom: &'a str,
    gas_price: f64,
    use_governance: bool,
    chain_axelar_id: &'a str,
    env: &'a str,
    proposal_key: &'a str,
}

pub async fn run(ctx: &mut DeployContext, step: &Step, step_name: &str) -> Result<()> {
    let mnemonic = ctx.state.mnemonic.clone();
    let env = ctx.state.env;
    let (signing_key, axelar_address) = derive_axelar_wallet(&mnemonic)?;
    let (lcd, chain_id, fee_denom, gas_price) = read_axelar_config(&ctx.target_json).await?;
    let use_governance = env != Network::DevnetAmplifier;

    let chain_axelar_id = {
        let content = tokio::fs::read_to_string(&ctx.target_json).await?;
        let root: Value = serde_json::from_str(&content)?;
        root.pointer(&format!("/chains/{}/axelarId", ctx.axelar_id))
            .and_then(|v| v.as_str())
            .unwrap_or(&ctx.axelar_id)
            .to_string()
    };

    let proposal_key = step.proposal_key().unwrap_or("").to_string();
    let tx = StepTxContext {
        signing_key: &signing_key,
        axelar_address: &axelar_address,
        lcd: &lcd,
        chain_id: &chain_id,
        fee_denom: &fee_denom,
        gas_price,
        use_governance,
        chain_axelar_id: &chain_axelar_id,
        env: env.as_str(),
        proposal_key: &proposal_key,
    };

    match step_name {
        "InstantiateChainContracts" => {
            instantiate::run_instantiate(ctx, tx).await?;
        }
        "RegisterDeployment" => {
            register_deployment::run_register_deployment(ctx, tx).await?;
        }
        "CreateRewardPools" => {
            reward_pools::run_create_reward_pools(ctx, tx).await?;
        }
        "AddRewards" => {
            reward_pools::run_add_rewards(ctx, tx).await?;
        }
        "RegisterItsOnHub" => {
            register_its::run_register_its_on_hub(ctx, tx).await?;
        }
        _ => {
            return Err(eyre::eyre!("unknown cosmos-tx step: {step_name}"));
        }
    }

    Ok(())
}
