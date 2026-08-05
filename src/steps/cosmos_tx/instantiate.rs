//! `InstantiateChainContracts` step. Asks Coordinator to instantiate the
//! per-chain Gateway / VotingVerifier / MultisigProver trio. On non-devnet
//! networks the message is wrapped in a governance proposal and the user
//! has to vote it through.

use base64::Engine;
use eyre::Result;
use serde_json::{Value, json};

use super::StepTxContext;
use super::defaults::{DEFAULT_PROPOSAL_DEPOSIT_UAXL, DEFAULT_VV_BLOCK_EXPIRY};
use crate::commands::deploy::DeployContext;
use crate::cosmos::{
    build_execute_msg_any, build_submit_proposal_any, extract_proposal_id, lcd_fetch_code_id,
    read_axelar_contract_field, sign_and_broadcast_cosmos_tx,
};
use crate::evm::get_salt_from_key;
use crate::ui;
use crate::utils::compute_domain_separator;

struct ChainContractAddresses {
    coordinator: String,
    rewards: String,
    multisig: String,
    codec: String,
    governance: String,
}

struct ChainCodeIds {
    gateway: u64,
    verifier: u64,
    prover: u64,
}

struct InstantiatePlan {
    execute_msg: Value,
    deployment_name: String,
    salt_key: String,
    domain_separator: String,
    contract_admin: &'static str,
    codes: ChainCodeIds,
}

async fn read_chain_contract_addresses(ctx: &DeployContext) -> Result<ChainContractAddresses> {
    read_axelar_contract_field(&ctx.target_json, "/axelar/contracts/Router/address").await?;
    Ok(ChainContractAddresses {
        coordinator: read_axelar_contract_field(
            &ctx.target_json,
            "/axelar/contracts/Coordinator/address",
        )
        .await?,
        rewards: read_axelar_contract_field(&ctx.target_json, "/axelar/contracts/Rewards/address")
            .await?,
        multisig: read_axelar_contract_field(
            &ctx.target_json,
            "/axelar/contracts/Multisig/address",
        )
        .await?,
        codec: read_axelar_contract_field(
            &ctx.target_json,
            "/axelar/contracts/ChainCodecEvm/address",
        )
        .await?,
        governance: read_axelar_contract_field(&ctx.target_json, "/axelar/governanceAddress")
            .await?,
    })
}

async fn fetch_chain_code_ids(ctx: &DeployContext, lcd: &str) -> Result<ChainCodeIds> {
    ui::info("fetching code IDs...");
    let gateway_hash = read_code_hash(ctx, "Gateway").await?;
    let verifier_hash = read_code_hash(ctx, "VotingVerifier").await?;
    let prover_hash = read_code_hash(ctx, "MultisigProver").await?;

    let gateway = lcd_fetch_code_id(lcd, &gateway_hash).await?;
    let verifier = lcd_fetch_code_id(lcd, &verifier_hash).await?;
    let prover = lcd_fetch_code_id(lcd, &prover_hash).await?;
    ui::kv(
        "code IDs",
        &format!("gateway={gateway}, verifier={verifier}, prover={prover}"),
    );
    Ok(ChainCodeIds {
        gateway,
        verifier,
        prover,
    })
}

async fn read_code_hash(ctx: &DeployContext, contract: &str) -> Result<String> {
    let pointer = format!("/axelar/contracts/{contract}/storeCodeProposalCodeHash");

    read_axelar_contract_field(&ctx.target_json, &pointer).await
}

fn contract_admin(env: &str) -> &'static str {
    match env {
        "devnet-amplifier" => "axelar1zlr7e5qf3sz7yf890rkh9tcnu87234k6k7ytd9",
        "testnet" => "axelar1wxej3l9aczsns3harrtdzk7rct29jl47tvu8mp",
        "mainnet" => "axelar1nctnr9x0qexemeld5w7w752rmqdsqqv92dw9am",
        _ => "axelar12qvsvse32cjyw60ztysd3v655aj5urqeup82ky",
    }
}

