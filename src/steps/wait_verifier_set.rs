use eyre::Result;
use serde_json::{Value, json};

use crate::commands::deploy::DeployContext;
use crate::cosmos::{
    build_execute_msg_any, derive_axelar_wallet, lcd_cosmwasm_smart_query, read_axelar_config,
    read_axelar_contract_field, sign_and_broadcast_cosmos_tx,
};
use crate::timing::VERIFIER_SET_POLL_INTERVAL;
use crate::ui;

fn print_setup_instructions(
    env: crate::types::Network,
    chain: &str,
    rpc_url: &str,
    multisig: &str,
    prover: &str,
    verifier: &str,
) {
    ui::info(&format!(
        "waiting for verifier set on MultisigProver ({prover})..."
    ));
    ui::action_required(&[
        "An admin must complete these steps in order:",
        "",
        "1. Open a PR in https://github.com/axelarnetwork/infrastructure",
        "",
        &format!(
            "   File: infrastructure/{env}/apps/axelar-{env}/ampd/ampd-epsilon/helm-values.yaml"
        ),
        "",
        "   Add to config_toml.chains:",
        "",
        &format!("      - chain_name: {chain}"),
        &format!("        multisig: {multisig}"),
        &format!("        multisig_prover: {prover}"),
        &format!("        voting_verifier: {verifier}"),
        "",
        "   Add to handlers:",
        "",
        &format!("   {chain}:"),
        "     handler_type: evm",
        "     enabled: true",
        "     image:",
        "       repository: axelarnet/axelar-ampd-evm-handler",
        "       tag: v0.1.0",
        &format!("     rpc_url: {rpc_url}"),
        "",
        &format!("   File: infrastructure/{env}/apps/axelar-{env}/ampd/ampd/helm-values.yaml"),
        "",
        "   Add to handlers:",
        "",
        "     - type: MultisigSigner",
        &format!("       cosmwasm_contract: {multisig}"),
        &format!("       chain_name: {chain}"),
        "     - type: EvmMsgVerifier",
        &format!("       cosmwasm_contract: {verifier}"),
        &format!("       chain_name: {chain}"),
        &format!("       chain_rpc_url: {rpc_url}"),
        "       chain_finalization: RPCFinalizedBlock",
        "     - type: EvmVerifierSetVerifier",
        &format!("       cosmwasm_contract: {verifier}"),
        &format!("       chain_name: {chain}"),
        &format!("       chain_rpc_url: {rpc_url}"),
        "       chain_finalization: RPCFinalizedBlock",
        "",
        "2. Wait for the PR to be merged and deployed.",
        "",
        "3. Register chain support:",
        &format!("   ./register_chain_support.sh {chain}"),
    ]);
}

async fn wait_for_active_verifiers(lcd: &str, service_registry: &str, chain: &str, minimum: usize) {
    let spinner = ui::wait_spinner(&format!(
        "polling ServiceRegistry for active verifiers (need {minimum})..."
    ));
    loop {
        let query = json!({
            "active_verifiers": {
                "service_name": "amplifier",
                "chain_name": chain
            }
        });
        match lcd_cosmwasm_smart_query(lcd, service_registry, &query).await {
            Ok(data) if data.is_array() => {
                let count = data.as_array().map(|items| items.len()).unwrap_or(0);
                if count >= minimum {
                    spinner.finish_and_clear();
                    ui::success(&format!(
                        "{count} active verifiers registered for {chain} (>= {minimum})"
                    ));
                    return;
                }
                spinner.set_message(format!(
                    "{count}/{minimum} verifiers, retrying in {}s...",
                    VERIFIER_SET_POLL_INTERVAL.as_secs()
                ));
            }
            _ => spinner.set_message(format!(
                "not enough verifiers yet, retrying in {}s...",
                VERIFIER_SET_POLL_INTERVAL.as_secs()
            )),
        }
        tokio::time::sleep(VERIFIER_SET_POLL_INTERVAL).await;
    }
}

struct VerifierSetUpdate<'a> {
    admin_mnemonic: Option<&'a str>,
    lcd: &'a str,
    chain_id: &'a str,
    fee_denom: &'a str,
    gas_price: f64,
    prover: &'a str,
}

