//! Pure data lookups: program IDs, instruction discriminators, account-role
//! labels, anchor event discriminators. No I/O, no formatting — just tables.

use anchor_lang::Discriminator;
use solana_sdk::pubkey::Pubkey;
use std::collections::HashMap;

use crate::types::Network;

// ---------------------------------------------------------------------------
// Known program IDs (all networks, resolved at runtime)
// ---------------------------------------------------------------------------

pub fn known_programs() -> HashMap<Pubkey, &'static str> {
    let mut m = HashMap::new();

    // Every network's current Axelar program IDs, derived from the canonical
    // per-network table in `crate::solana::programs` — decode works on any
    // network without rebuilding.
    for network in Network::ALL {
        m.insert(network.solana_gateway_id(), "AxelarGateway");
        m.insert(network.solana_gas_service_id(), "AxelarGasService");
        m.insert(network.solana_memo_id(), "AxelarMemo");
        m.insert(network.solana_its_id(), "AxelarITS");
    }

    let all_ids: &[(&str, &str)] = &[
        // Legacy devnet deploys (kept so old transactions still decode)
        (
            "gasHyxjNZSNsEiMbRLa5JGLCNx1TRsdCy1xwfMBehYB",
            "AxelarGasService",
        ),
        // Well-known system programs
        ("11111111111111111111111111111111", "SystemProgram"),
        (
            "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
            "TokenProgram",
        ),
        ("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb", "Token2022"),
        (
            "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL",
            "AssociatedToken",
        ),
        (
            "metaqbxxUerdq28cj1RbAWkYQm3ybzjb6a8bt518x1s",
            "MetaplexMetadata",
        ),
        (
            "Sysvar1nstructions1111111111111111111111111",
            "SysvarInstructions",
        ),
        (
            "ComputeBudget111111111111111111111111111111",
            "ComputeBudget",
        ),
        ("SysvarRent111111111111111111111111111111111", "SysvarRent"),
    ];

    for (addr, name) in all_ids {
        if let Ok(pk) = addr.parse() {
            m.entry(pk).or_insert(name);
        }
    }

    m
}

// ---------------------------------------------------------------------------
// Anchor instruction discriminators (sha256("global:<name>")[0:8])
// ---------------------------------------------------------------------------

pub fn instruction_name(discriminator: &[u8]) -> Option<&'static str> {
    if discriminator.len() < 8 {
        return None;
    }
    match discriminator[..8] {
        // Gateway
        [0xb1, 0x96, 0x55, 0x82, 0x81, 0x5c, 0xbc, 0xd3] => Some("CallContract"),
        [0xd0, 0x7f, 0x15, 0x01, 0xc2, 0xbe, 0xc4, 0x46] => Some("InitializeConfig"),
        [0x88, 0xc9, 0xf1, 0x4a, 0x08, 0xed, 0x3f, 0xe7] => {
            Some("InitializePayloadVerificationSession")
        }
        [0x5b, 0x8b, 0x18, 0x45, 0xfb, 0xa2, 0xf5, 0x70] => Some("VerifySignature"),
        [0x41, 0x9a, 0x84, 0x87, 0x69, 0x05, 0xad, 0x15] => Some("ApproveMessage"),
        [0xed, 0xe5, 0xc8, 0xc1, 0x07, 0xe5, 0xd4, 0x7f] => Some("ValidateMessage"),
        [0x7a, 0xc4, 0xe7, 0x9f, 0xa3, 0x18, 0xcf, 0xa6] => Some("RotateSigners"),
        [0x11, 0xee, 0x56, 0xd0, 0xe9, 0x7a, 0xc3, 0xba] => Some("TransferOperatorship"),
        // GasService
        [0x4e, 0x8c, 0xae, 0x08, 0xbc, 0xe8, 0xef, 0x03] => Some("PayGas"),
        // Memo
        [0xce, 0xb2, 0x4f, 0x13, 0x3f, 0xd2, 0x48, 0xef] => Some("SendMemo"),
        // ITS
        [0xd0, 0xc6, 0xf5, 0x3c, 0x87, 0xe4, 0x64, 0xdc] => Some("InterchainTransfer"),
        [0xcc, 0x91, 0x2f, 0x06, 0x85, 0x07, 0x51, 0xc8] => Some("DeployInterchainToken"),
        [0xdf, 0x5b, 0x11, 0x99, 0x5f, 0xec, 0x7e, 0xc6] => Some("DeployRemoteInterchainToken"),
        [0x82, 0xdd, 0xf2, 0x9a, 0x0d, 0xc1, 0xbd, 0x1d] => Some("Execute"),
        [0x36, 0xa7, 0x65, 0x49, 0xb0, 0xbd, 0x78, 0x41] => Some("ExecuteInterchainTransfer"),
        [0xf3, 0x9b, 0x31, 0xbd, 0x5e, 0xea, 0xb2, 0x69] => Some("ExecuteDeployInterchainToken"),
        _ => None,
    }
}

