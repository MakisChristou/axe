//! On-disk state for the deployment pipeline.
//!
//! State lives at `{data_dir}/axe/{axelarId}.json` and is read once at the
//! start of a `axe deploy run` (or read-only by `axe deploy status`),
//! mutated as steps complete, and written back atomically. Keep the
//! types here close to the JSON they serialize to — the schema *is* the
//! type.
//!
//! The on-disk format is **not a stable contract**: when the type definition
//! changes in a way that breaks deserialization, users are expected to run
//! `axe deploy reset` and re-init.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use alloy::primitives::Address;
use eyre::Result;
use serde::{Deserialize, Serialize};

use crate::types::{ChainKey, Network};
use crate::ui;

// ---------------------------------------------------------------------------
// Top-level State
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct State {
    pub axelar_id: ChainKey,
    pub rpc_url: String,
    pub target_json: PathBuf,
    pub mnemonic: String,
    pub env: Network,
    pub cosm_salt: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admin_mnemonic: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deployer_private_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway_deployer_private_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway_deployer: Option<Address>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gas_service_deployer_private_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub its_deployer_private_key: Option<String>,
    /// Versioning identifier for the ITS deployment, hashed via
    /// `get_salt_from_key` into a 32-byte CREATE2 salt at deploy time.
    /// Free-form string (e.g. "v2.2.0"), not on-the-wire bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub its_salt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub its_proxy_salt: Option<String>,

    /// The CREATE2-predicted gateway address, written by the
    /// `predict-address` step before the gateway is actually deployed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub predicted_gateway_address: Option<Address>,

    /// Deployed by `axe test gmp` for the EVM-direct flow; cached so reruns
    /// reuse the same contract.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_receiver_address: Option<Address>,

    /// Cosmos governance proposal IDs by `proposalKey`. Populated by
    /// `cosmos-tx` step handlers and read by the matching `cosmos-poll`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub proposals: BTreeMap<String, u64>,

    pub steps: Vec<Step>,
}

// ---------------------------------------------------------------------------
// Steps
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    pub name: String,
    pub status: StepStatus,
    #[serde(flatten)]
    pub kind: StepKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StepStatus {
    Pending,
    Completed,
}

/// One deployment step's kind plus any kind-specific state. Each variant's
/// extra fields are persisted alongside `name`/`status` via
/// `#[serde(flatten)]` on `Step::kind`, so the on-disk shape matches the
/// pre-typed JSON (e.g. `{ "name": ..., "status": ..., "kind": "deploy-gateway",
/// "implementationAddress": "0x..." }`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum StepKind {
    EvmCompat,
    DeployCreate,
    DeployCreate2,
    PredictAddress,
    ConfigEdit,
    CosmosTx {
        #[serde(rename = "proposalKey")]
        proposal_key: String,
    },
    CosmosPoll {
        #[serde(rename = "proposalKey")]
        proposal_key: String,
    },
    CosmosQuery,
    WaitVerifierSet,
    DeployGateway {
        #[serde(
            rename = "implementationAddress",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        implementation_address: Option<Address>,
    },
    RegisterOperators,
    DeployUpgradable {
        #[serde(
            rename = "implementationAddress",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        implementation_address: Option<Address>,
        #[serde(
            rename = "proxyAddress",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        proxy_address: Option<Address>,
    },
    /// ITS deploy is broken into many sub-deploys for idempotent retry.
    /// Each sub-deploy writes its address back here as it succeeds; on a
    /// later run, the handler sees what's already done and skips it.
    DeployIts {
        #[serde(
            rename = "itsDeployerAddress",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        its_deployer_address: Option<Address>,
        #[serde(
            rename = "TokenManagerDeployerAddress",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        token_manager_deployer_address: Option<Address>,
        #[serde(
            rename = "InterchainTokenAddress",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        interchain_token_address: Option<Address>,
        #[serde(
            rename = "InterchainTokenDeployerAddress",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        interchain_token_deployer_address: Option<Address>,
        #[serde(
            rename = "TokenManagerAddress",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        token_manager_address: Option<Address>,
        #[serde(
            rename = "TokenHandlerAddress",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        token_handler_address: Option<Address>,
        #[serde(
            rename = "InterchainTokenServiceImplAddress",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        interchain_token_service_impl_address: Option<Address>,
        #[serde(
            rename = "InterchainTokenFactoryImplAddress",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        interchain_token_factory_impl_address: Option<Address>,
    },
    TransferOwnership {
        contract: String,
        #[serde(rename = "newOwner")]
        new_owner: Address,
    },
}

