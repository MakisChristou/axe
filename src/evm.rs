use std::collections::HashSet;
use std::num::NonZeroUsize;

use alloy::signers::k256::PublicKey;
use alloy::signers::k256::elliptic_curve::sec1::ToEncodedPoint;
use alloy::{
    hex,
    network::{Network, ReceiptResponse},
    primitives::{Address, Bytes, FixedBytes, keccak256},
    providers::{DynProvider, PendingTransactionBuilder, Provider as _, ProviderBuilder},
    rpc::client::RpcClient,
    signers::local::PrivateKeySigner,
    sol,
    sol_types::{SolCall, SolValue},
    transports::{http::Http, layers::FallbackLayer},
};
use eyre::Result;
use serde_json::Value;
use tower::Layer;

use crate::timing::EVM_TX_RECEIPT_TIMEOUT;
use crate::ui;

/// Build a read-only EVM provider that transparently fails over across `urls`
/// (primary first, then the public fallback).
///
/// alloy's [`FallbackLayer`] ranks the transports by success-rate + latency, so
/// a persistently-failing endpoint - Hedera 429s, Avalanche "state not
/// available for pending block" - is routed around automatically. Combined with
/// the axe-level retries that re-issue each call, this is the "retry, then fall
/// back to the public RPC" behaviour: a retried call whose primary keeps
/// erroring lands on the healthy public transport. Identical URLs are
/// deduplicated, so passing `[private, public]` where the secret is unset (both
/// resolve to the public RPC) is a harmless single-transport provider.
pub fn connect_evm(urls: &[String]) -> Result<DynProvider> {
    Ok(ProviderBuilder::new()
        .connect_client(fallback_client(urls)?)
        .erased())
}

/// Signed variant for state-changing calls: same failover, plus the wallet
/// filler so the provider can sign + send.
pub fn connect_evm_signed(urls: &[String], signer: PrivateKeySigner) -> Result<DynProvider> {
    Ok(ProviderBuilder::new()
        .wallet(signer)
        .connect_client(fallback_client(urls)?)
        .erased())
}

/// Assemble an [`RpcClient`] over one-or-more HTTP transports behind a
/// single-active fallback (`active_transport_count = 1` so writes are never
/// broadcast to two endpoints in parallel).
fn fallback_client(urls: &[String]) -> Result<RpcClient> {
    let mut seen = HashSet::new();
    let transports: Vec<Http<reqwest::Client>> = urls
        .iter()
        .filter(|url| seen.insert((*url).clone()))
        .map(|url| Ok::<_, eyre::Report>(Http::new(url.parse()?)))
        .collect::<Result<_>>()?;
    if transports.is_empty() {
        eyre::bail!("connect_evm: no RPC URLs provided");
    }
    // `NonZeroUsize::MIN` is 1 - one active transport at a time.
    let fallback = FallbackLayer::default().with_active_transport_count(NonZeroUsize::MIN);
    Ok(RpcClient::new(fallback.layer(transports), false))
}