fn gateway_account_labels(ix_name: &str) -> Option<&'static [&'static str]> {
    match ix_name {
        "CallContract" => Some(&[
            "sender",
            "sender",
            "gateway_root_pda",
            "event_authority",
            "gateway_program",
        ]),
        "InitializePayloadVerificationSession" => Some(&[
            "payer",
            "gateway_root_pda",
            "verification_session",
            "verifier_set_tracker",
            "system_program",
        ]),
        "VerifySignature" => Some(&[
            "gateway_root_pda",
            "verification_session",
            "verifier_set_tracker",
        ]),
        "ApproveMessage" => Some(&[
            "gateway_root_pda",
            "funder",
            "verification_session",
            "incoming_message",
            "system_program",
            "event_authority",
            "gateway_program",
        ]),
        "ValidateMessage" => Some(&[
            "incoming_message_pda",
            "caller",
            "gateway_root_pda",
            "event_authority",
            "gateway_program",
        ]),
        "RotateSigners" => Some(&[
            "payer",
            "gateway_root_pda",
            "verification_session",
            "new_verifier_set_tracker",
            "system_program",
            "event_authority",
            "gateway_program",
        ]),
        "PayGas" => Some(&[
            "sender",
            "gas_config_pda",
            "system_program",
            "event_authority",
            "gas_service_program",
        ]),
        "SendMemo" => Some(&[
            "memo_program",
            "sender",
            "gateway_root_pda",
            "event_authority",
            "gateway_program",
        ]),
        _ => None,
    }
}

fn its_transfer_account_labels(ix_name: &str) -> Option<&'static [&'static str]> {
    match ix_name {
        "InterchainTransfer" => Some(&[
            "payer",
            "authority",
            "gateway_root_pda",
            "gateway_event_authority",
            "gateway_program",
            "call_contract_signing_pda",
            "gas_treasury",
            "gas_service",
            "gas_event_authority",
            "its_root_pda",
            "token_manager_pda",
            "token_program",
            "token_mint",
            "authority_token_account",
            "token_manager_ata",
            "system_program",
            "its_event_authority",
            "its_program",
        ]),
        "Execute" => Some(&[
            "incoming_message_pda",
            "signing_pda",
            "gateway_root_pda",
            "gateway_event_authority",
            "gateway_program",
            "payer",
            "its_root_pda",
            "token_manager_pda",
            "token_mint",
            "token_manager_ata",
            "token_program",
            "associated_token_program",
            "system_program",
            "its_event_authority",
            "its_program",
            // remaining_accounts for deploy/transfer
            "sysvar_instructions",
            "mpl_token_metadata_program",
            "mpl_token_metadata_account",
        ]),
        // CPI accounts are reordered by Anchor's to_account_metas
        // (writable signers, readonly signers, writable, readonly)
        "ExecuteDeployInterchainToken" => Some(&[
            "payer",
            "system_program",
            "its_root_pda",
            "token_manager_pda",
            "token_mint",
            "token_manager_ata",
            "token_program",
            "associated_token_program",
            "sysvar_instructions",
            "mpl_token_metadata_program",
            "mpl_token_metadata_account",
            "its_event_authority",
            "its_program",
            "minter",
            "minter_roles_pda",
        ]),
        "ExecuteInterchainTransfer" => Some(&[
            "payer",
            "its_root_pda",
            "destination",
            "destination_ata",
            "token_mint",
            "token_manager_pda",
            "token_manager_ata",
            "token_program",
            "associated_token_program",
            "system_program",
            "its_program",
            "its_event_authority",
            "its_program",
        ]),
        _ => None,
    }
}