// ---------------------------------------------------------------------------
// Default deployment plan
// ---------------------------------------------------------------------------

/// Returns the canonical ordered set of steps that a fresh deployment runs.
/// `migrate_steps` appends any new entries here onto an existing state file
/// so partial deployments pick up newly-added stages without manual surgery.
pub fn default_steps() -> Vec<Step> {
    use StepKind as K;
    let new_owner = alloy::primitives::address!("49845e5d9985d8dc941462293ed38EEfF18B0eAE");
    let mut steps = vec![
        pending_step("EvmCompatibilityCheck", K::EvmCompat),
        pending_step("ConstAddressDeployer", K::DeployCreate),
        pending_step("Create3Deployer", K::DeployCreate2),
        pending_step("PredictGatewayAddress", K::PredictAddress),
        pending_step("AddCosmWasmConfig", K::ConfigEdit),
        pending_step(
            "InstantiateChainContracts",
            K::CosmosTx {
                proposal_key: "instantiate".into(),
            },
        ),
        pending_step(
            "WaitInstantiateProposal",
            K::CosmosPoll {
                proposal_key: "instantiate".into(),
            },
        ),
        pending_step("SaveDeployedContracts", K::CosmosQuery),
        pending_step(
            "RegisterDeployment",
            K::CosmosTx {
                proposal_key: "register".into(),
            },
        ),
        pending_step(
            "WaitRegisterProposal",
            K::CosmosPoll {
                proposal_key: "register".into(),
            },
        ),
        pending_step(
            "CreateRewardPools",
            K::CosmosTx {
                proposal_key: "rewardPools".into(),
            },
        ),
        pending_step(
            "WaitRewardPoolsProposal",
            K::CosmosPoll {
                proposal_key: "rewardPools".into(),
            },
        ),
        pending_step(
            "AddRewards",
            K::CosmosTx {
                proposal_key: "addRewards".into(),
            },
        ),
        pending_step("WaitForVerifierSet", K::WaitVerifierSet),
        pending_step(
            "AxelarGateway",
            K::DeployGateway {
                implementation_address: None,
            },
        ),
        pending_step("Operators", K::DeployCreate2),
        pending_step("RegisterOperators", K::RegisterOperators),
        pending_step(
            "AxelarGasService",
            K::DeployUpgradable {
                implementation_address: None,
                proxy_address: None,
            },
        ),
    ];
    steps.extend([
        ownership_step("TransferOperatorsOwnership", "Operators", new_owner),
        ownership_step("TransferGatewayOwnership", "AxelarGateway", new_owner),
        ownership_step("TransferGasServiceOwnership", "AxelarGasService", new_owner),
        deploy_its_step(),
        pending_step(
            "RegisterItsOnHub",
            K::CosmosTx {
                proposal_key: "itsHubRegister".into(),
            },
        ),
        pending_step(
            "WaitItsHubRegistration",
            K::CosmosPoll {
                proposal_key: "itsHubRegister".into(),
            },
        ),
    ]);
    steps
}

fn pending_step(name: &str, kind: StepKind) -> Step {
    Step {
        name: name.to_string(),
        status: StepStatus::Pending,
        kind,
    }
}

