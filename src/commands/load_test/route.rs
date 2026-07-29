use eyre::{Result, eyre};

use super::{Protocol, TestType};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SupportedRoute {
    Gmp(GmpRoute),
    Its(ItsRoute),
    ItsWithData(ItsWithDataRoute),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum GmpRoute {
    SolToEvm,
    EvmToSol,
    EvmToEvm,
    SolToSol,
    StellarToEvm,
    EvmToStellar,
    StellarToSol,
    SolToStellar,
    SuiToEvm,
    EvmToSui,
    SolToSui,
    StellarToSui,
    SuiToSol,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ItsRoute {
    StellarToEvm,
    EvmToStellar,
    StellarToSol,
    EvmToSol,
    SolToEvm,
    XrplToEvm,
    EvmToXrpl,
    EvmToEvm,
    EvmToSui,
    SolToSui,
    StellarToSui,
    SuiToEvm,
    SuiToSol,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ItsWithDataRoute {
    EvmToSol,
}

impl SupportedRoute {
    pub(super) fn resolve(
        protocol: Protocol,
        test_type: TestType,
        source_chain: &str,
        destination_chain: &str,
    ) -> Result<Self> {
        let route = match (protocol, test_type) {
            (Protocol::Gmp, TestType::SolToEvm) => Self::Gmp(GmpRoute::SolToEvm),
            (Protocol::Gmp, TestType::EvmToSol) => Self::Gmp(GmpRoute::EvmToSol),
            (Protocol::Gmp, TestType::EvmToEvm) => Self::Gmp(GmpRoute::EvmToEvm),
            (Protocol::Gmp, TestType::SolToSol) => Self::Gmp(GmpRoute::SolToSol),
            (Protocol::Gmp, TestType::StellarToEvm) => Self::Gmp(GmpRoute::StellarToEvm),
            (Protocol::Gmp, TestType::EvmToStellar) => Self::Gmp(GmpRoute::EvmToStellar),
            (Protocol::Gmp, TestType::StellarToSol) => Self::Gmp(GmpRoute::StellarToSol),
            (Protocol::Gmp, TestType::SolToStellar) => Self::Gmp(GmpRoute::SolToStellar),
            (Protocol::Gmp, TestType::SuiToEvm) => Self::Gmp(GmpRoute::SuiToEvm),
            (Protocol::Gmp, TestType::EvmToSui) => Self::Gmp(GmpRoute::EvmToSui),
            (Protocol::Gmp, TestType::SolToSui) => Self::Gmp(GmpRoute::SolToSui),
            (Protocol::Gmp, TestType::StellarToSui) => Self::Gmp(GmpRoute::StellarToSui),
            (Protocol::Gmp, TestType::SuiToSol) => Self::Gmp(GmpRoute::SuiToSol),
            (Protocol::Its, TestType::StellarToEvm) => Self::Its(ItsRoute::StellarToEvm),
            (Protocol::Its, TestType::EvmToStellar) => Self::Its(ItsRoute::EvmToStellar),
            (Protocol::Its, TestType::StellarToSol) => Self::Its(ItsRoute::StellarToSol),
            (Protocol::Its, TestType::EvmToSol) => Self::Its(ItsRoute::EvmToSol),
            (Protocol::Its, TestType::SolToEvm) => Self::Its(ItsRoute::SolToEvm),
            (Protocol::Its, TestType::XrplToEvm) => Self::Its(ItsRoute::XrplToEvm),
            (Protocol::Its, TestType::EvmToXrpl) => Self::Its(ItsRoute::EvmToXrpl),
            (Protocol::Its, TestType::EvmToEvm) => Self::Its(ItsRoute::EvmToEvm),
            (Protocol::Its, TestType::EvmToSui) => Self::Its(ItsRoute::EvmToSui),
            (Protocol::Its, TestType::SolToSui) => Self::Its(ItsRoute::SolToSui),
            (Protocol::Its, TestType::StellarToSui) => Self::Its(ItsRoute::StellarToSui),
            (Protocol::Its, TestType::SuiToEvm) => Self::Its(ItsRoute::SuiToEvm),
            (Protocol::Its, TestType::SuiToSol) => Self::Its(ItsRoute::SuiToSol),
            (Protocol::ItsWithData, TestType::EvmToSol) => {
                Self::ItsWithData(ItsWithDataRoute::EvmToSol)
            }
            (Protocol::Gmp, TestType::XrplToEvm | TestType::EvmToXrpl) => {
                return Err(eyre!(
                    "GMP {source_chain}->{destination_chain} is not yet supported for XRPL. \
                     XRPL has no executable layer, so GMP in either direction is not applicable; \
                     use --protocol its instead."
                ));
            }
            (Protocol::Its, TestType::SolToStellar) => {
                return Err(eyre!(
                    "ITS sol -> stellar is not implemented yet. Use --protocol gmp for this pair."
                ));
            }
            (Protocol::Its, TestType::SolToSol) => {
                return Err(eyre!(
                    "ITS {source_chain}->{destination_chain} is not yet supported"
                ));
            }
            (Protocol::ItsWithData, _) => {
                return Err(eyre!("its-with-data only supports evm-to-sol currently"));
            }
            (_, TestType::XrplToSui) => {
                return Err(eyre!(
                    "xrpl -> sui ITS needs the Sui destination verifier plus a registered AXE/XRP \
                     token on Sui ITS. Not yet implemented."
                ));
            }
            (Protocol::Its, TestType::SuiToStellar | TestType::SuiToXrpl) => {
                return Err(eyre!(
                    "sui -> {destination_chain} ITS not yet wired. Source-side PTB construction is \
                     identical to sui -> evm / sui -> sol, only the destination verifier differs — \
                     follow the its_sui_to_sol.rs pattern \
                     (verify_onchain_solana_its-style swap)."
                ));
            }
            (Protocol::Gmp, TestType::SuiToStellar | TestType::SuiToXrpl) => {
                return Err(eyre!(
                    "sui -> {destination_chain} GMP not implemented yet. Follow the run_sui_to_sol \
                     pattern in gmp.rs (Sui-side PTB stays identical, only destination verification \
                     swaps in)."
                ));
            }
        };
        Ok(route)
    }
}

#[cfg(test)]
mod tests {
    use super::{GmpRoute, ItsRoute, ItsWithDataRoute, SupportedRoute};
    use crate::commands::load_test::{Protocol, TestType};

    #[test]
    fn resolves_supported_protocol_routes() {
        for (protocol, test_type, expected) in [
            (
                Protocol::Gmp,
                TestType::SuiToSol,
                SupportedRoute::Gmp(GmpRoute::SuiToSol),
            ),
            (
                Protocol::Its,
                TestType::XrplToEvm,
                SupportedRoute::Its(ItsRoute::XrplToEvm),
            ),
            (
                Protocol::ItsWithData,
                TestType::EvmToSol,
                SupportedRoute::ItsWithData(ItsWithDataRoute::EvmToSol),
            ),
        ] {
            assert_eq!(
                SupportedRoute::resolve(protocol, test_type, "source", "destination").unwrap(),
                expected
            );
        }
    }

    #[test]
    fn rejects_unsupported_protocol_routes_at_the_boundary() {
        for (protocol, test_type, expected) in [
            (
                Protocol::Gmp,
                TestType::XrplToEvm,
                "GMP source->destination is not yet supported for XRPL",
            ),
            (
                Protocol::Its,
                TestType::SolToStellar,
                "ITS sol -> stellar is not implemented yet",
            ),
            (
                Protocol::ItsWithData,
                TestType::SuiToEvm,
                "its-with-data only supports evm-to-sol currently",
            ),
        ] {
            let error =
                SupportedRoute::resolve(protocol, test_type, "source", "destination").unwrap_err();
            assert!(error.to_string().contains(expected));
        }
    }

    #[test]
    fn route_contract_covers_the_full_protocol_pair_matrix() {
        let protocols = [Protocol::Gmp, Protocol::Its, Protocol::ItsWithData];
        let test_types = [
            TestType::SolToEvm,
            TestType::EvmToSol,
            TestType::EvmToEvm,
            TestType::SolToSol,
            TestType::XrplToEvm,
            TestType::EvmToXrpl,
            TestType::StellarToEvm,
            TestType::EvmToStellar,
            TestType::StellarToSol,
            TestType::SolToStellar,
            TestType::SuiToEvm,
            TestType::EvmToSui,
            TestType::SuiToSol,
            TestType::SolToSui,
            TestType::SuiToStellar,
            TestType::StellarToSui,
            TestType::SuiToXrpl,
            TestType::XrplToSui,
        ];
        let expected_supported = [
            (Protocol::Gmp, TestType::SolToEvm),
            (Protocol::Gmp, TestType::EvmToSol),
            (Protocol::Gmp, TestType::EvmToEvm),
            (Protocol::Gmp, TestType::SolToSol),
            (Protocol::Gmp, TestType::StellarToEvm),
            (Protocol::Gmp, TestType::EvmToStellar),
            (Protocol::Gmp, TestType::StellarToSol),
            (Protocol::Gmp, TestType::SolToStellar),
            (Protocol::Gmp, TestType::SuiToEvm),
            (Protocol::Gmp, TestType::EvmToSui),
            (Protocol::Gmp, TestType::SuiToSol),
            (Protocol::Gmp, TestType::SolToSui),
            (Protocol::Gmp, TestType::StellarToSui),
            (Protocol::Its, TestType::SolToEvm),
            (Protocol::Its, TestType::EvmToSol),
            (Protocol::Its, TestType::EvmToEvm),
            (Protocol::Its, TestType::XrplToEvm),
            (Protocol::Its, TestType::EvmToXrpl),
            (Protocol::Its, TestType::StellarToEvm),
            (Protocol::Its, TestType::EvmToStellar),
            (Protocol::Its, TestType::StellarToSol),
            (Protocol::Its, TestType::SuiToEvm),
            (Protocol::Its, TestType::EvmToSui),
            (Protocol::Its, TestType::SuiToSol),
            (Protocol::Its, TestType::SolToSui),
            (Protocol::Its, TestType::StellarToSui),
            (Protocol::ItsWithData, TestType::EvmToSol),
        ];

        let mut actual_supported = Vec::new();
        let mut checked = 0;
        for protocol in protocols {
            for test_type in test_types {
                checked += 1;
                if SupportedRoute::resolve(protocol, test_type, "source", "destination").is_ok() {
                    actual_supported.push((protocol, test_type));
                }
            }
        }

        assert_eq!(checked, 54);
        assert_eq!(actual_supported, expected_supported);
    }
}