sol! {
    #[sol(rpc)]
    contract ConstAddressDeployer {
        function deploy(bytes bytecode, bytes32 salt) external returns (address deployedAddress_);
        function deployedAddress(bytes bytecode, address sender, bytes32 salt) external view returns (address deployedAddress_);
    }

    #[sol(rpc)]
    contract Create3Deployer {
        function deploy(bytes bytecode, bytes32 salt) external returns (address deployedAddress_);
        function deployedAddress(bytes bytecode, address sender, bytes32 salt) external view returns (address deployedAddress_);
    }

    #[sol(rpc)]
    contract Ownable {
        function transferOwnership(address newOwner) external;
        function owner() external view returns (address);
    }

    #[sol(rpc)]
    contract Operators {
        function addOperator(address operator) external;
        function isOperator(address account) external view returns (bool);
    }

    #[sol(rpc)]
    contract SenderReceiver {
        function sendMessage(
            string calldata destinationChain,
            string calldata destinationAddress,
            string calldata message_
        ) external payable;
        function sendPayload(
            string calldata destinationChain,
            string calldata destinationAddress,
            bytes calldata payload
        ) external payable;
        function message() external view returns (string);
        function gateway() external view returns (address);
        function execute(
            bytes32 commandId,
            string calldata sourceChain,
            string calldata sourceAddress,
            bytes calldata payload
        ) external;
    }

    #[sol(rpc)]
    contract AxelarAmplifierGateway {
        function callContract(
            string calldata destinationChain,
            string calldata destinationContractAddress,
            bytes calldata payload
        ) external;
        function isContractCallApproved(
            bytes32 commandId,
            string calldata sourceChain,
            string calldata sourceAddress,
            address contractAddress,
            bytes32 payloadHash
        ) external view returns (bool);
        function isMessageApproved(
            string calldata sourceChain,
            string calldata messageId,
            string calldata sourceAddress,
            address contractAddress,
            bytes32 payloadHash
        ) external view returns (bool);
        /// Amplifier gateway: true once the approved message has been consumed
        /// by the destination contract (`validateMessage`). Unlike
        /// `isMessageApproved` (which flips back to false on execution), this
        /// stays true, so it detects an approve→execute that happened between
        /// two polls.
        function isMessageExecuted(
            string calldata sourceChain,
            string calldata messageId
        ) external view returns (bool);
        /// Legacy consensus gateway: true once the approval command has been
        /// executed (i.e. the destination contract consumed it). Returns false
        /// before approval and after the approval is otherwise spent.
        function isCommandExecuted(bytes32 commandId) external view returns (bool);
    }

    #[sol(rpc)]
    contract AxelarServiceGovernance {
        function operator() external view returns (address);
        function governanceChain() external view returns (string);
        function governanceAddress() external view returns (string);
        function minimumTimeLockDelay() external view returns (uint256);
        function getProposalEta(address target, bytes callData, uint256 nativeValue) external view returns (uint256);
        function isOperatorProposalApproved(address target, bytes callData, uint256 nativeValue) external view returns (bool);
        function executeOperatorProposal(address target, bytes callData, uint256 nativeValue) external payable;
        function executeProposal(address target, bytes callData, uint256 nativeValue) external payable;
        function execute(bytes32 commandId, string sourceChain, string sourceAddress, bytes payload) external;
    }

    #[sol(rpc)]
    contract AxelarGasService {
        function payNativeGasForContractCall(
            address sender,
            string calldata destinationChain,
            string calldata destinationAddress,
            bytes calldata payload,
            address refundAddress
        ) external payable;
    }

    /// Legacy init-based proxy (AxelarGasServiceProxy, AxelarDepositServiceProxy)
    #[sol(rpc)]
    contract LegacyProxy {
        function init(address implementationAddress, address newOwner, bytes memory params) external;
    }

    // WeightedSigners type for gateway setup params encoding
    struct WeightedSigner {
        address signer;
        uint128 weight;
    }

    struct WeightedSigners {
        WeightedSigner[] signers;
        uint128 threshold;
        bytes32 nonce;
    }

    // Gateway setup params: abi.encode(address operator, WeightedSigners[] signers)
    function setupParams(address operator, WeightedSigners[] signers);

    // AxelarGateway ContractCall event (emitted by callContract)
    event ContractCall(
        address indexed sender,
        string destinationChain,
        string destinationContractAddress,
        bytes32 indexed payloadHash,
        bytes payload
    );

    // Legacy consensus AxelarGateway destination events (IAxelarConsensusGateway).
    // `contractAddress` and `payloadHash` are indexed, so the destination
    // approval can be located with a cheap topic-filtered eth_getLogs; the
    // non-indexed `sourceTxHash` then pins the match to our exact source tx and
    // carries the on-chain `commandId` (topic1).
    event ContractCallApproved(
        bytes32 indexed commandId,
        string sourceChain,
        string sourceAddress,
        address indexed contractAddress,
        bytes32 indexed payloadHash,
        bytes32 sourceTxHash,
        uint256 sourceEventIndex
    );
    event ContractCallExecuted(bytes32 indexed commandId);

    #[sol(rpc)]
    contract InterchainTokenFactory {
        function deployInterchainToken(
            bytes32 salt,
            string calldata name,
            string calldata symbol,
            uint8 decimals,
            uint256 initialSupply,
            address minter
        ) external payable returns (bytes32 tokenId);

        function deployRemoteInterchainToken(
            bytes32 salt,
            string calldata destinationChain,
            uint256 gasValue
        ) external payable returns (bytes32 tokenId);
    }

    #[sol(rpc)]
    contract InterchainTokenService {
        function owner() external view returns (address);
        function isOperator(address account) external view returns (bool);
        function interchainTokenAddress(bytes32 tokenId) external view returns (address);
        /// Hedera-fork variant — `interchainTokenAddress` was removed because
        /// HTS tokens don't have deterministic addresses. The Hedera ITS exposes
        /// `registeredTokenAddress` instead; same semantics for our purposes
        /// (returns the deployed token's address for a given token id).
        function registeredTokenAddress(bytes32 tokenId) external view returns (address);
        /// Address of the deployed `TokenManager` for the given token id. The
        /// token manager — not the ITS proxy — is the spender that pulls
        /// tokens via `safeTransferFrom(from, address(this), amount)` for
        /// lock/unlock-managed tokens, so canonical-token transfers must
        /// approve THIS address (not the ITS proxy).
        function tokenManagerAddress(bytes32 tokenId) external view returns (address);
        /// Token-manager type for the given token id. The values match the
        /// `TokenManagerType` enum in axelar-its:
        ///   0 = NATIVE_INTERCHAIN_TOKEN — mint/burn via the Axelar
        ///       `InterchainToken`'s owner-only `mint/burn`, no allowance
        ///       required.
        ///   1 = MINT_BURN_FROM — `burnFrom` via the token, requires
        ///       allowance from sender → token manager.
        ///   2 = LOCK_UNLOCK — `safeTransferFrom` via the token manager,
        ///       requires allowance from sender → token manager.
        ///   3 = LOCK_UNLOCK_FEE — same as LOCK_UNLOCK with a fee hook.
        ///   4 = MINT_BURN — mint/burn via a custom token whose minter is
        ///       the token manager; no allowance required.
        ///
        /// Reverts on older ITS deployments for token ids that have no
        /// `TokenManager` deployed locally — that revert is itself
        /// diagnostic (the canonical token isn't registered on this chain).
        function tokenManagerType(bytes32 tokenId) external view returns (uint8);
        function isTrustedChain(string calldata chainName) external view returns (bool);
        function itsHubAddress() external view returns (string memory);
        /// Legacy ITS trust API. Returns the trusted address for a chain — for
        /// hub-routed chains this is the literal string "hub". For "axelar"
        /// this returns the actual hub's bech32 address. Reverts if not set.
        function trustedAddress(string calldata chain) external view returns (string memory);
        function interchainTransfer(
            bytes32 tokenId,
            string calldata destinationChain,
            bytes calldata destinationAddress,
            uint256 amount,
            bytes calldata metadata,
            uint256 gasValue
        ) external payable;
        function execute(
            bytes32 commandId,
            string calldata sourceChain,
            string calldata sourceAddress,
            bytes calldata payload
        ) external;
    }

    // InterchainTokenDeployed event (emitted by ITS when a token is deployed)
    event InterchainTokenDeployed(
        bytes32 indexed tokenId,
        address tokenAddress,
        address minter,
        string name,
        string symbol,
        uint8 decimals
    );

    #[sol(rpc)]
    contract ERC20 {
        function name() external view returns (string);
        function decimals() external view returns (uint8);
        function balanceOf(address account) external view returns (uint256);
        function transfer(address to, uint256 amount) external returns (bool);
        function approve(address spender, uint256 amount) external returns (bool);
        function allowance(address owner, address spender) external view returns (uint256);
    }

    #[sol(rpc)]
    contract InterchainToken {
        function interchainTokenId() external view returns (bytes32);
        function interchainTransfer(
            string calldata destinationChain,
            bytes calldata destinationAddress,
            uint256 amount,
            bytes calldata metadata
        ) external payable;
    }
}

