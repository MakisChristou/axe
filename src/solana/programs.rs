//! Solana Axelar program IDs and the ITS chain-name hash, per network.
//!
//! Source of truth: the `declare_id!` / `CHAIN_NAME_HASH` cfg blocks in the
//! solana-axelar-{gateway,gas-service,memo,its} crates (crates.io,
//! constants verified identical in v1.0.0 and v1.1.1),
//! cross-checked against the chains-config JSONs in
//! axelar-contract-deployments. axe deliberately does NOT use the crates'
//! `id()` / `find_pda()` / `CHAIN_NAME_HASH` — those are baked per cargo
//! feature at compile time, while axe selects the network at runtime.
//! Update this table when Axelar redeploys a program.

use solana_sdk::pubkey::Pubkey;

use crate::types::Network;

impl Network {
    pub const fn solana_gateway_id(self) -> Pubkey {
        match self {
            Self::Mainnet => Pubkey::from_str_const("gtwqvLL93XK7pC2eMvfGamqokvs19AytzaVhrL2iKiz"),
            Self::Testnet => Pubkey::from_str_const("gtwJ8LWDRWZpbvCqp8sDeTgy3GSyuoEsiaKC8wSXJqq"),
            Self::Stagenet => Pubkey::from_str_const("gtwYHfHHipAoj8Hfp3cGr3vhZ8f3UtptGCQLqjBkaSZ"),
            Self::DevnetAmplifier => {
                Pubkey::from_str_const("gtwT4uGVTYSPnTGv6rSpMheyFyczUicxVWKqdtxNGw9")
            }
        }
    }

    pub const fn solana_gas_service_id(self) -> Pubkey {
        match self {
            Self::Mainnet => Pubkey::from_str_const("gaszjG8797GGm8oACCzH2KLLifGp2nugKkLwaecwBjT"),
            Self::Testnet => Pubkey::from_str_const("gasq7KHHv9Rs8C82hu3dgoBD9wk5LTKpWqbdf5o5juu"),
            Self::Stagenet => Pubkey::from_str_const("gasgy6jz24wrfZL98uMy8QFUFziVPZ3bNLGXqnyTstW"),
            Self::DevnetAmplifier => {
                Pubkey::from_str_const("gasUBnVr9GZon2cp8X5gyFrUsQFhzrprjy734Ci6Bmn")
            }
        }
    }

    pub const fn solana_memo_id(self) -> Pubkey {
        match self {
            Self::Mainnet => Pubkey::from_str_const("memtaCuA26EANM26matPAhiQGPnvsrYdJYrYcK8wcon"),
            Self::Testnet => Pubkey::from_str_const("mem7UJouaeyTgySvXhQSxWtGFrWPQ89jywjc8YvQFRT"),
            Self::Stagenet => Pubkey::from_str_const("mem4E22pPgkbHAvoUYHa7HybBgUKn6jFjvj1YnPdkaq"),
            Self::DevnetAmplifier => {
                Pubkey::from_str_const("memKnP9ex71TveNFpsFNVqAYGEe1v9uHVsHNdFPW6FY")
            }
        }
    }

    pub const fn solana_its_id(self) -> Pubkey {
        match self {
            Self::Mainnet => Pubkey::from_str_const("itsAUdHnV2K99ppbM6d6WUDac8MD54RAE7dUKHnw1Eg"),
            Self::Testnet => Pubkey::from_str_const("itsJo4kNJ3mdh3requwbtTTt7vyYTudp1pxhn2KiHMc"),
            Self::Stagenet => Pubkey::from_str_const("itsm3zZhp2oGgEfq7XBu9ojRCYZJnhzecbAEPCrvx2B"),
            Self::DevnetAmplifier => {
                Pubkey::from_str_const("itsYxmqAxNKUL5zaj3fD1K1whuVhqpxKVoiLGie1reF")
            }
        }
    }

    /// The chain-name hash baked into the network's deployed ITS program
    /// (used for token-id derivation). Copied verbatim from the
    /// solana-axelar-its per-feature constants — these are what the
    /// on-chain programs compute with, so they are the source of truth.
    ///
    /// Note: upstream's testnet and stagenet constants are swapped relative
    /// to their names — the testnet bytes are keccak256("solana-stagenet")
    /// and the stagenet bytes are keccak256("solana-testnet"). Mainnet is
    /// keccak256("solana"), devnet-amplifier is keccak256("solana-devnet").
    pub const fn solana_its_chain_name_hash(self) -> [u8; 32] {
        match self {
            Self::Mainnet => [
                110, 239, 41, 235, 176, 58, 162, 20, 74, 26, 107, 98, 18, 206, 116, 245, 4, 163,
                77, 183, 153, 184, 22, 26, 33, 20, 0, 23, 232, 13, 61, 138,
            ],
            Self::Testnet => [
                159, 1, 245, 195, 103, 184, 207, 215, 88, 74, 183, 125, 33, 47, 221, 82, 55, 77,
                255, 177, 89, 88, 76, 133, 128, 193, 177, 171, 2, 107, 173, 86,
            ],
            Self::Stagenet => [
                67, 5, 100, 18, 3, 83, 80, 76, 10, 94, 7, 166, 63, 92, 244, 200, 233, 32, 8, 242,
                33, 188, 46, 11, 38, 32, 244, 151, 37, 161, 40, 0,
            ],
            Self::DevnetAmplifier => [
                10, 171, 102, 67, 72, 176, 161, 92, 42, 179, 148, 228, 13, 72, 172, 178, 168, 16,
                138, 252, 99, 222, 187, 187, 25, 30, 121, 52, 235, 103, 11, 169,
            ],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The deps stay pinned to the devnet-amplifier feature (see Cargo.toml),
    /// so the crates' baked constants verify the devnet row of every table
    /// against the upstream source of truth.
    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "this guard test intentionally compares runtime IDs with the crates' baked IDs"
    )]
    fn devnet_row_matches_pinned_crate_constants() {
        let n = Network::DevnetAmplifier;
        assert_eq!(n.solana_gateway_id(), solana_axelar_gateway::id());
        assert_eq!(n.solana_gas_service_id(), solana_axelar_gas_service::id());
        assert_eq!(n.solana_memo_id(), solana_axelar_memo::id());
        assert_eq!(n.solana_its_id(), solana_axelar_its::id());
        assert_eq!(
            n.solana_its_chain_name_hash(),
            solana_axelar_its::CHAIN_NAME_HASH
        );
    }

    /// Validates the documented chain-name strings against the hash bytes,
    /// covering the three rows the pinned feature can't check directly.
    ///
    /// The testnet/stagenet names really are crossed — upstream
    /// solana-axelar-its (v1.0.0 and v1.1.1) ships its testnet constant as
    /// keccak256("solana-stagenet") and vice versa, and the deployed
    /// programs were compiled with those constants, so axe must mirror
    /// them to derive the same token IDs the chain does.
    #[test]
    fn chain_name_hashes_match_documented_names() {
        let cases = [
            (Network::Mainnet, "solana"),
            (Network::Testnet, "solana-stagenet"),
            (Network::Stagenet, "solana-testnet"),
            (Network::DevnetAmplifier, "solana-devnet"),
        ];
        for (network, name) in cases {
            assert_eq!(
                network.solana_its_chain_name_hash(),
                solana_sdk::keccak::hash(name.as_bytes()).to_bytes(),
                "chain name hash mismatch for {name}"
            );
        }
    }
}
