use alloy::{
    network::TransactionBuilder,
    primitives::{Address, Bytes},
    providers::Provider,
    rpc::types::TransactionRequest,
    sol_types::SolValue,
};
use eyre::Result;

use crate::evm::read_artifact_bytecode;
use crate::state::{State, save_state};
use crate::ui;

/// Reuse the cached SenderReceiver if its bytecode is still on chain;
/// redeploy if it's gone (the testnet occasionally wipes contracts) or if no
/// cached address exists. Persists the resulting address back to state.
pub async fn ensure_sender_receiver_deployed<P: Provider>(
    provider: &P,
    state: &mut State,
    gateway: Address,
    gas_service: Address,
) -> Result<Address> {
    let addr = match state.sender_receiver_address {
        Some(addr) if !provider.get_code_at(addr).await?.is_empty() => {
            ui::info(&format!("SenderReceiver: reusing {addr}"));
            addr
        }
        Some(addr) => {
            ui::warn(&format!(
                "SenderReceiver at {addr} has no code, redeploying..."
            ));
            deploy_sender_receiver(provider, gateway, gas_service).await?
        }
        None => {
            ui::info("deploying SenderReceiver...");
            deploy_sender_receiver(provider, gateway, gas_service).await?
        }
    };

    state.sender_receiver_address = Some(addr);
    save_state(state).await?;
    ui::address("SenderReceiver", &format!("{addr}"));
    Ok(addr)
}

async fn deploy_sender_receiver<P: Provider>(
    provider: &P,
    gateway: Address,
    gas_service: Address,
) -> Result<Address> {
    let bytecode = read_artifact_bytecode("artifacts/SenderReceiver.json").await?;
    let mut deploy_code = bytecode;
    deploy_code.extend_from_slice(&(gateway, gas_service).abi_encode_params());

    let tx = TransactionRequest::default().with_deploy_code(Bytes::from(deploy_code));

    let pending = provider.send_transaction(tx).await?;
    let receipt = crate::evm::broadcast_and_log(pending, "deploy tx").await?;
    let addr = receipt
        .contract_address
        .ok_or_else(|| eyre::eyre!("no contract address in receipt"))?;
    Ok(addr)
}