/// Compute salt: keccak256(abi.encode(key)) — matches JS getSaltFromKey
pub fn get_salt_from_key(key: &str) -> FixedBytes<32> {
    let encoded = key.abi_encode();
    keccak256(&encoded)
}

pub async fn read_artifact_bytecode(artifact_path: &str) -> Result<Vec<u8>> {
    let content = tokio::fs::read_to_string(artifact_path).await?;
    let artifact: Value = serde_json::from_str(&content)?;
    let bytecode_hex = artifact["bytecode"]
        .as_str()
        .ok_or_else(|| eyre::eyre!("no bytecode field in artifact"))?;
    Ok(hex::decode(
        bytecode_hex.strip_prefix("0x").unwrap_or(bytecode_hex),
    )?)
}

/// Convert an ECDSA public key (compressed or uncompressed) to an EVM address.
pub fn pubkey_to_address(pubkey_bytes: &[u8]) -> Result<Address> {
    let pubkey =
        PublicKey::from_sec1_bytes(pubkey_bytes).map_err(|e| eyre::eyre!("invalid pubkey: {e}"))?;

    // Get uncompressed SEC1 encoding (65 bytes: 0x04 || x || y)
    let uncompressed = pubkey.to_encoded_point(false);

    // EVM address = keccak256(x || y)[12..32]  (skip the 0x04 prefix)
    let hash = keccak256(&uncompressed.as_bytes()[1..]);
    Ok(Address::from_slice(&hash[12..]))
}

