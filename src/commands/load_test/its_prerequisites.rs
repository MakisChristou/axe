use eyre::Result;

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
            .contract_address("Gateway", destination)
            .is_ok(),
        GatewayRequirement::AmplifierOnly => {
            config
                .axelar
                .contract_address("VotingVerifier", destination)
                .is_err()
                || config
                    .axelar
                    .contract_address("Gateway", destination)
                    .is_ok()
        }
        GatewayRequirement::Xrpl { axelar_id } => {
            config
                .axelar
                .contract_address("Gateway", destination)
                .is_ok()
                || config
                    .axelar
                    .contract_address("XrplGateway", axelar_id)
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
        .global_contract_address("AxelarnetGateway")
        .is_err()
    {
        eyre::bail!("no AxelarnetGateway address in config — required for ITS load test");
    }

    Ok(())
}
