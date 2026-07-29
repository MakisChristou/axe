//! Sui chains-config readers: pull RPC URL, the Example/AxelarGateway/
//! GasService object IDs, and the AxelarGateway Move-package address out of
//! the JSON config the load test ships with.

use eyre::{Result, eyre};
use sui_sdk_types::Address as SuiAddress;

use crate::config::ChainsConfig;

#[derive(Debug, Clone)]
pub struct SuiContractsConfig {
    pub example_pkg: SuiAddress,
    pub gmp_singleton: SuiAddress,
    pub gateway_object: SuiAddress,
    pub gas_service_object: SuiAddress,
}

/// Read Sui chain config (RPC + key contract object IDs) from the chains config JSON.
pub fn read_sui_chain_config(
    config: &std::path::Path,
    chain_id: &str,
) -> Result<(String, SuiContractsConfig)> {
    let config = ChainsConfig::load(config)?;
    let chain = config.chain(chain_id)?;
    let rpc = chain
        .rpc
        .as_ref()
        .ok_or_else(|| eyre!("no rpc for sui chain '{chain_id}'"))?
        .clone();

    let example = chain.contract("Example", chain_id)?;
    let example_pkg = example
        .address
        .as_deref()
        .ok_or_else(|| eyre!("no Example.address for '{chain_id}'"))?;
    let gmp_singleton = example
        .objects
        .as_ref()
        .and_then(|objects| objects.gmp_singleton.as_deref())
        .ok_or_else(|| eyre!("no Example.objects.GmpSingleton for '{chain_id}'"))?;
    let gateway_object = chain
        .contract("AxelarGateway", chain_id)?
        .objects
        .as_ref()
        .and_then(|objects| objects.gateway.as_deref())
        .ok_or_else(|| eyre!("no AxelarGateway.objects.Gateway for '{chain_id}'"))?;
    let gas_service_object = chain
        .contract("GasService", chain_id)?
        .objects
        .as_ref()
        .and_then(|objects| objects.gas_service.as_deref())
        .ok_or_else(|| eyre!("no GasService.objects.GasService for '{chain_id}'"))?;

    Ok((
        rpc,
        SuiContractsConfig {
            example_pkg: parse_sui_addr(example_pkg)?,
            gmp_singleton: parse_sui_addr(gmp_singleton)?,
            gateway_object: parse_sui_addr(gateway_object)?,
            gas_service_object: parse_sui_addr(gas_service_object)?,
        },
    ))
}

pub fn parse_sui_addr(s: &str) -> Result<SuiAddress> {
    SuiAddress::from_hex(s).map_err(|e| eyre!("Sui address parse '{s}': {e:?}"))
}

/// Read the AxelarGateway Move-package address for a Sui chain. Used by the
/// destination-side verifier to construct event-type strings for
/// `events::MessageApproved` / `events::MessageExecuted`.
pub fn read_sui_gateway_pkg(config: &std::path::Path, chain_id: &str) -> Result<String> {
    let config = ChainsConfig::load(config)?;
    Ok(config
        .chain(chain_id)?
        .contract_address("AxelarGateway", chain_id)?
        .to_string())
}