fn ownership_step(name: &str, contract: &str, new_owner: Address) -> Step {
    pending_step(
        name,
        StepKind::TransferOwnership {
            contract: contract.into(),
            new_owner,
        },
    )
}

fn deploy_its_step() -> Step {
    pending_step(
        "DeployInterchainTokenService",
        StepKind::DeployIts {
            its_deployer_address: None,
            token_manager_deployer_address: None,
            interchain_token_address: None,
            interchain_token_deployer_address: None,
            token_manager_address: None,
            token_handler_address: None,
            interchain_token_service_impl_address: None,
            interchain_token_factory_impl_address: None,
        },
    )
}

// ---------------------------------------------------------------------------
// Persistence
// ---------------------------------------------------------------------------

pub fn data_dir() -> Result<PathBuf> {
    let dir = dirs::data_dir()
        .ok_or_else(|| eyre::eyre!("could not determine data directory"))?
        .join("axe");
    Ok(dir)
}

pub fn state_path(axelar_id: &str) -> Result<PathBuf> {
    Ok(data_dir()?.join(format!("{axelar_id}.json")))
}

/// Read and deserialize the state file into a typed `State`.
pub fn read_state(axelar_id: &str) -> Result<State> {
    let path = state_path(axelar_id)?;
    read_state_at(&path)
}

pub fn read_state_at(path: &Path) -> Result<State> {
    let content = fs::read_to_string(path).map_err(|e| {
        eyre::eyre!(
            "failed to read state file {}: {e}. Run `init` first.",
            path.display()
        )
    })?;
    Ok(serde_json::from_str(&content)?)
}

/// Serialize and write the state file. The path is derived from
/// `state.axelar_id` so callers don't need to track it.
pub fn save_state(state: &State) -> Result<()> {
    let path = state_path(state.axelar_id.as_str())?;
    save_state_at(state, &path)
}

pub fn save_state_at(state: &State, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_string_pretty(state)? + "\n")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Step traversal + mutation
// ---------------------------------------------------------------------------

/// Find the first step whose status is still `Pending`.
pub fn next_pending_step(state: &State) -> Option<(usize, &Step)> {
    state
        .steps
        .iter()
        .enumerate()
        .find(|(_, s)| s.status == StepStatus::Pending)
}

/// Mark step `idx` as completed.
pub fn mark_step_completed(state: &mut State, idx: usize) {
    state.steps[idx].status = StepStatus::Completed;
}

/// Append any default steps (by name) that aren't already in `state.steps`,
/// so existing partial deployments pick up newly-introduced stages.
pub fn migrate_steps(state: &mut State) {
    let existing_names: std::collections::HashSet<String> =
        state.steps.iter().map(|s| s.name.clone()).collect();
    let mut added = 0;
    for default_step in default_steps() {
        if !existing_names.contains(&default_step.name) {
            state.steps.push(default_step);
            added += 1;
        }
    }
    if added > 0 {
        ui::info(&format!("migrated state: added {added} new step(s)"));
    }
}

// ---------------------------------------------------------------------------
// Step accessors — keep call sites concise without forcing match arms.
// ---------------------------------------------------------------------------

impl Step {
    /// The `proposalKey` for `CosmosTx` and `CosmosPoll` steps.
    pub fn proposal_key(&self) -> Option<&str> {
        match &self.kind {
            StepKind::CosmosTx { proposal_key } | StepKind::CosmosPoll { proposal_key } => {
                Some(proposal_key.as_str())
            }
            _ => None,
        }
    }

    /// Returns the implementation address recorded by a `DeployGateway` or
    /// `DeployUpgradable` step, or `None` if not yet deployed (or the wrong
    /// step kind).
    pub fn implementation_address(&self) -> Option<Address> {
        match &self.kind {
            StepKind::DeployGateway {
                implementation_address,
            }
            | StepKind::DeployUpgradable {
                implementation_address,
                ..
            } => *implementation_address,
            _ => None,
        }
    }

