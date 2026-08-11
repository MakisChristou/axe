//! `axe deploy sender-receiver` — deploy-once provisioning of the GMP
//! SenderReceiver helper for one EVM chain. Delegates to the load-test
//! deploy path, so the axe-tokens overlay check, on-chain verification,
//! paris-bytecode probe, fee-mode detection, and the pending-state
//! degrade all apply. A fresh deploy prints the overlay line to record.

use std::path::PathBuf;

use alloy::primitives::Address;
use eyre::{Result, eyre};

use crate::commands::load_test::{
    detect_network_from_config, ensure_sender_receiver_on_evm_chain, set_cache_network,
};
use crate::config::{ChainContract, ChainsConfig};
use crate::types::Network;
use crate::ui;

pub async fn run(
    config: PathBuf,
    chain: String,
    rpc: Option<String>,
    private_key: String,
    network: Option<Network>,
) -> Result<()> {
    let network = match network {
        Some(network) => network,
        None => detect_network_from_config(&config).ok_or_else(|| {
            eyre!(
                "cannot infer network from {}; pass --network",
                config.display()
            )
        })?,
    };
    // Scope the overlay lookup + cache files to the target network.
    set_cache_network(network);

    let cfg = ChainsConfig::load(&config).await?;
    let chain_cfg = cfg
        .chains
        .get(&chain)
        .ok_or_else(|| eyre!("chain '{chain}' not found in {}", config.display()))?;
    let gateway: Address = chain_cfg
        .contract_address(ChainContract::AxelarGateway, &chain)?
        .parse()?;
    let gas_service: Address = chain_cfg
        .contract_address(ChainContract::AxelarGasService, &chain)?
        .parse()?;
    let rpc_url = match rpc {
        Some(url) => url,
        None => chain_cfg
            .rpc
            .clone()
            .ok_or_else(|| eyre!("chain '{chain}' has no rpc in config; pass --rpc"))?,
    };

    ui::kv("network", &network.to_string());
    ui::kv("chain", &chain);
    ui::address("gateway", &format!("{gateway}"));
    ui::address("gas service", &format!("{gas_service}"));
    let addr =
        ensure_sender_receiver_on_evm_chain(&chain, &rpc_url, &private_key, gateway, gas_service)
            .await?;
    // Machine-readable line for scripted harvesting.
    println!("SENDER_RECEIVER {network} {chain} {addr}");
    Ok(())
}