/// Encode gateway setup params: abi.encode(address operator, WeightedSigners[] signers)
pub fn encode_gateway_setup_params(
    operator: Address,
    signers: &[(Address, u128)],
    threshold: u128,
    nonce: FixedBytes<32>,
) -> Bytes {
    let weighted_signers = vec![WeightedSigners {
        signers: signers
            .iter()
            .map(|(addr, weight)| WeightedSigner {
                signer: *addr,
                weight: *weight,
            })
            .collect(),
        threshold,
        nonce,
    }];

    let encoded = setupParamsCall {
        operator,
        signers: weighted_signers,
    }
    .abi_encode();

    // setupParamsCall encodes with the function selector (4 bytes) — we need just the params
    Bytes::from(encoded[4..].to_vec())
}

/// Decode EVM revert data into a human-readable error name.
pub fn decode_revert_data(hex_str: &str) -> String {
    let hex_data = hex_str.strip_prefix("0x").unwrap_or(hex_str);
    if hex_data.len() < 8 {
        return format!("unknown revert data: 0x{hex_data}");
    }
    let selector = &hex_data[..8];
    match selector {
        "68155f9a" => "InvalidImplementation() — implementation has no code".into(),
        "97905dfb" => "SetupFailed() — gateway setup() reverted".into(),
        "49e27cff" => "InvalidOwner() — owner is zero address".into(),
        "0dc149f0" => "AlreadyInitialized() — proxy already initialized".into(),
        "30cd7471" => "NotOwner() — caller is not owner".into(),
        "5e231fff" => "InvalidSigners() — signers array invalid".into(),
        "aabd5a09" => "InvalidThreshold() — threshold out of range".into(),
        "84677ce8" => "InvalidWeights() — signer weights invalid".into(),
        "bf10dd3a" => "NotProxy() — must be called via proxy delegatecall".into(),
        "d924e5f4" => "InvalidOwnerAddress()".into(),
        "f9188a68" => "UntrustedChain() — destination chain not trusted by ITS".into(),
        "08c379a0" => {
            if let Ok(bytes) = hex::decode(hex_data)
                && bytes.len() > 4 + 32 + 32
            {
                let offset = 4 + 32;
                let len = u32::from_be_bytes(
                    bytes[offset + 28..offset + 32].try_into().unwrap_or([0; 4]),
                ) as usize;
                let str_start = offset + 32;
                let str_end = (str_start + len).min(bytes.len());
                let msg = String::from_utf8_lossy(&bytes[str_start..str_end]);
                return format!("revert: \"{msg}\"");
            }
            format!("Error(string) — data: 0x{hex_data}")
        }
        _ => format!("unknown error selector 0x{selector} (data: 0x{hex_data})"),
    }
}

