//! Typed wrapper around the `axelar-contract-deployments` chains-config JSON.
//!
//! The on-disk schema (testnet.json, mainnet.json, devnet-amplifier.json,
//! stagenet.json) is sprawling and grows over time; we only type the fields
//! we actually read. Anything we don't enumerate is absorbed by the
//! `extra: HashMap<String, Value>` flattening on each struct, so adding new
//! fields to the deployments repo does not break parsing here.
//!
//! Every field is `Option<T>` because the deployments repo is the source of
//! truth and we don't want a missing entry on one chain to fail-stop the
//! whole load. Callers that *require* a field should `?` it explicitly with
//! a clear error message instead of silently defaulting.

// This module intentionally types more of the deployments JSON schema than
// callers read today. Serde still needs those fields to preserve the shape and
// absorb unknown data via `extra`; runtime callers currently read only a subset
// such as RPC URLs, Axelar IDs, contract addresses, and gas price fields.
// Keeping this allow at module scope is less noisy than annotating every
// schema field that exists for JSON compatibility rather than direct reads.
#![allow(dead_code)]

use std::collections::HashMap;
use std::path::Path;

use eyre::{Result, WrapErr};
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Deserialize)]
pub struct ChainsConfig {
    pub chains: HashMap<String, ChainConfig>,
    pub axelar: AxelarConfig,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

impl ChainsConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let s = std::fs::read_to_string(path)
            .wrap_err_with(|| format!("failed to read chains config '{}'", path.display()))?;
        Self::from_json_str(&s)
            .wrap_err_with(|| format!("failed to parse chains config '{}'", path.display()))
    }

    pub fn from_json_str(s: &str) -> Result<Self> {
        Ok(serde_json::from_str(s)?)
    }
}

/// One entry under the top-level `chains` map (e.g. `chains.hedera`,
/// `chains.solana`). `chain_type` stays a raw `String` because the on-disk
/// set ("evm", "svm", "stellar", "sui", "xrpl") is wider than the
/// `crate::types::ChainType` enum's closed set; callers that only handle
/// evm/svm should `.parse::<ChainType>()` and propagate the error.
#[derive(Debug, Deserialize)]
pub struct ChainConfig {
    #[serde(rename = "axelarId")]
    pub axelar_id: Option<String>,
    pub name: Option<String>,
    pub rpc: Option<String>,
    #[serde(rename = "wssRpc")]
    pub wss_rpc: Option<String>,
    #[serde(rename = "networkType")]
    pub network_type: Option<String>,
    #[serde(rename = "chainType")]
    pub chain_type: Option<String>,
    #[serde(rename = "tokenSymbol")]
    pub token_symbol: Option<String>,
    #[serde(rename = "tokenAddress")]
    pub token_address: Option<String>,
    #[serde(rename = "gasScalingFactor")]
    pub gas_scaling_factor: Option<u32>,
    pub decimals: Option<u8>,
    pub contracts: Option<HashMap<String, ContractEntry>>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

/// Top-level `axelar` block. Holds Axelar-network connection fields plus the
/// nested `contracts` map keyed by contract name then by chain id (e.g.
/// `axelar.contracts.Gateway.hedera.address`). Some inner maps mix chain
/// entries with metadata keys like `lastUploadedCodeId`, so the inner value
/// stays untyped (`HashMap<String, Value>`); see [`AxelarConfig::contract_address`].
#[derive(Debug, Deserialize)]
pub struct AxelarConfig {
    #[serde(rename = "axelarId")]
    pub axelar_id: Option<String>,
    #[serde(rename = "chainId")]
    pub chain_id: Option<String>,
    pub lcd: Option<String>,
    pub rpc: Option<String>,
    pub grpc: Option<String>,
    /// Raw gas-price string with denom suffix, e.g. `"0.007uaxl"`. Use
    /// [`AxelarConfig::parse_gas_price`] to split into the numeric price and
    /// denom.
    #[serde(rename = "gasPrice")]
    pub gas_price: Option<String>,
    #[serde(rename = "tokenSymbol")]
    pub token_symbol: Option<String>,
    #[serde(rename = "unitDenom")]
    pub unit_denom: Option<String>,
    #[serde(rename = "governanceAddress")]
    pub governance_address: Option<String>,
    #[serde(rename = "adminAddress")]
    pub admin_address: Option<String>,
    #[serde(rename = "multisigProverAdminAddress")]
    pub multisig_prover_admin_address: Option<String>,
    /// Stringified base-denom amount (e.g. `"2000000000"`) used as the
    /// deposit when submitting governance proposals on the Axelar hub.
    #[serde(rename = "govProposalDepositAmount")]
    pub gov_proposal_deposit_amount: Option<String>,
    /// Stringified base-denom amount used for expedited governance proposals.
    #[serde(rename = "govProposalExpeditedDepositAmount")]
    pub gov_proposal_expedited_deposit_amount: Option<String>,
    pub contracts: Option<HashMap<String, HashMap<String, Value>>>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

impl AxelarConfig {
    /// Look up `axelar.contracts.<contract>.<chain>.address`. Errors with a
    /// `field not found` message if the contract entry, chain entry, or
    /// address string is missing — callers can `.ok()` to opt back into
    /// `Option` semantics where absence is acceptable.
    pub fn contract_address(&self, contract: &str, chain: &str) -> Result<&str> {
        let opt = (|| -> Option<&str> {
            self.contracts
                .as_ref()?
                .get(contract)?
                .get(chain)?
                .get("address")?
                .as_str()
        })();
        opt.ok_or_else(|| {
            eyre::eyre!("no axelar.contracts.{contract}.{chain}.address in target json")
        })
    }

