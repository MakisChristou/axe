//! Sui-as-source ITS (`interchain_token_service`) helpers.
//!
//! We only support Sui-as-source ITS for now. The destination-side flow on
//! Sui is driven by the cgp-sui relayer auto-calling
//! `example::its::receive_interchain_transfer<T>` on `MessageApproved`, so it
//! surfaces in events the same way as GMP and reuses the GMP destination
//! verifier.
//!
//! The PTB structure mirrors
//! `axelar-contract-deployments/node_modules/@axelar-network/axelar-cgp-sui/move/example/sources/its/its.move`:
//!
//! ```move
//! public fun send_interchain_transfer_call<TOKEN>(
//!   singleton: &Singleton, its: &mut InterchainTokenService,
//!   gateway: &mut Gateway, gas_service: &mut GasService,
//!   token_id: TokenId, coin: Coin<TOKEN>,
//!   destination_chain: String, destination_address: vector<u8>,
//!   metadata: vector<u8>, refund_address: address,
//!   gas: Coin<SUI>, gas_params: vector<u8>, clock: &Clock,
//! );
//! ```

use base64::Engine;
use eyre::{Result, eyre};
use serde_json::{Value, json};
use sui_sdk_types::{
    Address as SuiAddress, Argument, GasPayment, ObjectReference, Transaction, TypeTag,
};

use super::config::parse_sui_addr;
use super::rpc::{SuiClient, object_ref_from_json, owner_addr_hex};
use super::tx::{PtbBuilder, sign_and_submit};
use super::wallet::SuiWallet;

/// Sui's well-known shared `Clock` object id (`0x6`).
pub const SUI_CLOCK_ADDR_HEX: &str =
    "0x0000000000000000000000000000000000000000000000000000000000000006";

/// Subset of contracts the Sui-source ITS PTB needs: Move-package addresses
/// and the shared object ids that get passed in. Loaded from the chains
/// config (axelar-chains-config/info/{network}.json).
#[derive(Debug, Clone)]
pub struct SuiItsContractsConfig {
    /// Example::its module: `package::module`. We invoke
    /// `example_pkg::its::send_interchain_transfer_call<T>` and friends.
    pub example_pkg: SuiAddress,
    /// `interchain_token_service` Move-package address.
    pub its_pkg: SuiAddress,
    /// Example::its::Singleton (shared).
    pub its_singleton: SuiAddress,
    /// InterchainTokenService::InterchainTokenService (shared).
    pub its_object: SuiAddress,
    /// AxelarGateway::Gateway (shared) — same as in `SuiContractsConfig`.
    pub gateway_object: SuiAddress,
    /// GasService::GasService (shared) — same as in `SuiContractsConfig`.
    pub gas_service_object: SuiAddress,
}

/// Read the Sui ITS contract addresses + shared object ids from the chain
/// config. The Example contract bundles a separate ItsSingleton (vs.
/// GmpSingleton) so we read both fresh here even though some are duplicated
/// in `SuiContractsConfig`.
pub fn read_sui_its_config(
    config: &std::path::Path,
    chain_id: &str,
) -> Result<SuiItsContractsConfig> {
    let content =
        std::fs::read_to_string(config).map_err(|e| eyre!("failed to read config: {e}"))?;
    let root: Value = serde_json::from_str(&content)?;
    let chain = root
        .pointer(&format!("/chains/{chain_id}"))
        .ok_or_else(|| eyre!("chain '{chain_id}' not found in config"))?;

    let read = |ptr: &str| -> Result<&str> {
        chain
            .pointer(ptr)
            .and_then(|v| v.as_str())
            .ok_or_else(|| eyre!("missing {ptr} for sui chain '{chain_id}'"))
    };

    Ok(SuiItsContractsConfig {
        example_pkg: parse_sui_addr(read("/contracts/Example/address")?)?,
        its_pkg: parse_sui_addr(read("/contracts/InterchainTokenService/address")?)?,
        its_singleton: parse_sui_addr(read("/contracts/Example/objects/ItsSingleton")?)?,
        its_object: parse_sui_addr(read(
            "/contracts/InterchainTokenService/objects/InterchainTokenService",
        )?)?,
        gateway_object: parse_sui_addr(read("/contracts/AxelarGateway/objects/Gateway")?)?,
        gas_service_object: parse_sui_addr(read("/contracts/GasService/objects/GasService")?)?,
    })
}

