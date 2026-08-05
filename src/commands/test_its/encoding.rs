//! ITS payload encoders. Two flavours: borsh (Solana → hub) and abi
//! (EVM ITS proxy + the hub envelope). Each function is a pure transform
//! with no I/O, so the unit-test surface is just byte-equality against
//! known-good encodings from the on-chain CallContractEvent.

use alloy::{
    primitives::{Bytes, FixedBytes, U256},
    sol_types::SolValue,
};
use eyre::Result;
use solana_axelar_its::encoding::{DeployInterchainToken, HubMessage, Message};

/// Borsh-encode a HubMessage::SendToHub{ DeployInterchainToken } the way the
/// Solana ITS program does in `gmp::send_to_hub_wrap` + `encoding::HubMessage`.
/// Returns the raw bytes that get put on the wire to the cosmos hub.
pub(super) fn encode_send_to_hub_deploy(
    destination_chain: &str,
    token_id: &[u8; 32],
    name: &str,
    symbol: &str,
    decimals: u8,
    minter: Option<Vec<u8>>,
) -> Result<Vec<u8>> {
    let inner = Message::DeployInterchainToken(DeployInterchainToken {
        token_id: *token_id,
        name: name.to_string(),
        symbol: symbol.to_string(),
        decimals,
        minter,
    });
    let hub = HubMessage::SendToHub {
        destination_chain: destination_chain.to_string(),
        message: inner,
    };
    borsh::to_vec(&hub).map_err(|e| eyre::eyre!("borsh encode failed: {e}"))
}

/// ABI-encode the inner ITS deploy payload destined for the EVM ITS proxy.
/// Format: `abi.encode(uint256 messageType=1, bytes32 tokenId, string name, string symbol, uint8 decimals, bytes minter)`.
/// Note: `uint8` and `uint256` produce identical 32-byte encodings in tuple position
/// for values that fit, so we widen to U256 for alloy's `abi_encode_params` tuple support.
pub(super) fn encode_inner_deploy(
    token_id: &[u8; 32],
    name: &str,
    symbol: &str,
    decimals: u8,
    minter: &[u8],
) -> Vec<u8> {
    (
        crate::types::ItsMessageType::DeployInterchainToken.as_u256(),
        FixedBytes::<32>::from(*token_id),
        name.to_string(),
        symbol.to_string(),
        U256::from(decimals),
        Bytes::copy_from_slice(minter),
    )
        .abi_encode_params()
}

/// ABI-encode the inner ITS interchain-transfer payload.
/// Format: `abi.encode(uint256 messageType=0, bytes32 tokenId, bytes sourceAddress, bytes destinationAddress, uint256 amount, bytes data)`.
pub(super) fn encode_inner_transfer(
    token_id: &[u8; 32],
    source_address: &[u8],
    destination_address: &[u8],
    amount: u64,
    data: &[u8],
) -> Vec<u8> {
    (
        crate::types::ItsMessageType::InterchainTransfer.as_u256(),
        FixedBytes::<32>::from(*token_id),
        Bytes::copy_from_slice(source_address),
        Bytes::copy_from_slice(destination_address),
        U256::from(amount),
        Bytes::copy_from_slice(data),
    )
        .abi_encode_params()
}

/// ABI-encode the outer hub envelope for an inbound ITS message.
/// Format: `abi.encode(uint256 messageType=4, string originalSourceChain, bytes innerPayload)`.
pub(super) fn encode_receive_from_hub(
    original_source_chain: &crate::types::ChainAxelarId,
    inner: &[u8],
) -> Vec<u8> {
    (
        crate::types::ItsMessageType::ReceiveFromHub.as_u256(),
        original_source_chain.to_string(),
        Bytes::copy_from_slice(inner),
    )
        .abi_encode_params()
}
