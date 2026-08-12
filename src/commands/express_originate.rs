//! Originate a transfer that Axelar's own express executor will front.
//!
//! The monitor in [`super::test_express`] only observes. To exercise the real
//! `gmp-express-executor` service end-to-end, a transfer has to match the
//! express registry in `gmp-api/config/projects.yml` (project `axelar-app`):
//!
//! - the **gateway** path (`ContractCallWithToken`). Express gates on that
//!   event, so ITS `interchainTransfer` never enters the express queue.
//! - source address **and** destination contract equal to the AxelarApp proxy,
//!   the only access-controlled address the service holds an allowance for.
//! - asset `aUSDC`, within the per-chain USD cap the registry sets.
//! - gas paid through `payNativeGasForExpressCallWithToken`, which AxelarApp
//!   does when `gatewaySend.enableExpress` is set.
//!
//! This module builds exactly that call. It does **not** express-execute
//! anything itself. The caller then watches the returned tx hash through the
//! ordinary two-phase monitor, so a pass means the service fronted the funds
//! and was reimbursed by the canonical `executeWithToken`.

use alloy::primitives::{Address, Bytes, FixedBytes, U256};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol;
use alloy::sol_types::SolCall;
use eyre::{Result, eyre};

use crate::evm::{EvmEndpoints, send_tx_robust};
use crate::retry::retry_with_fallback_all;
use crate::timing::EVM_TX_RECEIPT_TIMEOUT;
use crate::ui;

/// The AxelarApp proxy. Deployed at the same address on every chain that
/// carries it, and the address registered under the `axelar-app` project in
/// `gmp-api/config/projects.yml`. Override with `--app-address` if the
/// registry moves.
pub const AXELAR_APP_PROXY: &str = "0xe4f05a0D5541C03d07f5175147E92D796Cae8db6";

/// The only asset the testnet express registry covers for this project.
const EXPRESS_SYMBOL: &str = "aUSDC";

/// `SendKind::GatewayToken` - the express-capable branch of `callAndSend`.
const SEND_KIND_GATEWAY_TOKEN: u8 = 1;
/// `IntakeMode::Approval` - a plain ERC20 allowance granted to AxelarApp.
const INTAKE_MODE_APPROVAL: u8 = 2;

sol! {
    #[sol(rpc)]
    interface IGatewayTokens {
        function tokenAddresses(string symbol) external view returns (address);
    }

    #[sol(rpc)]
    interface IERC20Approve {
        function allowance(address owner, address spender) external view returns (uint256);
        function approve(address spender, uint256 amount) external returns (bool);
    }

    struct TokenPermissions { address token; uint256 amount; }
    struct PermitTransferFrom { TokenPermissions permitted; uint256 nonce; uint256 deadline; }
    struct InterchainSendParams { bytes32 tokenId; bytes destinationAddress; uint256 gasValue; }
    struct GatewaySendParams {
        string tokenSymbol;
        string destinationContractAddress;
        address gasRefundRecipient;
        bool enableExpress;
    }
    struct CallAndSendParams {
        uint8 kind;
        uint8 intakeMode;
        uint256 amount;
        string destinationChain;
        PermitTransferFrom permit;
        bytes signature;
        bytes sourcePayload;
        bytes destinationPayload;
        InterchainSendParams interchainSend;
        GatewaySendParams gatewaySend;
    }

    #[sol(rpc)]
    interface IAxelarApp {
        function callAndSend(CallAndSendParams params) external payable;
    }
}

/// Everything `originate` needs, owned so the struct carries no lifetime.
pub struct OriginateArgs {
    pub source_rpc_urls: Vec<String>,
    pub source_gateway: Address,
    pub app_address: Address,
    pub destination_chain: String,
    /// aUSDC base units (6 decimals). Must sit inside the registry's cap.
    pub amount: U256,
    /// Final recipient of the delivered tokens on the destination chain.
    pub recipient: Address,
    pub gas_value_wei: U256,
}

