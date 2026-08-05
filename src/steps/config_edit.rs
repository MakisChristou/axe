use eyre::Result;
use serde_json::{Value, json};

use crate::commands::deploy::DeployContext;
use crate::types::Network;
use crate::ui;

pub async fn run(ctx: &DeployContext) -> Result<()> {
    let predicted_addr = ctx
        .state
        .predicted_gateway_address
        .ok_or_else(|| {
            eyre::eyre!("no predictedGatewayAddress in state. Run predict-address step first")
        })?
        .to_string();
    let env = ctx.state.env;

    let content = tokio::fs::read_to_string(&ctx.target_json).await?;
    let mut root: Value = serde_json::from_str(&content)?;

    let chain_axelar_id = root
        .pointer(&format!("/chains/{}/axelarId", ctx.axelar_id))
        .and_then(|v| v.as_str())
        .unwrap_or(&ctx.axelar_id)
        .to_string();

    let (governance_address, admin_address, service_name, voting_threshold, signing_threshold) =
        match env {
            Network::DevnetAmplifier => (
                "axelar1zlr7e5qf3sz7yf890rkh9tcnu87234k6k7ytd9",
                "axelar1zlr7e5qf3sz7yf890rkh9tcnu87234k6k7ytd9",
                "validators",
                json!(["6", "10"]),
                json!(["6", "10"]),
            ),
            Network::Testnet => (
                "axelar10d07y265gmmuvt4z0w9aw880jnsr700j7v9daj",
                "axelar17qafmnc4hrfa96cq37wg5l68sxh354pj6eky35",
                "amplifier",
                json!(["51", "100"]),
                json!(["51", "100"]),
            ),
            Network::Mainnet => (
                "axelar10d07y265gmmuvt4z0w9aw880jnsr700j7v9daj",
                "axelar1pczf792wf3p3xssk4dmwfxrh6hcqnrjp70danj",
                "amplifier",
                json!(["2", "3"]),
                json!(["2", "3"]),
            ),
            Network::Stagenet => (
                "axelar10d07y265gmmuvt4z0w9aw880jnsr700j7v9daj",
                "axelar1l7vz4m5g92kvga050vk9ycjynywdlk4zhs07dv",
                "amplifier",
                json!(["51", "100"]),
                json!(["51", "100"]),
            ),
        };

    // Add VotingVerifier chain config
    let voting_verifier_config = json!({
        "governanceAddress": governance_address,
        "serviceName": service_name,
        "sourceGatewayAddress": predicted_addr,
        "votingThreshold": voting_threshold,
        "blockExpiry": 50,
        "confirmationHeight": 1,
        "msgIdFormat": "hex_tx_hash_and_event_index",
        "addressFormat": "eip55"
    });

    let vv = root
        .pointer_mut("/axelar/contracts/VotingVerifier")
        .ok_or_else(|| eyre::eyre!("no axelar.contracts.VotingVerifier in target json"))?
        .as_object_mut()
        .ok_or_else(|| eyre::eyre!("VotingVerifier is not an object"))?;
    vv.insert(chain_axelar_id.clone(), voting_verifier_config);
    ui::success(&format!("added VotingVerifier.{chain_axelar_id} config"));

    // Add MultisigProver chain config
    let multisig_prover_config = json!({
        "governanceAddress": governance_address,
        "adminAddress": admin_address,
        "signingThreshold": signing_threshold,
        "serviceName": service_name,
        "verifierSetDiffThreshold": 0,
        "encoder": "abi",
        "keyType": "ecdsa"
    });

    let mp = root
        .pointer_mut("/axelar/contracts/MultisigProver")
        .ok_or_else(|| eyre::eyre!("no axelar.contracts.MultisigProver in target json"))?
        .as_object_mut()
        .ok_or_else(|| eyre::eyre!("MultisigProver is not an object"))?;
    mp.insert(chain_axelar_id.clone(), multisig_prover_config);
    ui::success(&format!("added MultisigProver.{chain_axelar_id} config"));

    tokio::fs::write(
        &ctx.target_json,
        serde_json::to_string_pretty(&root)? + "\n",
    )
    .await?;
    ui::success(&format!("updated {}", ctx.target_json.display()));

    Ok(())
}