    /// Returns the proxy address recorded by a `DeployUpgradable` step.
    pub fn proxy_address(&self) -> Option<Address> {
        match &self.kind {
            StepKind::DeployUpgradable { proxy_address, .. } => *proxy_address,
            _ => None,
        }
    }

    /// Set the implementation address on a compatible deployment step.
    pub fn set_implementation_address(&mut self, addr: Address) -> Result<()> {
        match &mut self.kind {
            StepKind::DeployGateway {
                implementation_address,
            }
            | StepKind::DeployUpgradable {
                implementation_address,
                ..
            } => *implementation_address = Some(addr),
            other => {
                return Err(eyre::eyre!(
                    "cannot set an implementation address on step kind {other:?}"
                ));
            }
        }
        Ok(())
    }

    /// Set the proxy address on a `DeployUpgradable` step.
    pub fn set_proxy_address(&mut self, addr: Address) -> Result<()> {
        match &mut self.kind {
            StepKind::DeployUpgradable { proxy_address, .. } => *proxy_address = Some(addr),
            other => {
                return Err(eyre::eyre!(
                    "cannot set a proxy address on step kind {other:?}"
                ));
            }
        }
        Ok(())
    }

    /// Read one of the eight ITS sub-deploy addresses by name. Used by
    /// `deploy_its.rs`'s idempotent-retry loop. Returns `None` if the step
    /// isn't a `DeployIts`, the name is unknown, or the address hasn't been
    /// recorded yet.
    pub fn its_address(&self, name: &str) -> Option<Address> {
        let StepKind::DeployIts {
            its_deployer_address,
            token_manager_deployer_address,
            interchain_token_address,
            interchain_token_deployer_address,
            token_manager_address,
            token_handler_address,
            interchain_token_service_impl_address,
            interchain_token_factory_impl_address,
        } = &self.kind
        else {
            return None;
        };
        match name {
            "itsDeployer" => *its_deployer_address,
            "TokenManagerDeployer" => *token_manager_deployer_address,
            "InterchainToken" => *interchain_token_address,
            "InterchainTokenDeployer" => *interchain_token_deployer_address,
            "TokenManager" => *token_manager_address,
            "TokenHandler" => *token_handler_address,
            "InterchainTokenServiceImpl" => *interchain_token_service_impl_address,
            "InterchainTokenFactoryImpl" => *interchain_token_factory_impl_address,
            _ => None,
        }
    }

    /// Set one of the eight ITS sub-deploy addresses by name.
    pub fn set_its_address(&mut self, name: &str, addr: Address) -> Result<()> {
        let StepKind::DeployIts {
            its_deployer_address,
            token_manager_deployer_address,
            interchain_token_address,
            interchain_token_deployer_address,
            token_manager_address,
            token_handler_address,
            interchain_token_service_impl_address,
            interchain_token_factory_impl_address,
        } = &mut self.kind
        else {
            return Err(eyre::eyre!(
                "cannot set ITS address '{name}' on a non-DeployIts step"
            ));
        };
        let slot = match name {
            "itsDeployer" => its_deployer_address,
            "TokenManagerDeployer" => token_manager_deployer_address,
            "InterchainToken" => interchain_token_address,
            "InterchainTokenDeployer" => interchain_token_deployer_address,
            "TokenManager" => token_manager_address,
            "TokenHandler" => token_handler_address,
            "InterchainTokenServiceImpl" => interchain_token_service_impl_address,
            "InterchainTokenFactoryImpl" => interchain_token_factory_impl_address,
            other => return Err(eyre::eyre!("unknown ITS sub-deploy name '{other}'")),
        };
        *slot = Some(addr);
        Ok(())
    }