impl SuiClient {
    /// Pick the largest owned `Coin<T>` object for `owner` (matched by exact
    /// Move type tag). Used to source the input coin for an ITS transfer.
    pub async fn pick_coin_of_type(
        &self,
        owner: &SuiAddress,
        coin_type: &str,
    ) -> Result<(ObjectReference, u128)> {
        let r = self
            .call(
                "suix_getCoins",
                json!([owner_addr_hex(owner), coin_type, null, 50]),
            )
            .await?;
        let arr = r["data"]
            .as_array()
            .ok_or_else(|| eyre!("getCoins(coin_type='{coin_type}') missing data: {r}"))?;
        if arr.is_empty() {
            return Err(eyre!(
                "wallet {} has no Coin<{coin_type}> objects — mint or transfer some first",
                owner_addr_hex(owner)
            ));
        }
        let mut best: Option<&Value> = None;
        let mut best_bal: u128 = 0;
        for c in arr {
            let bal: u128 = c["balance"]
                .as_str()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            if bal > best_bal {
                best_bal = bal;
                best = Some(c);
            }
        }
        let c = best.ok_or_else(|| eyre!("no usable Coin<{coin_type}> object"))?;
        Ok((object_ref_from_json(c)?, best_bal))
    }

    /// Resolve the Move coin type tag T from a 32-byte ITS token id by
    /// `sui_devInspectTransactionBlock` of `interchain_token_service::registered_coin_type`.
    /// The return value is the BCS-encoded `std::ascii::String` containing
    /// the type-tag-without-leading-`0x`.
    pub async fn dev_inspect_registered_coin_type(
        &self,
        sender: &SuiAddress,
        its_pkg: SuiAddress,
        its_object: SuiAddress,
        token_id: [u8; 32],
    ) -> Result<String> {
        let initial_v = self.get_shared_object_initial_version(&its_object).await?;
        let mut b = PtbBuilder::new();
        // token_id::from_address(<addr>)
        let tid_addr = SuiAddress::from_hex(format!("0x{}", hex::encode(token_id)))
            .map_err(|e| eyre!("token id addr: {e:?}"))?;
        let tid_arg = b.pure_address(tid_addr)?;
        let tid_obj = b.move_call(its_pkg, "token_id", "from_address", vec![], vec![tid_arg])?;
        let its = b.shared_object(its_object, initial_v, false);
        let _ = b.move_call(
            its_pkg,
            "interchain_token_service",
            "registered_coin_type",
            vec![],
            vec![its, tid_obj],
        )?;
        let kind = b.into_transaction_kind();
        let kind_bcs = bcs::to_bytes(&kind).map_err(|e| eyre!("bcs encode kind: {e}"))?;
        let kind_b64 = base64::engine::general_purpose::STANDARD.encode(&kind_bcs);

        let r = self
            .call(
                "sui_devInspectTransactionBlock",
                json!([owner_addr_hex(sender), kind_b64, null, null]),
            )
            .await?;
        // `results[1].returnValues[0]` = [<bcs bytes>, <type-tag-string>].
        // The bytes encode `std::ascii::String` = ULEB128(len) || bytes.
        let bytes_val = r
            .pointer("/results/1/returnValues/0/0")
            .ok_or_else(|| eyre!("dev-inspect returnValues missing: {r}"))?;
        let bytes_arr = bytes_val
            .as_array()
            .ok_or_else(|| eyre!("dev-inspect returnValues bytes not array: {bytes_val}"))?;
        let raw: Vec<u8> = bytes_arr
            .iter()
            .filter_map(|v| v.as_u64().map(|n| n as u8))
            .collect();
        // Strip leading ULEB128 length prefix.
        let mut i = 0usize;
        let mut len: u64 = 0;
        let mut shift: u32 = 0;
        loop {
            let b = *raw.get(i).ok_or_else(|| eyre!("uleb128 truncated"))?;
            len |= ((b & 0x7f) as u64) << shift;
            i += 1;
            if (b & 0x80) == 0 {
                break;
            }
            shift += 7;
            if shift > 63 {
                return Err(eyre!("uleb128 overflow"));
            }
        }
        let s_bytes = raw
            .get(i..i + len as usize)
            .ok_or_else(|| eyre!("type tag bytes truncated"))?;
        let s = std::str::from_utf8(s_bytes).map_err(|e| eyre!("type tag utf8: {e}"))?;
        // Sui returns the type tag without a leading `0x` for the address.
        // Re-prefix so it round-trips through `TypeTag::from_str`.
        if let Some((addr, rest)) = s.split_once("::") {
            Ok(format!("0x{addr}::{rest}"))
        } else {
            Ok(s.to_string())
        }
    }
}