async fn update_or_wait_for_verifier_set(request: VerifierSetUpdate<'_>) -> Result<()> {
    if let Some(mnemonic) = request.admin_mnemonic {
        ui::info("calling update_verifier_set with admin key...");
        let (key, address) = derive_axelar_wallet(mnemonic)?;
        let message =
            build_execute_msg_any(&address, request.prover, &json!("update_verifier_set"))?;
        sign_and_broadcast_cosmos_tx(
            &key,
            &address,
            request.lcd,
            request.chain_id,
            request.fee_denom,
            request.gas_price,
            vec![message],
        )
        .await?;
        ui::success("update_verifier_set tx succeeded!");
        return Ok(());
    }
    ui::info("no admin mnemonic provided, waiting for manual update_verifier_set...");
    ui::info("(provide MULTISIG_PROVER_MNEMONIC in .env to automate this)");
    let spinner = ui::wait_spinner("waiting for verifier set...");
    loop {
        match lcd_cosmwasm_smart_query(request.lcd, request.prover, &json!("current_verifier_set"))
            .await
        {
            Ok(data) if !data.is_null() && data.get("id").is_some() => {
                let id = data["id"].as_str().unwrap_or("?");
                spinner.finish_and_clear();
                ui::success(&format!("verifier set found! id: {id}"));
                return Ok(());
            }
            _ => tokio::time::sleep(VERIFIER_SET_POLL_INTERVAL).await,
        }
    }
}

pub async fn run(ctx: &DeployContext) -> Result<()> {
    let content = tokio::fs::read_to_string(&ctx.target_json).await?;
    let root: Value = serde_json::from_str(&content)?;
    let chain_axelar_id = root
        .pointer(&format!("/chains/{}/axelarId", ctx.axelar_id))
        .and_then(|v| v.as_str())
        .unwrap_or(&ctx.axelar_id)
        .to_string();
    let rpc_url = ctx.state.rpc_url.clone();

    let prover_addr = read_axelar_contract_field(
        &ctx.target_json,
        &format!("/axelar/contracts/MultisigProver/{chain_axelar_id}/address"),
    )
    .await?;
    let verifier_addr = read_axelar_contract_field(
        &ctx.target_json,
        &format!("/axelar/contracts/VotingVerifier/{chain_axelar_id}/address"),
    )
    .await?;
    let multisig_addr =
        read_axelar_contract_field(&ctx.target_json, "/axelar/contracts/Multisig/address").await?;
    let service_registry_addr = read_axelar_contract_field(
        &ctx.target_json,
        "/axelar/contracts/ServiceRegistry/address",
    )
    .await?;
    let (lcd, chain_id, fee_denom, gas_price) = read_axelar_config(&ctx.target_json).await?;
    let env = ctx.state.env;

    // Check if verifier set already exists
    let query_msg = json!("current_verifier_set");
    if let Ok(data) = lcd_cosmwasm_smart_query(&lcd, &prover_addr, &query_msg).await
        && !data.is_null()
        && data.get("id").is_some()
    {
        let id = data["id"].as_str().unwrap_or("?");
        ui::success(&format!("verifier set already exists! id: {id}"));
        return Ok(());
    }

    let min_verifiers: usize = match env {
        crate::types::Network::DevnetAmplifier => 3,
        crate::types::Network::Mainnet => 25,
        _ => 22, // testnet, stagenet
    };
    print_setup_instructions(
        env,
        &chain_axelar_id,
        &rpc_url,
        &multisig_addr,
        &prover_addr,
        &verifier_addr,
    );
    wait_for_active_verifiers(
        &lcd,
        &service_registry_addr,
        &chain_axelar_id,
        min_verifiers,
    )
    .await;
    update_or_wait_for_verifier_set(VerifierSetUpdate {
        admin_mnemonic: ctx.state.admin_mnemonic.as_deref(),
        lcd: &lcd,
        chain_id: &chain_id,
        fee_denom: &fee_denom,
        gas_price,
        prover: &prover_addr,
    })
    .await
}
