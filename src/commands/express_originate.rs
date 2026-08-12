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
//! - the registry's express asset (testnet `aUSDC`, mainnet `axlUSDC`),
//!   within the per-chain USD cap it sets.
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
use crate::types::Network;
use crate::ui;

/// The AxelarApp proxy registered under the `axelar-app` project in
/// `gmp-api/config/projects.yml`. One deterministic address per network,
/// identical across every chain that carries it. Override with
/// `--app-address` if the registry moves.
pub fn default_app_proxy(network: Network) -> &'static str {
    match network {
        Network::Mainnet => "0x77Accd23cC3Ccc5E36a543CEdcD03764BF6AD401",
        _ => "0xe4f05a0D5541C03d07f5175147E92D796Cae8db6",
    }
}

/// Gateway symbol to send by default.
///
/// The mainnet registry also covers `USDC`, but that one is registered
/// per-chain and unevenly - mainnet `base`, for instance, carries `axlUSDC`
/// and no `USDC`. Since this side only sees the *source* gateway, defaulting
/// to `USDC` could pick a symbol the destination cannot mint and fail late, on
/// the destination execute. `axlUSDC` is the canonical Axelar asset present on
/// every gateway, so it is the safe default. Pass `--symbol USDC` for a pair
/// known to carry it.
pub fn default_symbols(network: Network) -> Vec<String> {
    match network {
        Network::Mainnet => vec!["axlUSDC".to_string()],
        _ => vec!["aUSDC".to_string()],
    }
}

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
        function balanceOf(address owner) external view returns (uint256);
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
    /// Gateway symbols to probe, in order. See [`default_symbols`].
    pub symbols: Vec<String>,
    /// Token base units (USDC-family is 6 decimals). Must sit inside the
    /// registry's per-chain cap.
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

    let (symbol, token) = gateway_token(
        &endpoints,
        args.source_gateway,
        &args.symbols,
        sender,
        args.amount,
    )
    .await?;
    ui::address(&format!("{symbol} (source)"), &format!("{token}"));

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
            tokenSymbol: symbol.clone(),
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

/// Pick the express asset to send: the first candidate symbol the source
/// gateway carries *and* the sender holds enough of.
///
/// Both halves matter. The registry lists several symbols because different
/// chains register different ones (mainnet `base` has `axlUSDC` but no
/// `USDC`), and a wallet rarely holds all of them, so resolving on address
/// alone picks a symbol the run then fails on for balance.
async fn gateway_token(
    endpoints: &EvmEndpoints,
    gateway: Address,
    symbols: &[String],
    holder: Address,
    amount: U256,
) -> Result<(String, Address)> {
    let mut seen: Vec<String> = Vec::new();
    for symbol in symbols {
        let wanted = symbol.clone();
        let token = retry_with_fallback_all("gateway.tokenAddresses", endpoints.providers(), |p| {
            let wanted = wanted.clone();
            async move {
                IGatewayTokens::new(gateway, p)
                    .tokenAddresses(wanted)
                    .call()
                    .await
            }
        })
        .await
        .map_err(|e| eyre!("gateway.tokenAddresses({symbol}) failed: {e}"))?;
        if token.is_zero() {
            seen.push(format!("{symbol}: not registered"));
            continue;
        }
        let balance =
            retry_with_fallback_all("token.balanceOf", endpoints.providers(), |p| async move {
                IERC20Approve::new(token, p).balanceOf(holder).call().await
            })
            .await
            .map_err(|e| eyre!("balanceOf({symbol}) failed: {e}"))?;
        if balance >= amount {
            return Ok((symbol.clone(), token));
        }
        seen.push(format!("{symbol}: holds {balance}, needs {amount}"));
    }
    Err(eyre!(
        "no usable express asset on this chain ({}) - fund the wallet or pass --symbol",
        seen.join("; ")
    ))
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
