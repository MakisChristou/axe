use std::collections::{HashMap, HashSet};

use alloy::primitives::{Address, address};
use eyre::{Result, eyre};
use serde::Serialize;

use super::super::types::{CatalogResponse, WalletAsset, format_units};
use crate::types::Network;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SolverDeployment {
    Devnet,
    Testnet,
    Mainnet,
}

impl SolverDeployment {
    pub const fn address(self) -> Address {
        match self {
            Self::Devnet => address!("509955a5cb4E1D80D3927E0cdA3Dc21a0C1d0141"),
            Self::Testnet => address!("24c5ffcaba6490556a7cf9ec73588d78ef9cce47"),
            Self::Mainnet => address!("Db291eF29c66A0A5bA35EB521fe8D52d3Ab5898c"),
        }
    }
}

impl TryFrom<Network> for SolverDeployment {
    type Error = eyre::Report;

    fn try_from(network: Network) -> Result<Self> {
        match network {
            Network::DevnetAmplifier => Ok(Self::Devnet),
            Network::Testnet => Ok(Self::Testnet),
            Network::Mainnet => Ok(Self::Mainnet),
            Network::Stagenet => Err(eyre!(
                "Axelar RFQ has no stagenet solver; use devnet-amplifier, testnet, or mainnet"
            )),
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InventoryReport {
    pub network: Network,
    pub solver_address: String,
    pub price_source: &'static str,
    pub known_value_usd: f64,
    pub valued_assets: usize,
    pub readable_assets: usize,
    pub total_assets: usize,
    pub chains: Vec<InventoryChain>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InventoryChain {
    pub chain_id: String,
    pub chain_label: String,
    pub chain_type: String,
    pub rpc_available: bool,
    pub known_value_usd: f64,
    pub tokens: Vec<InventoryToken>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InventoryToken {
    pub address: String,
    pub symbol: String,
    pub decimals: u8,
    pub balance: Option<String>,
    pub price_usd: Option<f64>,
    pub value_usd: Option<f64>,
}

pub struct InventoryInputs {
    pub network: Network,
    pub solver_address: Address,
    pub catalog: CatalogResponse,
    pub available_chain_ids: HashSet<String>,
    pub assets: Vec<WalletAsset>,
    pub price_source: &'static str,
    pub prices: HashMap<String, f64>,
}

impl InventoryReport {
    pub fn build(inputs: InventoryInputs) -> Self {
        let balances = inputs
            .assets
            .iter()
            .map(|asset| {
                (
                    (
                        asset.id.chain_id.clone(),
                        asset.id.token_address.to_ascii_lowercase(),
                    ),
                    asset,
                )
            })
            .collect::<HashMap<_, _>>();
        let chains = inputs
            .catalog
            .chains
            .into_iter()
            .map(|entry| {
                let tokens = entry
                    .tokens
                    .into_iter()
                    .map(|token| {
                        let asset = balances.get(&(
                            entry.chain.chain_id.clone(),
                            token.address.to_ascii_lowercase(),
                        ));
                        let balance =
                            asset.map(|asset| format_units(asset.balance, token.decimals));
                        let price_usd = inputs
                            .prices
                            .get(&token.symbol.to_ascii_uppercase())
                            .copied();
                        let value_usd = asset
                            .and_then(|asset| {
                                format_units(asset.balance, token.decimals)
                                    .parse::<f64>()
                                    .ok()
                            })
                            .zip(price_usd)
                            .map(|(balance, price)| balance * price);
                        InventoryToken {
                            address: token.address,
                            symbol: token.symbol,
                            decimals: token.decimals,
                            balance,
                            price_usd,
                            value_usd,
                        }
                    })
                    .collect::<Vec<_>>();
                InventoryChain {
                    chain_id: entry.chain.chain_id.clone(),
                    chain_label: entry.chain.chain_label,
                    chain_type: entry.chain.chain_type,
                    rpc_available: inputs.available_chain_ids.contains(&entry.chain.chain_id),
                    known_value_usd: tokens
                        .iter()
                        .filter_map(|token| token.value_usd)
                        .fold(0.0, |total, value| total + value),
                    tokens,
                }
            })
            .collect::<Vec<_>>();
        Self {
            network: inputs.network,
            solver_address: inputs.solver_address.to_string(),
            price_source: inputs.price_source,
            known_value_usd: chains
                .iter()
                .map(|chain| chain.known_value_usd)
                .fold(0.0, |total, value| total + value),
            valued_assets: chains
                .iter()
                .flat_map(|chain| &chain.tokens)
                .filter(|token| token.value_usd.is_some())
                .count(),
            readable_assets: chains
                .iter()
                .flat_map(|chain| &chain.tokens)
                .filter(|token| token.balance.is_some())
                .count(),
            total_assets: chains.iter().map(|chain| chain.tokens.len()).sum(),
            chains,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_each_rfq_network_to_its_solver() {
        assert_eq!(
            SolverDeployment::try_from(Network::DevnetAmplifier)
                .unwrap()
                .address()
                .to_string(),
            "0x509955a5cb4E1D80D3927E0cdA3Dc21a0C1d0141"
        );
        assert_eq!(
            SolverDeployment::try_from(Network::Testnet)
                .unwrap()
                .address()
                .to_string(),
            "0x24c5ffcAbA6490556a7CF9eC73588d78ef9CCE47"
        );
        assert_eq!(
            SolverDeployment::try_from(Network::Mainnet)
                .unwrap()
                .address()
                .to_string(),
            "0xDb291eF29c66A0A5bA35EB521fe8D52d3Ab5898c"
        );
        assert!(SolverDeployment::try_from(Network::Stagenet).is_err());
    }
}