async fn build_instantiate_plan(
    ctx: &DeployContext,
    tx: &StepTxContext<'_>,
    addresses: &ChainContractAddresses,
    codes: ChainCodeIds,
) -> Result<InstantiatePlan> {
    let content = tokio::fs::read_to_string(&ctx.target_json).await?;
    let root: Value = serde_json::from_str(&content)?;
    let verifier = root
        .pointer(&format!(
            "/axelar/contracts/VotingVerifier/{}",
            tx.chain_axelar_id
        ))
        .ok_or_else(|| eyre::eyre!("no VotingVerifier.{} config", tx.chain_axelar_id))?;
    let prover = root
        .pointer(&format!(
            "/axelar/contracts/MultisigProver/{}",
            tx.chain_axelar_id
        ))
        .ok_or_else(|| eyre::eyre!("no MultisigProver.{} config", tx.chain_axelar_id))?;
    let salt_key = ctx.state.cosm_salt.clone();
    let salt =
        base64::engine::general_purpose::STANDARD.encode(get_salt_from_key(&salt_key).as_slice());
    let domain_separator = alloy::hex::encode(
        compute_domain_separator(&ctx.target_json, &ctx.axelar_id)
            .await?
            .as_slice(),
    );
    let admin = contract_admin(tx.env);
    let deployment_name = format!(
        "{}-{}-{}-{}",
        tx.chain_axelar_id, codes.gateway, codes.verifier, codes.prover
    );
    let admin_address = match tx.env {
        "testnet" => "axelar1w7y7v26rtnrj4vrx6q3qq4hfsmc68hhsxnadlf",
        _ => prover
            .get("adminAddress")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                eyre::eyre!("no adminAddress in MultisigProver config for {}", tx.env)
            })?,
    };
    let execute_msg = json!({
        "instantiate_chain_contracts": {
            "deployment_name": deployment_name,
            "salt": salt,
            "params": { "manual": {
                "gateway": {
                    "code_id": codes.gateway,
                    "label": format!("Gateway-{}", tx.chain_axelar_id),
                    "msg": null,
                    "contract_admin": admin
                },
                "verifier": {
                    "code_id": codes.verifier,
                    "label": format!("VotingVerifier-{}", tx.chain_axelar_id),
                    "msg": {
                        "governance_address": verifier["governanceAddress"],
                        "service_name": verifier["serviceName"],
                        "source_gateway_address": verifier["sourceGatewayAddress"],
                        "voting_threshold": verifier["votingThreshold"],
                        "block_expiry": verifier["blockExpiry"].as_u64().unwrap_or(DEFAULT_VV_BLOCK_EXPIRY).to_string(),
                        "confirmation_height": verifier["confirmationHeight"],
                        "source_chain": tx.chain_axelar_id,
                        "rewards_address": addresses.rewards,
                        "msg_id_format": verifier["msgIdFormat"],
                        "chain_codec_address": addresses.codec,
                        "address_format": verifier["addressFormat"]
                    },
                    "contract_admin": admin
                },
                "prover": {
                    "code_id": codes.prover,
                    "label": format!("MultisigProver-{}", tx.chain_axelar_id),
                    "msg": {
                        "governance_address": prover["governanceAddress"],
                        "admin_address": admin_address,
                        "multisig_address": addresses.multisig,
                        "signing_threshold": prover["signingThreshold"],
                        "service_name": prover["serviceName"],
                        "chain_name": tx.chain_axelar_id,
                        "verifier_set_diff_threshold": prover["verifierSetDiffThreshold"],
                        "key_type": prover["keyType"],
                        "domain_separator": domain_separator,
                        "notify_signing_session": false,
                        "expect_full_message_payloads": false,
                        "sig_verifier_address": null,
                        "chain_codec_address": addresses.codec
                    },
                    "contract_admin": admin
                }
            }}
        }
    });
    Ok(InstantiatePlan {
        execute_msg,
        deployment_name,
        salt_key,
        domain_separator,
        contract_admin: admin,
        codes,
    })
}

