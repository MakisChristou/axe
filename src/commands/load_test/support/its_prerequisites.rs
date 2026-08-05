//! ITS route prerequisite validation.

use eyre::Result;

use crate::config::AxelarChainContract;
use crate::config::AxelarGlobalContract;
use crate::config::ChainsConfig;

#[derive(Clone, Copy)]
pub(super) enum GatewayRequirement<'a> {
    Required,
    AmplifierOnly,
    Xrpl { axelar_id: &'a str },
}

pub(super) fn verify(
    config: &ChainsConfig,
    destination: &str,
    gateway: GatewayRequirement<'_>,
) -> Result<()> {
    let has_gateway = match gateway {
        GatewayRequirement::Required => config
            .axelar
            .contract_address(AxelarChainContract::Gateway, destination)
            .is_ok(),
        GatewayRequirement::AmplifierOnly => {
            config
                .axelar
                .contract_address(AxelarChainContract::VotingVerifier, destination)
                .is_err()
                || config
                    .axelar
                    .contract_address(AxelarChainContract::Gateway, destination)
                    .is_ok()
        }
        GatewayRequirement::Xrpl { axelar_id } => {
            config
                .axelar
                .contract_address(AxelarChainContract::Gateway, destination)
                .is_ok()
                || config
                    .axelar
                    .contract_address(AxelarChainContract::XrplGateway, axelar_id)
                    .is_ok()
        }
    };

    if !has_gateway {
        match gateway {
            GatewayRequirement::Xrpl { .. } => eyre::bail!(
                "destination chain '{destination}' has no Cosmos Gateway (or XrplGateway) in the config — verification would fail."
            ),
            GatewayRequirement::Required | GatewayRequirement::AmplifierOnly => eyre::bail!(
                "destination chain '{destination}' has no Cosmos Gateway in the config — verification would fail."
            ),
        }
    }

    if config
        .axelar
        .global_contract_address(AxelarGlobalContract::AxelarnetGateway)
        .is_err()
    {
        eyre::bail!("no AxelarnetGateway address in config — required for ITS load test");
    }

    Ok(())
}
