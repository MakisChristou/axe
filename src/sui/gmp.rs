//! High-level Sui→destination GMP send: build, sign, and submit the
//! `Example::gmp::send_call` PTB and lift the on-chain `ContractCall` event
//! out of the response into [`GmpSendResult`].

use eyre::Result;
use sui_sdk_types::{Argument, GasPayment};

use super::config::SuiContractsConfig;
use super::rpc::SuiClient;
use super::tx::{PtbBuilder, sign_and_submit};
use super::wallet::SuiWallet;

/// One Sui→destination GMP call. Bundles the per-call inputs (destination
/// fields + gas/budget) so `send_gmp_call` doesn't need an 8-positional-arg
/// signature where it's easy to swap, e.g., the chain and address strings at
/// the call site.
pub struct SuiGmpCall {
    pub destination_chain: String,
    pub destination_address: String,
    pub payload: Vec<u8>,
    /// Cross-chain gas paid into the Sui `GasService`, in mist (1 SUI = 1e9
    /// mist). Used by the relayer to fund the destination-side `execute`.
    pub gas_value_mist: u64,
    /// On-chain Sui tx gas budget in mist, separate from `gas_value_mist`
    /// (which is the cross-chain message gas). Caller picks based on a
    /// pessimistic upper bound for the PTB cost.
    pub gas_budget_mist: u64,
}

/// Outcome of a GMP send: tx digest + the index in `events[]` of the
/// `ContractCall` event (which is the message id suffix).
#[derive(Debug, Clone)]
pub struct GmpSendResult {
    pub digest: String,
    pub success: bool,
    pub error: Option<String>,
    pub event_index: u32,
    pub source_address_hex: String,
    pub payload_hash_hex: String,
}

/// Build, sign, and submit a Sui GMP send_call calling
/// `Example::gmp::send_call(singleton, gateway, gas_service, dest_chain,
///   dest_address, payload, refund_address, coin, params)`.
///
/// Move signature: `destination_chain: String, destination_address: String,
/// payload: vector<u8>, refund_address: address, coin: Coin<SUI>, params:
/// vector<u8>`. We mirror the TypeScript reference (`sui/gmp.js`).
///
/// `destination_address` is the human-readable string the destination chain
/// expects (for EVM, e.g. `"0xd7f2…"`), not raw bytes.
///
/// `gas_value_mist` is the SUI to attach as cross-chain gas (split off the
/// gas coin). `gas_budget_mist` is the on-chain Sui gas budget (the cost
/// of running this PTB itself), separate from the cross-chain gas payment.
pub async fn send_gmp_call(
    client: &SuiClient,
    wallet: &SuiWallet,
    contracts: &SuiContractsConfig,
    call: &SuiGmpCall,
) -> Result<GmpSendResult> {
    // Fetch shared-object versions in parallel.
    let (singleton_v, gateway_v, gas_service_v) = tokio::try_join!(
        client.get_shared_object_initial_version(&contracts.gmp_singleton),
        client.get_shared_object_initial_version(&contracts.gateway_object),
        client.get_shared_object_initial_version(&contracts.gas_service_object),
    )?;

    let gas_coins = client
        .pick_gas_coins(
            &wallet.address,
            u128::from(call.gas_value_mist) + u128::from(call.gas_budget_mist),
        )
        .await?;
    let rgp = client.get_reference_gas_price().await?;

    let mut b = PtbBuilder::new();
    let singleton = b.shared_object(contracts.gmp_singleton, singleton_v, true);
    let gateway = b.shared_object(contracts.gateway_object, gateway_v, true);
    let gas_svc = b.shared_object(contracts.gas_service_object, gas_service_v, true);
    let dest_chain_arg = b.pure_string(&call.destination_chain)?;
    let dest_addr_arg = b.pure_string(&call.destination_address)?;
    let payload_arg = b.pure_vec_u8(&call.payload)?;
    let refund_arg = b.pure_address(wallet.address)?;
    let amt_arg = b.pure_u64(call.gas_value_mist)?;
    let coin_arg = b.split_coin(Argument::Gas, amt_arg);
    let params_arg = b.pure_vec_u8(&[])?;

    b.move_call(
        contracts.example_pkg,
        "gmp",
        "send_call",
        vec![],
        vec![
            singleton,
            gateway,
            gas_svc,
            dest_chain_arg,
            dest_addr_arg,
            payload_arg,
            refund_arg,
            coin_arg,
            params_arg,
        ],
    )?;

    let tx = b.build(
        wallet.address,
        GasPayment {
            objects: gas_coins,
            owner: wallet.address,
            price: rgp,
            budget: call.gas_budget_mist,
        },
    );

    let submitted = sign_and_submit(client, wallet, tx).await?;

    // Find the ContractCall event in events[] to determine event_index.
    // The on-chain message_id is `0x{digest_hex}-{event_index}`.
    let mut event_index = 0u32;
    let mut source_address_hex = String::new();
    let mut payload_hash_hex = String::new();
    for (i, ev) in submitted.events.iter().enumerate() {
        let ty = ev["type"].as_str().unwrap_or("");
        if ty.ends_with("::events::ContractCall") {
            event_index = i as u32;
            source_address_hex = ev
                .pointer("/parsedJson/source_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim_start_matches("0x")
                .to_string();
            payload_hash_hex = ev
                .pointer("/parsedJson/payload_hash")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim_start_matches("0x")
                .to_string();
            break;
        }
    }

    Ok(GmpSendResult {
        digest: submitted.digest,
        success: submitted.success,
        error: submitted.error,
        event_index,
        source_address_hex,
        payload_hash_hex,
    })
}