async fn save_instantiate_plan(
    ctx: &DeployContext,
    chain_axelar_id: &str,
    plan: &InstantiatePlan,
) -> Result<()> {
    let content = tokio::fs::read_to_string(&ctx.target_json).await?;
    let mut root: Value = serde_json::from_str(&content)?;
    let coordinator = root
        .pointer_mut("/axelar/contracts/Coordinator")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| eyre::eyre!("no Coordinator config"))?;
    if coordinator.get("deployments").is_none() {
        coordinator.insert("deployments".to_string(), json!({}));
    }
    coordinator["deployments"]
        .as_object_mut()
        .ok_or_else(|| eyre::eyre!("Coordinator.deployments is not an object"))?
        .insert(
            chain_axelar_id.to_string(),
            json!({
                "deploymentName": plan.deployment_name,
                "salt": plan.salt_key
            }),
        );
    if let Some(verifier) = root.pointer_mut(&format!(
        "/axelar/contracts/VotingVerifier/{chain_axelar_id}"
    )) {
        verifier["codeId"] = json!(plan.codes.verifier);
        verifier["contractAdmin"] = json!(plan.contract_admin);
    }
    if let Some(prover) = root.pointer_mut(&format!(
        "/axelar/contracts/MultisigProver/{chain_axelar_id}"
    )) {
        prover["codeId"] = json!(plan.codes.prover);
        prover["domainSeparator"] = json!(format!("0x{}", plan.domain_separator));
        prover["contractAdmin"] = json!(plan.contract_admin);
    }
    if let Some(gateway) = root.pointer_mut(&format!("/axelar/contracts/Gateway/{chain_axelar_id}"))
    {
        gateway["codeId"] = json!(plan.codes.gateway);
        gateway["contractAdmin"] = json!(plan.contract_admin);
    } else if let Some(gateways) = root
        .pointer_mut("/axelar/contracts/Gateway")
        .and_then(Value::as_object_mut)
    {
        gateways.insert(
            chain_axelar_id.to_string(),
            json!({
                "codeId": plan.codes.gateway,
                "contractAdmin": plan.contract_admin
            }),
        );
    }
    tokio::fs::write(
        &ctx.target_json,
        serde_json::to_string_pretty(&root)? + "\n",
    )
    .await?;
    Ok(())
}

pub(super) async fn run_instantiate(ctx: &mut DeployContext, tx: StepTxContext<'_>) -> Result<()> {
    ui::info(&format!(
        "instantiating chain contracts for {}...",
        tx.chain_axelar_id
    ));
    let addresses = read_chain_contract_addresses(ctx).await?;
    let codes = fetch_chain_code_ids(ctx, tx.lcd).await?;
    let plan = build_instantiate_plan(ctx, &tx, &addresses, codes).await?;
    let json_str = serde_json::to_string_pretty(&plan.execute_msg)?;
    ui::info(&format!(
        "execute msg: {}",
        ui::truncated_json(&json_str, 3)
    ));
    let sender = if tx.use_governance {
        &addresses.governance
    } else {
        tx.axelar_address
    };
    let inner_msg = build_execute_msg_any(sender, &addresses.coordinator, &plan.execute_msg)?;
    let messages = if tx.use_governance {
        let deposit_amount = read_axelar_contract_field(
            &ctx.target_json,
            "/axelar/govProposalExpeditedDepositAmount",
        )
        .await
        .unwrap_or_else(|_| DEFAULT_PROPOSAL_DEPOSIT_UAXL.to_string());
        let title = format!("Instantiate chain contracts for {}", tx.chain_axelar_id);
        let summary = format!(
            "Instantiate Gateway, VotingVerifier and MultisigProver contracts for {} via Coordinator",
            tx.chain_axelar_id
        );
        vec![build_submit_proposal_any(
            tx.axelar_address,
            vec![inner_msg],
            &title,
            &summary,
            &deposit_amount,
            tx.fee_denom,
            true,
        )?]
    } else {
        vec![inner_msg]
    };
    let tx_resp = sign_and_broadcast_cosmos_tx(
        tx.signing_key,
        tx.axelar_address,
        tx.lcd,
        tx.chain_id,
        tx.fee_denom,
        tx.gas_price,
        messages,
    )
    .await?;
    save_instantiate_plan(ctx, tx.chain_axelar_id, &plan).await?;
    if tx.use_governance {
        let proposal_id = extract_proposal_id(&tx_resp)?;
        ui::kv("proposal submitted", &proposal_id.to_string());
        ui::action_required(&[
            "Vote on the proposal:",
            &format!(
                "./vote_{}_proposal.sh {}-nodes {proposal_id}",
                tx.env, tx.env
            ),
        ]);
        ctx.state
            .proposals
            .insert(tx.proposal_key.to_string(), proposal_id);
    } else {
        ui::success("direct execution completed");
    }

    Ok(())
}
