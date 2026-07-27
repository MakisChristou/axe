//! `InstantiateChainContracts` step. Asks Coordinator to instantiate the
//! per-chain Gateway / VotingVerifier / MultisigProver trio. On non-devnet
//! networks the message is wrapped in a governance proposal and the user
//! has to vote it through.

use std::fs;

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

pub(super) async fn run_instantiate(ctx: &mut DeployContext, tx: StepTxContext<'_>) -> Result<()> {
    let StepTxContext {
        signing_key,
        axelar_address,
        lcd,
        chain_id,
        fee_denom,
        gas_price,
        use_governance,
        chain_axelar_id,
        env,
        proposal_key,
    } = tx;
    ui::info(&format!(
        "instantiating chain contracts for {chain_axelar_id}..."
    ));

    let coordinator_addr =
        read_axelar_contract_field(&ctx.target_json, "/axelar/contracts/Coordinator/address")?;
    let rewards_addr =
        read_axelar_contract_field(&ctx.target_json, "/axelar/contracts/Rewards/address")?;
    let multisig_addr =
        read_axelar_contract_field(&ctx.target_json, "/axelar/contracts/Multisig/address")?;
    let _router_addr =
        read_axelar_contract_field(&ctx.target_json, "/axelar/contracts/Router/address")?;
    let chain_codec_addr =
        read_axelar_contract_field(&ctx.target_json, "/axelar/contracts/ChainCodecEvm/address")?;
    let governance_address =
        read_axelar_contract_field(&ctx.target_json, "/axelar/governanceAddress")?;

    ui::info("fetching code IDs...");
    let gateway_hash = read_axelar_contract_field(
        &ctx.target_json,
        "/axelar/contracts/Gateway/storeCodeProposalCodeHash",
    )?;
    let verifier_hash = read_axelar_contract_field(
        &ctx.target_json,
        "/axelar/contracts/VotingVerifier/storeCodeProposalCodeHash",
    )?;
    let prover_hash = read_axelar_contract_field(
        &ctx.target_json,
        "/axelar/contracts/MultisigProver/storeCodeProposalCodeHash",
    )?;

    let gateway_code_id = lcd_fetch_code_id(lcd, &gateway_hash).await?;
    let verifier_code_id = lcd_fetch_code_id(lcd, &verifier_hash).await?;
    let prover_code_id = lcd_fetch_code_id(lcd, &prover_hash).await?;
    ui::kv(
        "code IDs",
        &format!("gateway={gateway_code_id}, verifier={verifier_code_id}, prover={prover_code_id}"),
    );

    let content = fs::read_to_string(&ctx.target_json)?;
    let root: Value = serde_json::from_str(&content)?;
    let vv_config = root
        .pointer(&format!(
            "/axelar/contracts/VotingVerifier/{chain_axelar_id}"
        ))
        .ok_or_else(|| eyre::eyre!("no VotingVerifier.{chain_axelar_id} config"))?;
    let mp_config = root
        .pointer(&format!(
            "/axelar/contracts/MultisigProver/{chain_axelar_id}"
        ))
        .ok_or_else(|| eyre::eyre!("no MultisigProver.{chain_axelar_id} config"))?;

    let salt_key = ctx.state.cosm_salt.clone();
    let salt_bytes = get_salt_from_key(&salt_key);
    let salt_b64 = base64::engine::general_purpose::STANDARD.encode(salt_bytes.as_slice());

    let domain_separator = compute_domain_separator(&ctx.target_json, &ctx.axelar_id)?;
    let domain_sep_hex = alloy::hex::encode(domain_separator.as_slice());

    let contract_admin = match env {
        "devnet-amplifier" => "axelar1zlr7e5qf3sz7yf890rkh9tcnu87234k6k7ytd9",
        "testnet" => "axelar1wxej3l9aczsns3harrtdzk7rct29jl47tvu8mp",
        "mainnet" => "axelar1nctnr9x0qexemeld5w7w752rmqdsqqv92dw9am",
        _ => "axelar12qvsvse32cjyw60ztysd3v655aj5urqeup82ky",
    };

    let deployment_name =
        format!("{chain_axelar_id}-{gateway_code_id}-{verifier_code_id}-{prover_code_id}");

    let execute_msg = json!({
        "instantiate_chain_contracts": {
            "deployment_name": deployment_name,
            "salt": salt_b64,
            "params": {
                "manual": {
                    "gateway": {
                        "code_id": gateway_code_id,
                        "label": format!("Gateway-{chain_axelar_id}"),
                        "msg": null,
                        "contract_admin": contract_admin
                    },
                    "verifier": {
                        "code_id": verifier_code_id,
                        "label": format!("VotingVerifier-{chain_axelar_id}"),
                        "msg": {
                            "governance_address": vv_config["governanceAddress"],
                            "service_name": vv_config["serviceName"],
                            "source_gateway_address": vv_config["sourceGatewayAddress"],
                            "voting_threshold": vv_config["votingThreshold"],
                            "block_expiry": vv_config["blockExpiry"].as_u64().unwrap_or(DEFAULT_VV_BLOCK_EXPIRY).to_string(),
                            "confirmation_height": vv_config["confirmationHeight"],
                            "source_chain": chain_axelar_id,
                            "rewards_address": rewards_addr,
                            "msg_id_format": vv_config["msgIdFormat"],
                            "chain_codec_address": chain_codec_addr,
                            "address_format": vv_config["addressFormat"]
                        },
                        "contract_admin": contract_admin
                    },
                    "prover": {
                        "code_id": prover_code_id,
                        "label": format!("MultisigProver-{chain_axelar_id}"),
                        "msg": {
                            "governance_address": mp_config["governanceAddress"],
                            "admin_address": match env {
                                "testnet" => "axelar1w7y7v26rtnrj4vrx6q3qq4hfsmc68hhsxnadlf",
                                _ => mp_config.get("adminAddress").and_then(|v| v.as_str())
                                    .ok_or_else(|| eyre::eyre!("no adminAddress in MultisigProver config for {env}"))?,
                            },
                            "multisig_address": multisig_addr,
                            "signing_threshold": mp_config["signingThreshold"],
                            "service_name": mp_config["serviceName"],
                            "chain_name": chain_axelar_id,
                            "verifier_set_diff_threshold": mp_config["verifierSetDiffThreshold"],
                            "key_type": mp_config["keyType"],
                            "domain_separator": domain_sep_hex,
                            "notify_signing_session": false,
                            "expect_full_message_payloads": false,
                            "sig_verifier_address": null,
                            "chain_codec_address": chain_codec_addr
                        },
                        "contract_admin": contract_admin
                    }
                }
            }
        }
    });

    let json_str = serde_json::to_string_pretty(&execute_msg)?;
    ui::info(&format!(
        "execute msg: {}",
        ui::truncated_json(&json_str, 3)
    ));

    let sender = if use_governance {
        &governance_address
    } else {
        axelar_address
    };
    let inner_msg = build_execute_msg_any(sender, &coordinator_addr, &execute_msg)?;

    let messages = if use_governance {
        let deposit_amount = read_axelar_contract_field(
            &ctx.target_json,
            "/axelar/govProposalExpeditedDepositAmount",
        )
        .unwrap_or_else(|_| DEFAULT_PROPOSAL_DEPOSIT_UAXL.to_string());
        let title = format!("Instantiate chain contracts for {chain_axelar_id}");
        let summary = format!(
            "Instantiate Gateway, VotingVerifier and MultisigProver contracts for {chain_axelar_id} via Coordinator"
        );
        vec![build_submit_proposal_any(
            axelar_address,
            vec![inner_msg],
            &title,
            &summary,
            &deposit_amount,
            fee_denom,
            true,
        )?]
    } else {
        vec![inner_msg]
    };

    let tx_resp = sign_and_broadcast_cosmos_tx(
        signing_key,
        axelar_address,
        lcd,
        chain_id,
        fee_denom,
        gas_price,
        messages,
    )
    .await?;

    // Save deployment name to target json
    let content = fs::read_to_string(&ctx.target_json)?;
    let mut root: Value = serde_json::from_str(&content)?;
    let coord = root
        .pointer_mut("/axelar/contracts/Coordinator")
        .and_then(|v| v.as_object_mut())
        .ok_or_else(|| eyre::eyre!("no Coordinator config"))?;
    if coord.get("deployments").is_none() {
        coord.insert("deployments".to_string(), json!({}));
    }
    coord["deployments"].as_object_mut().unwrap().insert(
        chain_axelar_id.to_string(),
        json!({
            "deploymentName": deployment_name,
            "salt": salt_key
        }),
    );

    if let Some(vv) = root.pointer_mut(&format!(
        "/axelar/contracts/VotingVerifier/{chain_axelar_id}"
    )) {
        vv["codeId"] = json!(verifier_code_id);
        vv["contractAdmin"] = json!(contract_admin);
    }
    if let Some(mp) = root.pointer_mut(&format!(
        "/axelar/contracts/MultisigProver/{chain_axelar_id}"
    )) {
        mp["codeId"] = json!(prover_code_id);
        mp["domainSeparator"] = json!(format!("0x{domain_sep_hex}"));
        mp["contractAdmin"] = json!(contract_admin);
    }
    if let Some(gw) = root.pointer_mut(&format!("/axelar/contracts/Gateway/{chain_axelar_id}")) {
        gw["codeId"] = json!(gateway_code_id);
        gw["contractAdmin"] = json!(contract_admin);
    } else if let Some(gateway_obj) = root
        .pointer_mut("/axelar/contracts/Gateway")
        .and_then(|v| v.as_object_mut())
    {
        gateway_obj.insert(
            chain_axelar_id.to_string(),
            json!({
                "codeId": gateway_code_id,
                "contractAdmin": contract_admin
            }),
        );
    }
    fs::write(
        &ctx.target_json,
        serde_json::to_string_pretty(&root)? + "\n",
    )?;

    if use_governance {
        let proposal_id = extract_proposal_id(&tx_resp)?;
        ui::kv("proposal submitted", &proposal_id.to_string());
        ui::action_required(&[
            "Vote on the proposal:",
            &format!("./vote_{env}_proposal.sh {env}-nodes {proposal_id}"),
        ]);
        ctx.state
            .proposals
            .insert(proposal_key.to_string(), proposal_id);
    } else {
        ui::success("direct execution completed");
    }

    Ok(())
}