    /// Clear all ITS sub-deploy addresses on this step (used when the
    /// deployer key changes and stale predictions need to be discarded).
    pub fn clear_its_helper_addresses(&mut self) -> Result<()> {
        let StepKind::DeployIts {
            token_manager_deployer_address,
            interchain_token_address,
            interchain_token_deployer_address,
            token_manager_address,
            token_handler_address,
            interchain_token_service_impl_address,
            interchain_token_factory_impl_address,
            ..
        } = &mut self.kind
        else {
            return Err(eyre::eyre!(
                "cannot clear ITS helper addresses on a non-DeployIts step"
            ));
        };
        *token_manager_deployer_address = None;
        *interchain_token_address = None;
        *interchain_token_deployer_address = None;
        *token_manager_address = None;
        *token_handler_address = None;
        *interchain_token_service_impl_address = None;
        *interchain_token_factory_impl_address = None;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// The default deployment plan should round-trip through serde without
    /// changes — every variant must be (de)serializable as part of `Step`.
    #[test]
    fn default_steps_round_trip() {
        let original = State {
            axelar_id: ChainKey::new("test-chain"),
            rpc_url: "https://example.com/rpc".into(),
            target_json: PathBuf::from("/tmp/target.json"),
            mnemonic: "abandon abandon abandon".into(),
            env: Network::Testnet,
            cosm_salt: "cosmsalt".into(),
            admin_mnemonic: None,
            deployer_private_key: None,
            gateway_deployer_private_key: None,
            gateway_deployer: None,
            gas_service_deployer_private_key: None,
            its_deployer_private_key: None,
            its_salt: None,
            its_proxy_salt: None,
            predicted_gateway_address: None,
            sender_receiver_address: None,
            proposals: BTreeMap::new(),
            steps: default_steps(),
        };

        let json = serde_json::to_string_pretty(&original).expect("serialize");
        let parsed: State = serde_json::from_str(&json).expect("deserialize");

        // Spot-check structural fidelity
        assert_eq!(parsed.steps.len(), original.steps.len());
        for (a, b) in parsed.steps.iter().zip(original.steps.iter()) {
            assert_eq!(a.name, b.name);
            assert_eq!(a.status, b.status);
        }
        assert_eq!(parsed.env, Network::Testnet);
        assert_eq!(parsed.axelar_id.as_str(), "test-chain");
    }

    /// Step output mutation should hit only the variant-specific field.
    #[test]
    fn step_set_implementation_address() {
        let mut step = Step {
            name: "AxelarGateway".into(),
            status: StepStatus::Pending,
            kind: StepKind::DeployGateway {
                implementation_address: None,
            },
        };
        let addr: Address = "0x49845e5d9985d8dc941462293ed38EEfF18B0eAE"
            .parse()
            .unwrap();
        step.set_implementation_address(addr).unwrap();
        assert_eq!(step.implementation_address(), Some(addr));
    }

    /// `migrate_steps` should append unknown defaults onto an existing list
    /// without disturbing the pre-existing entries.
    #[test]
    fn migrate_appends_only() {
        let mut state = State {
            axelar_id: ChainKey::new("c"),
            rpc_url: "u".into(),
            target_json: PathBuf::from("/x"),
            mnemonic: "m".into(),
            env: Network::Testnet,
            cosm_salt: "s".into(),
            admin_mnemonic: None,
            deployer_private_key: None,
            gateway_deployer_private_key: None,
            gateway_deployer: None,
            gas_service_deployer_private_key: None,
            its_deployer_private_key: None,
            its_salt: None,
            its_proxy_salt: None,
            predicted_gateway_address: None,
            sender_receiver_address: None,
            proposals: BTreeMap::new(),
            steps: vec![Step {
                name: "EvmCompatibilityCheck".into(),
                status: StepStatus::Completed,
                kind: StepKind::EvmCompat,
            }],
        };
        let original_first = state.steps[0].clone();
        migrate_steps(&mut state);
        assert!(state.steps.len() > 1);
        assert_eq!(state.steps[0].name, original_first.name);
        assert_eq!(state.steps[0].status, StepStatus::Completed);
    }
}