fn its_deploy_account_labels(ix_name: &str) -> &'static [&'static str] {
    match ix_name {
        "DeployInterchainToken" => &[
            "payer",
            "deployer",
            "system_program",
            "its_root_pda",
            "token_manager_pda",
            "token_mint",
            "token_manager_ata",
            "token_program",
            "associated_token_program",
            "sysvar_instructions",
            "mpl_token_metadata_program",
            "mpl_token_metadata_account",
            "deployer_ata",
            "minter",
            "minter_roles_pda",
        ],
        "DeployRemoteInterchainToken" => &[
            "payer",
            "deployer",
            "token_mint",
            "metadata_account",
            "token_manager_pda",
            "gateway_root_pda",
            "gateway_program",
            "system_program",
            "its_root_pda",
            "call_contract_signing_pda",
            "gateway_event_authority",
            "gas_treasury",
            "gas_service",
            "gas_event_authority",
        ],
        _ => &[],
    }
}

/// Return human-readable account role labels for known instructions.
pub(super) fn account_labels(ix_name: &str) -> &'static [&'static str] {
    gateway_account_labels(ix_name)
        .or_else(|| its_transfer_account_labels(ix_name))
        .unwrap_or_else(|| its_deploy_account_labels(ix_name))
}

// ---------------------------------------------------------------------------
// Anchor event discriminators
//
// Anchor CPI events are prefixed with EVENT_IX_TAG_LE (8 bytes) then the
// event discriminator (8 bytes), then borsh-encoded event data. Both the
// tag and each event's discriminator come from anchor / the imported
// solana-axelar-* crates so we don't hand-maintain bytes that can drift
// from what the on-chain programs actually emit.
// ---------------------------------------------------------------------------

pub use anchor_lang::event::EVENT_IX_TAG_LE;

pub fn event_name(discriminator: &[u8]) -> Option<&'static str> {
    if discriminator.len() < 8 {
        return None;
    }
    let disc = &discriminator[..8];

    macro_rules! match_event {
        ($($ty:ty),* $(,)?) => {
            $(
                if disc == <$ty as Discriminator>::DISCRIMINATOR {
                    return Some(stringify!($ty).rsplit("::").next().unwrap_or(stringify!($ty)));
                }
            )*
        };
    }

    match_event!(
        // Gateway
        solana_axelar_gateway::events::CallContractEvent,
        solana_axelar_gateway::events::MessageApprovedEvent,
        solana_axelar_gateway::events::MessageExecutedEvent,
        solana_axelar_gateway::events::VerifierSetRotatedEvent,
        // GasService
        solana_axelar_gas_service::events::GasPaidEvent,
        solana_axelar_gas_service::events::GasAddedEvent,
        solana_axelar_gas_service::events::GasRefundedEvent,
        solana_axelar_gas_service::events::GasCollectedEvent,
        // ITS — note: crate types omit the `Event` suffix the gateway/gas
        // crates use, so labels here will read e.g. `InterchainTokenDeployed`
        // (truthful to the emitting struct).
        solana_axelar_its::events::InterchainTransferSent,
        solana_axelar_its::events::InterchainTransferReceived,
        solana_axelar_its::events::InterchainTokenDeployed,
        solana_axelar_its::events::InterchainTokenDeploymentStarted,
        solana_axelar_its::events::TokenManagerDeployed,
        solana_axelar_its::events::TokenMetadataRegistered,
        solana_axelar_its::events::LinkTokenStarted,
    );
    None
}

