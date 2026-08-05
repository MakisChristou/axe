use std::path::{Path, PathBuf};

use alloy::primitives::{Address, FixedBytes, keccak256};
use eyre::Result;
use serde_json::{Map, Value};

use crate::config::{AxelarGlobalContract, ChainContract, ChainsConfig};
use crate::ui;

pub async fn update_target_json(
    target_json: &Path,
    axelar_id: &str,
    contract_name: &str,
    contract_data: Value,
) -> Result<()> {
    let content = tokio::fs::read_to_string(target_json).await?;
    let mut root: Value = serde_json::from_str(&content)?;

    let contracts = root
        .pointer_mut(&format!("/chains/{axelar_id}/contracts"))
        .and_then(|v| v.as_object_mut())
        .ok_or_else(|| {
            eyre::eyre!(
                "no chains.{axelar_id}.contracts in {}",
                target_json.display()
            )
        })?;

    contracts.insert(contract_name.to_string(), contract_data);
    tokio::fs::write(target_json, serde_json::to_string_pretty(&root)? + "\n").await?;
    ui::success(&format!(
        "updated {contract_name} in {}",
        target_json.display()
    ));
    Ok(())
}

pub async fn patch_target_json(
    target_json: &Path,
    axelar_id: &str,
    contract_name: &str,
    patches: &Map<String, Value>,
) -> Result<()> {
    let content = tokio::fs::read_to_string(target_json).await?;
    let mut root: Value = serde_json::from_str(&content)?;

    let contract = root
        .pointer_mut(&format!("/chains/{axelar_id}/contracts/{contract_name}"))
        .and_then(|v| v.as_object_mut())
        .ok_or_else(|| {
            eyre::eyre!("no chains.{axelar_id}.contracts.{contract_name} in target json")
        })?;

    for (k, v) in patches {
        contract.insert(k.clone(), v.clone());
    }
    tokio::fs::write(target_json, serde_json::to_string_pretty(&root)? + "\n").await?;
    Ok(())
}

pub async fn read_contract_address(
    target_json: &Path,
    axelar_id: &str,
    contract: ChainContract,
) -> Result<Address> {
    let cfg = ChainsConfig::load(target_json).await?;
    let chain = cfg.chain(axelar_id)?;

    Ok(chain.contract_address(contract, axelar_id)?.parse()?)
}

pub async fn read_contract_address_by_name(
    target_json: &Path,
    axelar_id: &str,
    contract_name: &str,
) -> Result<Address> {
    let cfg = ChainsConfig::load(target_json).await?;
    let chain = cfg.chain(axelar_id)?;

    Ok(chain
        .contract_address_by_name(contract_name, axelar_id)?
        .parse()?)
}

/// Derive the axelar-contract-deployments repo root from target_json path.
/// target_json is like `<root>/axelar-chains-config/info/testnet.json`
pub fn deployments_root(target_json: &Path) -> Result<PathBuf> {
    target_json
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
        .map(|p| p.to_path_buf())
        .ok_or_else(|| {
            eyre::eyre!(
                "cannot derive deployments root from {}",
                target_json.display()
            )
        })
}

/// Known artifact paths relative to the axelar-contract-deployments repo root.
/// Returns (implementation_artifact, Option<proxy_artifact>).
pub fn artifact_paths_for_step(step_name: &str, root: &Path) -> Option<(String, Option<String>)> {
    let r = |p: &str| root.join(p).to_string_lossy().into_owned();
    match step_name {
        "ConstAddressDeployer" => Some((r("evm/legacy/ConstAddressDeployer.json"), None)),
        "Create3Deployer" => Some((
            r(
                "node_modules/@axelar-network/axelar-gmp-sdk-solidity/artifacts/contracts/deploy/Create3Deployer.sol/Create3Deployer.json",
            ),
            None,
        )),
        "AxelarGateway" => Some((
            r(
                "node_modules/@axelar-network/axelar-gmp-sdk-solidity/artifacts/contracts/gateway/AxelarAmplifierGateway.sol/AxelarAmplifierGateway.json",
            ),
            Some(r(
                "node_modules/@axelar-network/axelar-gmp-sdk-solidity/artifacts/contracts/gateway/AxelarAmplifierGatewayProxy.sol/AxelarAmplifierGatewayProxy.json",
            )),
        )),
        "Operators" => Some((
            r(
                "node_modules/@axelar-network/axelar-gmp-sdk-solidity/artifacts/contracts/utils/Operators.sol/Operators.json",
            ),
            None,
        )),
        "AxelarGasService" => Some((
            r(
                "node_modules/@axelar-network/axelar-cgp-solidity/artifacts/contracts/gas-service/AxelarGasService.sol/AxelarGasService.json",
            ),
            Some(r(
                "node_modules/@axelar-network/axelar-cgp-solidity/artifacts/contracts/gas-service/AxelarGasServiceProxy.sol/AxelarGasServiceProxy.json",
            )),
        )),
        _ => None,
    }
}

/// Compute domain separator: keccak256(chainAxelarId + routerAddress + axelarChainId)
pub async fn compute_domain_separator(
    target_json: &Path,
    axelar_id: &str,
) -> Result<FixedBytes<32>> {
    let cfg = ChainsConfig::load(target_json).await?;

    let chain_axelar_id = cfg
        .chains
        .get(axelar_id)
        .and_then(|c| c.axelar_id.as_deref())
        .ok_or_else(|| eyre::eyre!("no axelarId for chain {axelar_id}"))?;

    let router_address = cfg
        .axelar
        .global_contract_address(AxelarGlobalContract::Router)?;

    let axelar_chain_id = cfg
        .axelar
        .chain_id
        .as_deref()
        .ok_or_else(|| eyre::eyre!("no axelar.chainId in target json"))?;

    let input = format!("{chain_axelar_id}{router_address}{axelar_chain_id}");
    let hash = keccak256(input.as_bytes());
    ui::kv("domain separator input", &input);
    ui::kv("domain separator", &format!("{hash}"));
    Ok(hash)
}