    /// Look up `axelar.contracts.<contract>.address` — the chain-agnostic
    /// (global) form used by hub-level contracts like AxelarnetGateway,
    /// Router, Multisig, Coordinator, etc., which don't have a per-chain
    /// breakdown at this level.
    pub fn global_contract_address(&self, contract: &str) -> Result<&str> {
        let opt = (|| -> Option<&str> {
            self.contracts
                .as_ref()?
                .get(contract)?
                .get("address")?
                .as_str()
        })();
        opt.ok_or_else(|| eyre::eyre!("no axelar.contracts.{contract}.address in target json"))
    }

    /// Parse the raw `"0.007uaxl"`-shaped `gasPrice` into `(price, denom)`.
    /// Errors if the field is missing or doesn't carry a numeric prefix;
    /// silent defaults are unsafe because the hub binds the denom and price
    /// to specific deployments.
    pub fn parse_gas_price(&self) -> Result<(f64, String)> {
        let raw = self
            .gas_price
            .as_deref()
            .ok_or_else(|| eyre::eyre!("no axelar.gasPrice in target json"))?;
        let split = raw
            .find(|c: char| c.is_alphabetic())
            .ok_or_else(|| eyre::eyre!("axelar.gasPrice '{raw}' has no denom suffix"))?;
        let price: f64 = raw[..split]
            .parse()
            .map_err(|e| eyre::eyre!("axelar.gasPrice '{raw}' numeric prefix invalid: {e}"))?;
        Ok((price, raw[split..].to_string()))
    }