#[cfg(test)]
mod tests {
    use super::event_name;

    /// Bytes captured from real on-chain events in prior `axe decode tx` runs.
    /// If the imported solana-axelar-* crates ever rename or replace these
    /// event types, the discriminator returned by their `Discriminator` impl
    /// won't match the bytes the deployed programs emit, and this test will
    /// fail with the offending event — preventing the inspector from silently
    /// labelling on-chain events as `UnknownEvent`.
    #[test]
    fn known_on_chain_bytes_resolve() {
        let cases: &[(&[u8], &str)] = &[
            // Gateway
            (
                &[0xd3, 0xd3, 0x50, 0x7e, 0x96, 0x62, 0xb5, 0xc6],
                "CallContractEvent",
            ),
            (
                &[0xfa, 0xfe, 0x1d, 0xe3, 0x9f, 0xcd, 0x72, 0x59],
                "MessageApprovedEvent",
            ),
            (
                &[0x09, 0x9d, 0xbc, 0xe1, 0xa8, 0x1a, 0x5e, 0x52],
                "MessageExecutedEvent",
            ),
            (
                &[0x36, 0x4f, 0x98, 0x9b, 0x8a, 0x44, 0xe5, 0x60],
                "VerifierSetRotatedEvent",
            ),
            // GasService
            (
                &[0xbf, 0xa1, 0x16, 0xab, 0x29, 0x20, 0xd4, 0xf8],
                "GasPaidEvent",
            ),
            (
                &[0x43, 0x61, 0xf5, 0x20, 0xc3, 0xb4, 0x4a, 0x6d],
                "GasAddedEvent",
            ),
            (
                &[0xea, 0xd0, 0x71, 0x56, 0x5d, 0x7b, 0xc8, 0x0c],
                "GasRefundedEvent",
            ),
            (
                &[0x29, 0x99, 0xc7, 0x9f, 0x73, 0x4b, 0x4c, 0xb8],
                "GasCollectedEvent",
            ),
            // ITS — labels match crate struct names (no `Event` suffix).
            (
                &[0x60, 0x42, 0x01, 0x44, 0xf0, 0x34, 0x90, 0x8a],
                "InterchainTransferSent",
            ),
            (
                &[0xaf, 0xc9, 0xb2, 0x8b, 0x99, 0x45, 0x01, 0xd0],
                "InterchainTransferReceived",
            ),
            (
                &[0xf9, 0x5a, 0x7c, 0x8e, 0x42, 0x2a, 0x5c, 0xbc],
                "InterchainTokenDeployed",
            ),
            (
                &[0x91, 0x4a, 0xc7, 0xba, 0xd2, 0xe8, 0x93, 0x01],
                "InterchainTokenDeploymentStarted",
            ),
            (
                &[0x03, 0x8a, 0x01, 0x9b, 0x81, 0x57, 0x00, 0x29],
                "TokenManagerDeployed",
            ),
            (
                &[0x1b, 0x1f, 0xbd, 0xfb, 0xb7, 0x29, 0x08, 0x7c],
                "TokenMetadataRegistered",
            ),
            (
                &[0xef, 0x48, 0x83, 0xb5, 0xfb, 0x01, 0xde, 0x82],
                "LinkTokenStarted",
            ),
        ];
        for (bytes, expected) in cases {
            assert_eq!(
                event_name(bytes),
                Some(*expected),
                "discriminator drift for {expected}: bytes {bytes:02x?} no longer resolve"
            );
        }
    }
}
