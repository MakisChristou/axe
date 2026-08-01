/// A known contract under one chain's `contracts` map.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChainContract {
    Axe,
    AxelarExample,
    AxelarGasService,
    AxelarGateway,
    AxelarServiceGovernance,
    ConstAddressDeployer,
    Create3Deployer,
    Example,
    GasService,
    InterchainTokenFactory,
    InterchainTokenService,
    Operators,
}

impl ChainContract {
    pub const fn key(self) -> &'static str {
        match self {
            Self::Axe => "AXE",
            Self::AxelarExample => "AxelarExample",
            Self::AxelarGasService => "AxelarGasService",
            Self::AxelarGateway => "AxelarGateway",
            Self::AxelarServiceGovernance => "AxelarServiceGovernance",
            Self::ConstAddressDeployer => "ConstAddressDeployer",
            Self::Create3Deployer => "Create3Deployer",
            Self::Example => "Example",
            Self::GasService => "GasService",
            Self::InterchainTokenFactory => "InterchainTokenFactory",
            Self::InterchainTokenService => "InterchainTokenService",
            Self::Operators => "Operators",
        }
    }
}

/// A known per-chain contract under the `axelar` config.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AxelarChainContract {
    Gateway,
    InterchainTokenService,
    MultisigProver,
    VotingVerifier,
    XrplGateway,
    XrplVotingVerifier,
}

impl AxelarChainContract {
    pub const fn key(self) -> &'static str {
        match self {
            Self::Gateway => "Gateway",
            Self::InterchainTokenService => "InterchainTokenService",
            Self::MultisigProver => "MultisigProver",
            Self::VotingVerifier => "VotingVerifier",
            Self::XrplGateway => "XrplGateway",
            Self::XrplVotingVerifier => "XrplVotingVerifier",
        }
    }
}

/// A known global contract under the `axelar` config.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AxelarGlobalContract {
    AxelarnetGateway,
    InterchainTokenService,
    Router,
}

impl AxelarGlobalContract {
    pub const fn key(self) -> &'static str {
        match self {
            Self::AxelarnetGateway => "AxelarnetGateway",
            Self::InterchainTokenService => "InterchainTokenService",
            Self::Router => "Router",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AxelarChainContract, AxelarGlobalContract, ChainContract};

    #[test]
    fn contract_keys_match_deployment_config_names() {
        assert_eq!(ChainContract::AxelarGateway.key(), "AxelarGateway");
        assert_eq!(AxelarChainContract::VotingVerifier.key(), "VotingVerifier");
        assert_eq!(
            AxelarGlobalContract::AxelarnetGateway.key(),
            "AxelarnetGateway"
        );
    }
}