/// Outcome of a Sui-source ITS send. Mirrors `GmpSendResult` + adds the
/// resolved coin type for the report.
#[derive(Debug, Clone)]
pub struct ItsSendResult {
    pub digest: String,
    pub success: bool,
    pub error: Option<String>,
    pub event_index: u32,
    pub source_address_hex: String,
    pub payload_hash_hex: String,
}

/// Build, sign, and submit a single Sui ITS interchain transfer via
/// `example::its::send_interchain_transfer_call<T>`.
///
/// `coin_type_tag` is the Move type tag string for T (e.g.
/// `0x96b4…::token::TOKEN`); `transfer_amount` is in coin sub-units.
/// `destination_address_bytes` is the raw address as the destination chain
/// expects (20B for EVM, 32B for Solana/Stellar).
pub struct InterchainTransferRequest<'a> {
    pub client: &'a SuiClient,
    pub wallet: &'a SuiWallet,
    pub contracts: &'a SuiItsContractsConfig,
    pub coin_type_tag: &'a str,
    pub token_id: [u8; 32],
    pub destination_chain: &'a str,
    pub destination_address_bytes: &'a [u8],
    pub transfer_amount: u64,
    pub gas_value_mist: u64,
    pub gas_budget_mist: u64,
}

struct SharedVersions {
    singleton: u64,
    its: u64,
    gateway: u64,
    gas_service: u64,
    clock: u64,
}

async fn resolve_transfer_inputs(
    request: &InterchainTransferRequest<'_>,
    clock: &SuiAddress,
) -> Result<(SharedVersions, ObjectReference, ObjectReference, u64)> {
    let client = request.client;
    let contracts = request.contracts;
    let (singleton, its, gateway, gas_service, clock) = tokio::try_join!(
        client.get_shared_object_initial_version(&contracts.its_singleton),
        client.get_shared_object_initial_version(&contracts.its_object),
        client.get_shared_object_initial_version(&contracts.gateway_object),
        client.get_shared_object_initial_version(&contracts.gas_service_object),
        client.get_shared_object_initial_version(clock),
    )?;
    let (gas_coin, coin_pick, gas_price) = tokio::try_join!(
        client.pick_gas_coin(&request.wallet.address),
        client.pick_coin_of_type(&request.wallet.address, request.coin_type_tag),
        client.get_reference_gas_price(),
    )?;
    let (coin, balance) = coin_pick;
    if balance < request.transfer_amount as u128 {
        return Err(eyre!(
            "Coin<{}> object balance {balance} < transfer_amount {}",
            request.coin_type_tag,
            request.transfer_amount
        ));
    }
    Ok((
        SharedVersions {
            singleton,
            its,
            gateway,
            gas_service,
            clock,
        },
        gas_coin,
        coin,
        gas_price,
    ))
}