/// Try to extract revert data hex from an alloy error's Debug representation.
pub fn decode_evm_error(err: &dyn std::fmt::Debug) -> String {
    let debug = format!("{err:?}");
    for pattern in ["\"0x", "data: \"0x"] {
        if let Some(pos) = debug.find(pattern) {
            let start = debug[pos..].find("0x").map(|i| pos + i).unwrap_or(pos);
            let hex_end = debug[start + 2..]
                .find(|c: char| !c.is_ascii_hexdigit())
                .map(|i| start + 2 + i)
                .unwrap_or(debug.len());
            let hex_data = &debug[start..hex_end];
            if hex_data.len() >= 10 {
                return decode_revert_data(hex_data);
            }
        }
    }
    debug
}

/// Compute CREATE address: keccak256(rlp([sender, nonce]))[12..]
pub fn compute_create_address(sender: Address, nonce: u64) -> Address {
    let mut stream = Vec::new();

    let sender_bytes = sender.as_slice();
    let mut items = Vec::new();

    // RLP encode 20-byte address
    items.push(0x94u8); // 0x80 + 20
    items.extend_from_slice(sender_bytes);

    // RLP encode nonce
    if nonce == 0 {
        items.push(0x80);
    } else if nonce < 0x80 {
        items.push(nonce as u8);
    } else {
        let nonce_bytes = {
            let mut b = nonce.to_be_bytes().to_vec();
            while b.first() == Some(&0) {
                b.remove(0);
            }
            b
        };
        items.push(0x80 + nonce_bytes.len() as u8);
        items.extend_from_slice(&nonce_bytes);
    }

    // List header
    let len = items.len();
    if len < 56 {
        stream.push(0xc0 + len as u8);
    } else {
        let len_bytes = {
            let mut b = len.to_be_bytes().to_vec();
            while b.first() == Some(&0) {
                b.remove(0);
            }
            b
        };
        stream.push(0xf7 + len_bytes.len() as u8);
        stream.extend_from_slice(&len_bytes);
    }
    stream.extend_from_slice(&items);

    let hash = keccak256(&stream);
    Address::from_slice(&hash[12..])
}

/// Print the tx hash, wait for the receipt with the standard timeout, and
/// log the confirmation block. Replaces the 8-line broadcast-and-await block
/// repeated across the deploy/test/load-test commands.
///
/// `label` controls the prefix in both the kv print (e.g. "tx" → `tx: 0x…`)
/// and the timeout error (e.g. "{label} {tx_hash} timed out after Ns").
pub async fn broadcast_and_log<N>(
    pending: PendingTransactionBuilder<N>,
    label: &str,
) -> Result<N::ReceiptResponse>
where
    N: Network,
{
    let tx_hash = *pending.tx_hash();
    ui::tx_hash(label, &format!("{tx_hash}"));
    ui::info("waiting for confirmation...");
    let receipt = tokio::time::timeout(EVM_TX_RECEIPT_TIMEOUT, pending.get_receipt())
        .await
        .map_err(|_| {
            eyre::eyre!(
                "{label} {tx_hash} timed out after {}s",
                EVM_TX_RECEIPT_TIMEOUT.as_secs()
            )
        })??;
    ui::success(&format!(
        "confirmed in block {}",
        receipt.block_number().unwrap_or(0)
    ));
    Ok(receipt)
}
