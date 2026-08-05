//! EVM destination checks.

use alloy::primitives::{Address, FixedBytes};
use alloy::providers::Provider;
use eyre::Result;

use crate::evm::AxelarAmplifierGateway;
use crate::retry::retry_all;

/// Check `isMessageApproved` on the EVM gateway.
///
/// Wrapped in `retry_all`: these are read-only, idempotent `eth_call`s, so any
/// error is plausibly a transient RPC hiccup (5xx, 429, dropped connection).
/// Without the retry a single blip during one poll cycle propagates `?` up
/// through `poll_pipeline` and aborts the whole (up-to-2h) verification run,
/// marking every in-flight tx failed. EVM is the dominant mainnet destination
/// class, so the resilience the cosmos/Sui/Solana/XRPL paths already have
/// matters most here.
pub(in super::super) async fn check_evm_is_message_approved<P: Provider>(
    gw_contract: &AxelarAmplifierGateway::AxelarAmplifierGatewayInstance<&P>,
    source_chain: &str,
    message_id: &str,
    source_address: &str,
    contract_addr: Address,
    payload_hash: FixedBytes<32>,
) -> Result<bool> {
    let approved = retry_all("isMessageApproved", || async {
        gw_contract
            .isMessageApproved(
                source_chain.to_string(),
                message_id.to_string(),
                source_address.to_string(),
                contract_addr,
                payload_hash,
            )
            .call()
            .await
    })
    .await?;
    Ok(approved)
}

/// Check `isMessageExecuted` on the EVM amplifier gateway (single attempt).
/// True once the approved message has been consumed by the destination
/// contract. Unlike `isMessageApproved` — which flips back to false the instant
/// the message executes — this stays true, so it catches an approval that was
/// approved *and* executed between two polls (the fast-route race that left
/// monad-3 / hyperliquid stuck in the Approved phase until the inactivity
/// timeout). Wrapped in `retry_all` for the same transient-RPC resilience as
/// [`check_evm_is_message_approved`].
pub(in super::super) async fn check_evm_is_message_executed<P: Provider>(
    gw_contract: &AxelarAmplifierGateway::AxelarAmplifierGatewayInstance<&P>,
    source_chain: &str,
    message_id: &str,
) -> Result<bool> {
    let executed = retry_all("isMessageExecuted", || async {
        gw_contract
            .isMessageExecuted(source_chain.to_string(), message_id.to_string())
            .call()
            .await
    })
    .await?;
    Ok(executed)
}

/// Check `isCommandExecuted` on a legacy consensus gateway. True once the
/// destination contract has consumed the approval command. Wrapped in
/// `retry_all` for the same transient-RPC resilience as
/// [`check_evm_is_message_approved`].
pub(in super::super) async fn check_evm_command_executed<P: Provider>(
    gw_contract: &AxelarAmplifierGateway::AxelarAmplifierGatewayInstance<&P>,
    command_id: FixedBytes<32>,
) -> Result<bool> {
    let executed = retry_all("isCommandExecuted", || async {
        gw_contract.isCommandExecuted(command_id).call().await
    })
    .await?;
    Ok(executed)
}