    /// The four fields needed to sign and broadcast a cosmos tx against the
    /// Axelar hub: `(lcd, chain_id, fee_denom, gas_price)`. Errors if any
    /// of them are missing or unparseable. Replaces the old free-standing
    /// `cosmos::read_axelar_config` once all callers have switched to the
    /// typed config.
    pub fn cosmos_tx_params(&self) -> Result<(String, String, String, f64)> {
        let (price, denom) = self.parse_gas_price()?;
        let lcd = self
            .lcd
            .clone()
            .ok_or_else(|| eyre::eyre!("no axelar.lcd in target json"))?;
        let chain_id = self
            .chain_id
            .clone()
            .ok_or_else(|| eyre::eyre!("no axelar.chainId in target json"))?;
        Ok((lcd, chain_id, denom, price))
    }
}

/// One entry under a chain's `contracts` map (e.g.
/// `chains.hedera.contracts.AxelarGateway`). Most callers only read `address`;
/// the rest of the deployer-side fields are kept as `extra` so we don't
/// need to model the full `axelar-contract-deployments` shape here.
#[derive(Debug, Deserialize)]
pub struct ContractEntry {
    pub address: Option<String>,
    pub implementation: Option<String>,
    pub deployer: Option<String>,
    pub salt: Option<String>,
    pub version: Option<String>,
    #[serde(rename = "tokenId")]
    pub token_id: Option<String>,
    #[serde(rename = "typeArgument")]
    pub type_argument: Option<String>,
    pub objects: Option<ContractObjects>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

/// Known named objects recorded inside a deployed contract entry.
///
/// Sui deployments store package-owned object IDs here. Unknown object names
/// remain accepted through `extra`, but business logic can access the objects
/// it understands without traversing `serde_json::Value`.
#[derive(Debug, Deserialize)]
pub struct ContractObjects {
    #[serde(rename = "TokenId")]
    pub token_id: Option<String>,
    #[serde(rename = "GmpChannelId")]
    pub gmp_channel_id: Option<String>,
    #[serde(rename = "ItsChannelId")]
    pub its_channel_id: Option<String>,
    #[serde(rename = "GmpSingleton")]
    pub gmp_singleton: Option<String>,
    #[serde(rename = "ItsSingleton")]
    pub its_singleton: Option<String>,
    #[serde(rename = "InterchainTokenService")]
    pub interchain_token_service: Option<String>,
    #[serde(rename = "Gateway")]
    pub gateway: Option<String>,
    #[serde(rename = "GasService")]
    pub gas_service: Option<String>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

impl ChainsConfig {
    /// Return one typed chain entry by its chains-config key.
    pub fn chain(&self, chain_id: &str) -> Result<&ChainConfig> {
        self.chains
            .get(chain_id)
            .ok_or_else(|| eyre::eyre!("chain '{chain_id}' not found in config"))
    }
}

impl ChainConfig {
    /// Return one typed contract entry.
    pub fn contract(&self, contract: &str, chain_id: &str) -> Result<&ContractEntry> {
        self.contracts
            .as_ref()
            .and_then(|contracts| contracts.get(contract))
            .ok_or_else(|| eyre::eyre!("{contract} not deployed yet for {chain_id}"))
    }

    /// Look up `chains.<this>.contracts.<contract>.address`. Errors with a
    /// `not deployed yet` message if the contract entry or address is
    /// missing — callers can `.ok()` to opt back into `Option` semantics.
    pub fn contract_address(&self, contract: &str, axelar_id: &str) -> Result<&str> {
        let opt =
            (|| -> Option<&str> { self.contracts.as_ref()?.get(contract)?.address.as_deref() })();
        opt.ok_or_else(|| eyre::eyre!("{contract} not deployed yet for {axelar_id}"))
    }

