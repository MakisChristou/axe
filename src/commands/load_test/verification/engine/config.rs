//! Typed Axelar configuration for verification.

use std::path::Path;

use eyre::Result;

use crate::config::AxelarChainContract;
use crate::config::AxelarGlobalContract;
use crate::config::ChainsConfig;

/// Axelar config loaded for GMP verification.
pub(super) struct GmpAxelarConfig {
    pub(super) lcd: String,
    pub(super) voting_verifier: Option<String>,
    pub(super) cosm_gateway: String,
}

pub(super) async fn load_gmp_axelar_config(
    config: &Path,
    source_chain: &str,
    destination_chain: &str,
) -> Result<GmpAxelarConfig> {
    let cfg = ChainsConfig::load(config).await?;
    let (lcd, _, _, _) = cfg.axelar.cosmos_tx_params()?;
    let voting_verifier = cfg
        .axelar
        .contract_address(AxelarChainContract::VotingVerifier, source_chain)
        .ok()
        .map(String::from);
    let cosm_gateway = cfg
        .axelar
        .contract_address(AxelarChainContract::Gateway, destination_chain)?
        .to_string();
    Ok(GmpAxelarConfig {
        lcd,
        voting_verifier,
        cosm_gateway,
    })
}

/// Axelar config loaded for ITS-via-hub verification.
///
/// Callers still read the Tendermint RPC separately at the original call
/// site, preserving the ordering relative to transaction construction.
pub(super) struct ItsAxelarConfig {
    pub(super) cfg: ChainsConfig,
    pub(super) lcd: String,
    pub(super) voting_verifier: Option<String>,
    pub(super) axelarnet_gateway: String,
}

pub(super) async fn load_its_axelar_config(
    config: &Path,
    source_chain: &str,
) -> Result<ItsAxelarConfig> {
    let cfg = ChainsConfig::load(config).await?;
    let (lcd, _, _, _) = cfg.axelar.cosmos_tx_params()?;
    let voting_verifier = cfg
        .axelar
        .contract_address(AxelarChainContract::VotingVerifier, source_chain)
        .ok()
        .map(String::from);
    let axelarnet_gateway = cfg
        .axelar
        .global_contract_address(AxelarGlobalContract::AxelarnetGateway)?
        .to_string();
    Ok(ItsAxelarConfig {
        cfg,
        lcd,
        voting_verifier,
        axelarnet_gateway,
    })
}

pub(super) fn lookup_cosm_gateway_dest(
    cfg: &ChainsConfig,
    destination_chain: &str,
) -> Result<String> {
    Ok(cfg
        .axelar
        .contract_address(AxelarChainContract::Gateway, destination_chain)?
        .to_string())
}

/// Look up an XRPL destination Gateway, falling back to the deployment name
/// used by older configurations.
pub(super) fn lookup_xrpl_cosm_gateway_dest(
    cfg: &ChainsConfig,
    destination_chain: &str,
) -> Result<String> {
    Ok(cfg
        .axelar
        .contract_address(AxelarChainContract::Gateway, destination_chain)
        .or_else(|_| {
            cfg.axelar
                .contract_address(AxelarChainContract::XrplGateway, destination_chain)
        })?
        .to_string())
}