fn build_interchain_transfer(
    request: &InterchainTransferRequest<'_>,
    coin_type: TypeTag,
    clock: SuiAddress,
    versions: &SharedVersions,
    gas_coin: ObjectReference,
    transfer_coin: ObjectReference,
    gas_price: u64,
) -> Result<Transaction> {
    let mut builder = PtbBuilder::new();
    let mut token_id_le = request.token_id;
    token_id_le.reverse();
    let token_id_arg = builder.pure_bytes(token_id_le.to_vec());
    let token_id = builder.move_call(
        request.contracts.its_pkg,
        "token_id",
        "from_u256",
        vec![],
        vec![token_id_arg],
    )?;
    let transfer_coin = builder.owned_object(transfer_coin);
    let transfer_amount = builder.pure_u64(request.transfer_amount)?;
    let transfer_coin = builder.split_coin(transfer_coin, transfer_amount);
    let gas_value = builder.pure_u64(request.gas_value_mist)?;
    let cross_chain_gas = builder.split_coin(Argument::Gas, gas_value);
    let destination_chain = builder.pure_string(request.destination_chain)?;
    let destination_address = builder.pure_vec_u8(request.destination_address_bytes)?;
    let metadata = builder.pure_vec_u8(&[])?;
    let refund_address = builder.pure_address(request.wallet.address)?;
    let gas_params = builder.pure_vec_u8(&[])?;
    let singleton =
        builder.shared_object(request.contracts.its_singleton, versions.singleton, false);
    let its = builder.shared_object(request.contracts.its_object, versions.its, true);
    let gateway = builder.shared_object(request.contracts.gateway_object, versions.gateway, true);
    let gas_service = builder.shared_object(
        request.contracts.gas_service_object,
        versions.gas_service,
        true,
    );
    let clock = builder.shared_object(clock, versions.clock, false);

    builder.move_call(
        request.contracts.example_pkg,
        "its",
        "send_interchain_transfer_call",
        vec![coin_type],
        vec![
            singleton,
            its,
            gateway,
            gas_service,
            token_id,
            transfer_coin,
            destination_chain,
            destination_address,
            metadata,
            refund_address,
            cross_chain_gas,
            gas_params,
            clock,
        ],
    )?;
    Ok(builder.build(
        request.wallet.address,
        GasPayment {
            objects: vec![gas_coin],
            owner: request.wallet.address,
            price: gas_price,
            budget: request.gas_budget_mist,
        },
    ))
}

fn extract_contract_call(events: &[Value]) -> (u32, String, String) {
    events
        .iter()
        .enumerate()
        .find(|(_, event)| {
            event["type"]
                .as_str()
                .is_some_and(|event_type| event_type.ends_with("::events::ContractCall"))
        })
        .map(|(index, event)| {
            let field = |pointer| {
                event
                    .pointer(pointer)
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim_start_matches("0x")
                    .to_string()
            };
            (
                index as u32,
                field("/parsedJson/source_id"),
                field("/parsedJson/payload_hash"),
            )
        })
        .unwrap_or_default()
}

pub async fn send_its_interchain_transfer(
    request: InterchainTransferRequest<'_>,
) -> Result<ItsSendResult> {
    let coin_type: TypeTag = request
        .coin_type_tag
        .parse()
        .map_err(|e| eyre!("invalid coin_type '{}': {e}", request.coin_type_tag))?;
    let clock = parse_sui_addr(SUI_CLOCK_ADDR_HEX)?;
    let (versions, gas_coin, transfer_coin, gas_price) =
        resolve_transfer_inputs(&request, &clock).await?;
    let transaction = build_interchain_transfer(
        &request,
        coin_type,
        clock,
        &versions,
        gas_coin,
        transfer_coin,
        gas_price,
    )?;
    let submitted = sign_and_submit(request.client, request.wallet, transaction).await?;
    let (event_index, source_address_hex, payload_hash_hex) =
        extract_contract_call(&submitted.events);

    Ok(ItsSendResult {
        digest: submitted.digest,
        success: submitted.success,
        error: submitted.error,
        event_index,
        source_address_hex,
        payload_hash_hex,
    })
}