    /// Return this chain's `axelarId`, falling back to the supplied JSON key
    /// when the field is absent. The cosmos-side `axelarId` often differs
    /// from the JSON key (e.g. JSON key `"avalanche"` but
    /// `axelarId: "Avalanche"`); for chains where it isn't set, callers
    /// conventionally reuse the JSON key.
    pub fn axelar_id_or(&self, fallback: &str) -> String {
        self.axelar_id
            .clone()
            .unwrap_or_else(|| fallback.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal fixture covering every field the loader currently types.
    /// Mirrors a sliver of testnet.json without depending on the external
    /// repo at test time.
    const FIXTURE: &str = r#"
    {
      "chains": {
        "hedera": {
          "axelarId": "hedera",
          "name": "Hedera",
          "rpc": "https://testnet.hashio.io/api",
          "wssRpc": "wss://testnet.hashio.io/ws",
          "networkType": "testnet",
          "chainType": "evm",
          "tokenSymbol": "HBAR",
          "tokenAddress": "0x0000000000000000000000000000000000001234",
          "gasScalingFactor": 10,
          "decimals": 18,
          "contracts": {
            "AxelarGateway": {
              "address": "0xe432150cce91c13a887f7D836923d5597adD8E31",
              "salt": "v6.0.4"
            },
            "AXE": {
              "tokenId": "0x0101010101010101010101010101010101010101010101010101010101010101",
              "typeArgument": "0x2::axe::AXE",
              "objects": {
                "TokenId": "0x02",
                "GmpChannelId": "0x03",
                "FutureObject": "0x04"
              }
            }
          },
          "explorer": { "url": "https://hashscan.io/testnet" }
        },
        "solana": {
          "axelarId": "solana",
          "chainType": "svm",
          "rpc": "https://api.devnet.solana.com",
          "tokenSymbol": "SOL",
          "decimals": 9
        }
      },
      "axelar": {
        "axelarId": "axelarnet",
        "chainId": "axelar-testnet-lisbon-3",
        "lcd": "https://lcd-axelar-testnet.imperator.co",
        "gasPrice": "0.007uaxl",
        "tokenSymbol": "AXL",
        "unitDenom": "uaxl",
        "contracts": {
          "Gateway": {
            "hedera": {
              "address": "axelar1w8frw33jn0yx59845wdgk0yru6fxvgr6hlh4xfdtdf08y5jamcnsyu0z6u",
              "version": "1.1.1"
            },
            "lastUploadedCodeId": 24
          }
        }
      },
      "someUnknownTopLevelKey": { "ignored": true }
    }
    "#;

    #[test]
    fn parses_typed_fields() {
        let cfg = ChainsConfig::from_json_str(FIXTURE).expect("fixture parses");

        let hedera = cfg.chains.get("hedera").expect("hedera present");
        assert_eq!(hedera.axelar_id.as_deref(), Some("hedera"));
        assert_eq!(hedera.chain_type.as_deref(), Some("evm"));
        assert_eq!(hedera.token_symbol.as_deref(), Some("HBAR"));
        assert_eq!(
            hedera.wss_rpc.as_deref(),
            Some("wss://testnet.hashio.io/ws")
        );
        assert_eq!(hedera.network_type.as_deref(), Some("testnet"));
        assert_eq!(hedera.gas_scaling_factor, Some(10));
        assert_eq!(hedera.decimals, Some(18));
        assert_eq!(
            hedera.contract_address("AxelarGateway", "hedera").unwrap(),
            "0xe432150cce91c13a887f7D836923d5597adD8E31",
        );
        assert!(hedera.contract_address("AxelarMissing", "hedera").is_err());
        let axe = hedera.contract("AXE", "hedera").unwrap();
        assert_eq!(axe.type_argument.as_deref(), Some("0x2::axe::AXE"));
        let objects = axe.objects.as_ref().unwrap();
        assert_eq!(objects.token_id.as_deref(), Some("0x02"));
        assert_eq!(objects.gmp_channel_id.as_deref(), Some("0x03"));
        assert!(objects.extra.contains_key("FutureObject"));

        let sol = cfg.chains.get("solana").expect("solana present");
        assert_eq!(sol.chain_type.as_deref(), Some("svm"));
        assert!(sol.contracts.is_none());
    }

    #[test]
    fn parses_axelar_block_and_helpers() {
        let cfg = ChainsConfig::from_json_str(FIXTURE).expect("fixture parses");

        assert_eq!(
            cfg.axelar.lcd.as_deref(),
            Some("https://lcd-axelar-testnet.imperator.co")
        );
        assert_eq!(
            cfg.axelar.chain_id.as_deref(),
            Some("axelar-testnet-lisbon-3")
        );

        let (price, denom) = cfg.axelar.parse_gas_price().expect("gas price parses");
        assert!((price - 0.007).abs() < 1e-12);
        assert_eq!(denom, "uaxl");

        let (lcd, chain_id, fee_denom, gas_price) =
            cfg.axelar.cosmos_tx_params().expect("tx params resolve");
        assert_eq!(lcd, "https://lcd-axelar-testnet.imperator.co");
        assert_eq!(chain_id, "axelar-testnet-lisbon-3");
        assert_eq!(fee_denom, "uaxl");
        assert!((gas_price - 0.007).abs() < 1e-12);

        assert_eq!(
            cfg.axelar.contract_address("Gateway", "hedera").unwrap(),
            "axelar1w8frw33jn0yx59845wdgk0yru6fxvgr6hlh4xfdtdf08y5jamcnsyu0z6u",
        );
        // Metadata keys (lastUploadedCodeId) carry no `address` field, so the
        // lookup surfaces as a `field not found` error rather than panicking.
        assert!(
            cfg.axelar
                .contract_address("Gateway", "lastUploadedCodeId")
                .is_err()
        );
    }

    #[test]
    fn unknown_top_level_keys_absorbed() {
        let cfg = ChainsConfig::from_json_str(FIXTURE).expect("fixture parses");
        assert!(cfg.extra.contains_key("someUnknownTopLevelKey"));
    }

    // The real-schema sanity check lives in `config_source::tests::
    // fetches_and_parses_real_testnet_json` (`cargo test -- --ignored`),
    // which fetches testnet.json from GitHub instead of depending on a
    // sibling checkout.
}