/// Send the AxelarApp `callAndSend` that the express service will pick up.
/// Returns the source transaction hash.
pub async fn originate(signer: &PrivateKeySigner, args: OriginateArgs) -> Result<FixedBytes<32>> {
    let endpoints = EvmEndpoints::connect(&args.source_rpc_urls)?;
    let sender = signer.address();

    let token = gateway_token_address(&endpoints, args.source_gateway).await?;
    ui::address(&format!("{EXPRESS_SYMBOL} (source)"), &format!("{token}"));

    ensure_allowance(&endpoints, signer, token, args.app_address, args.amount).await?;

    // AxelarApp's destination envelope: the recipient, a fallback holder for a
    // failed delivery, and an optional destination-side multicall (empty here,
    // so the tokens are simply forwarded to `recipient`).
    let envelope = encode_delivery_envelope(args.recipient, sender);

    let params = CallAndSendParams {
        kind: SEND_KIND_GATEWAY_TOKEN,
        intakeMode: INTAKE_MODE_APPROVAL,
        amount: args.amount,
        destinationChain: args.destination_chain.clone(),
        permit: PermitTransferFrom {
            permitted: TokenPermissions {
                token,
                amount: args.amount,
            },
            nonce: U256::ZERO,
            deadline: U256::ZERO,
        },
        signature: Bytes::new(),
        sourcePayload: Bytes::new(),
        destinationPayload: envelope,
        interchainSend: InterchainSendParams {
            tokenId: FixedBytes::<32>::ZERO,
            destinationAddress: Bytes::new(),
            gasValue: U256::ZERO,
        },
        gatewaySend: GatewaySendParams {
            tokenSymbol: EXPRESS_SYMBOL.to_string(),
            destinationContractAddress: args.app_address.to_string(),
            gasRefundRecipient: sender,
            // Routes gas through payNativeGasForExpressCallWithToken, which is
            // what marks the call as express-eligible for the service.
            enableExpress: true,
        },
    };

    let call = IAxelarApp::callAndSendCall { params };
    let tx = TransactionRequest::default()
        .to(args.app_address)
        .from(sender)
        .value(args.gas_value_wei)
        .input(call.abi_encode().into());

    ui::info(&format!(
        "callAndSend -> {} via AxelarApp (enableExpress=true)",
        args.destination_chain
    ));
    let receipt = send_tx_robust(
        &endpoints,
        signer,
        tx,
        "express originate",
        EVM_TX_RECEIPT_TIMEOUT,
    )
    .await?;
    if !receipt.status() {
        return Err(eyre!(
            "express originate tx {:#x} reverted",
            receipt.transaction_hash
        ));
    }
    Ok(receipt.transaction_hash)
}

/// `abi.encode(recipient, leftoverRecipient, destinationPayload)` - the
/// envelope AxelarApp's `_deliver` decodes on the destination side.
fn encode_delivery_envelope(recipient: Address, leftover: Address) -> Bytes {
    use alloy::sol_types::SolValue;
    (recipient, leftover, Bytes::new())
        .abi_encode_params()
        .into()
}

/// Resolve the gateway-registered ERC20 for the express symbol.
async fn gateway_token_address(endpoints: &EvmEndpoints, gateway: Address) -> Result<Address> {
    let token = retry_with_fallback_all(
        "gateway.tokenAddresses",
        endpoints.providers(),
        |p| async move {
            IGatewayTokens::new(gateway, p)
                .tokenAddresses(EXPRESS_SYMBOL.to_string())
                .call()
                .await
        },
    )
    .await
    .map_err(|e| eyre!("gateway.tokenAddresses({EXPRESS_SYMBOL}) failed: {e}"))?;
    if token.is_zero() {
        return Err(eyre!(
            "{EXPRESS_SYMBOL} is not registered on this chain's gateway - pick a chain that carries it"
        ));
    }
    Ok(token)
}

/// AxelarApp pulls the token with a plain `transferFrom`, so it needs an
/// allowance. Approve once and reuse it on later runs.
async fn ensure_allowance(
    endpoints: &EvmEndpoints,
    signer: &PrivateKeySigner,
    token: Address,
    spender: Address,
    amount: U256,
) -> Result<()> {
    let owner = signer.address();
    let current =
        retry_with_fallback_all("token.allowance", endpoints.providers(), |p| async move {
            IERC20Approve::new(token, p)
                .allowance(owner, spender)
                .call()
                .await
        })
        .await
        .map_err(|e| eyre!("token.allowance failed: {e}"))?;
    if current >= amount {
        return Ok(());
    }

    let call = IERC20Approve::approveCall {
        spender,
        amount: U256::MAX,
    };
    let tx = TransactionRequest::default()
        .to(token)
        .from(owner)
        .input(call.abi_encode().into());
    let receipt = send_tx_robust(
        endpoints,
        signer,
        tx,
        "approve AxelarApp",
        EVM_TX_RECEIPT_TIMEOUT,
    )
    .await?;
    if !receipt.status() {
        return Err(eyre!("approve tx {:#x} reverted", receipt.transaction_hash));
    }
    Ok(())
}
