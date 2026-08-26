use std::collections::HashSet;
use std::num::NonZeroUsize;

use alloy::signers::k256::PublicKey;
use alloy::signers::k256::elliptic_curve::sec1::ToEncodedPoint;
use alloy::{
    consensus::{Transaction as ConsensusTx, TxEnvelope},
    eips::{BlockId, eip2718::Encodable2718},
    hex,
    network::{Network, ReceiptResponse},
    primitives::{Address, Bytes, FixedBytes, TxHash, keccak256},
    providers::{
        DynProvider, PendingTransactionBuilder, Provider as _, ProviderBuilder, SendableTx,
    },
    rpc::{client::RpcClient, types::TransactionRequest},
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
        .map(|url| {
            Ok::<_, eyre::Report>(Http::with_client(
                crate::http::client().clone(),
                url.parse()?,
            ))
        })
        .collect::<Result<_>>()?;
    if transports.is_empty() {
        eyre::bail!("connect_evm: no RPC URLs provided");
    }
    // `NonZeroUsize::MIN` is 1 - one active transport at a time.
    let fallback = FallbackLayer::default().with_active_transport_count(NonZeroUsize::MIN);
    Ok(RpcClient::new(fallback.layer(transports), false))
}

/// An ordered pool of independent EVM providers — one per unique RPC URL,
/// `[private, public]` — for application-level retry + failover via
/// [`crate::retry::retry_with_fallback`].
///
/// This complements the transport-level [`FallbackLayer`] in [`connect_evm`]:
/// that layer only rotates on *transport* failures (connection refused, 5xx,
/// socket timeout). A JSON-RPC **error body** — Hedera 429s, Avalanche's
/// "state not available for pending block", consensus-node timeouts — arrives
/// as a healthy HTTP 200, so the layer keeps routing to the broken endpoint.
/// Only the application, which sees the decoded error, can decide to move to
/// the public RPC. Iterating this pool with `retry_with_fallback` is that
/// decision.
#[derive(Clone)]
pub struct EvmEndpoints {
    urls: Vec<String>,
    providers: Vec<DynProvider>,
}

impl EvmEndpoints {
    /// Build one single-transport provider per unique URL, order preserved.
    pub fn connect(urls: &[String]) -> Result<Self> {
        let mut seen = HashSet::new();
        let unique: Vec<String> = urls
            .iter()
            .filter(|url| seen.insert((*url).clone()))
            .cloned()
            .collect();
        if unique.is_empty() {
            eyre::bail!("EvmEndpoints::connect: no RPC URLs provided");
        }
        let providers = unique
            .iter()
            .map(|url| {
                Ok(ProviderBuilder::new()
                    .connect_client(RpcClient::new(Http::new(url.parse()?), false))
                    .erased())
            })
            .collect::<Result<_>>()?;
        Ok(Self {
            urls: unique,
            providers,
        })
    }

    /// The read-only providers, `[private, public]` order — pass to
    /// `retry_with_fallback` and issue the call on the provider it hands you.
    pub fn providers(&self) -> &[DynProvider] {
        &self.providers
    }

    /// The first (preferred) provider, for call sites that need a single
    /// provider handle and carry their own retry logic.
    pub fn primary(&self) -> &DynProvider {
        &self.providers[0]
    }

    /// The endpoint URLs, in preference order. Test-only: production code
    /// reaches endpoints through the retry helpers, never by URL.
    #[cfg(test)]
    pub fn urls(&self) -> &[String] {
        &self.urls
    }

    /// The same pool with the preferred endpoint advanced by `n`, order
    /// otherwise preserved.
    ///
    /// The retry helpers always start at the front of the pool, so a caller
    /// that wants its *next* attempt to reach a different node has to rotate.
    /// Used when an error is endpoint-specific in effect rather than in kind -
    /// a mempool that already holds our nonce, say - where retrying the same
    /// node just re-reads the same view. A single-endpoint pool rotates to
    /// itself, which is harmless.
    pub fn rotated(&self, n: usize) -> Self {
        let len = self.providers.len();
        let shift = if len == 0 { 0 } else { n % len };
        let mut urls = self.urls.clone();
        let mut providers = self.providers.clone();
        urls.rotate_left(shift);
        providers.rotate_left(shift);
        Self { urls, providers }
    }

    /// Fill (nonce, gas, fees, chain id) and sign `tx` WITHOUT broadcasting,
    /// retrying the fill across endpoints — the fill is pure reads, so it can
    /// fail over freely. Returns the signed envelope: its hash is known before
    /// any broadcast, and its raw bytes can be re-broadcast on any endpoint
    /// idempotently (same bytes ⇒ same tx ⇒ at most one can land).
    ///
    /// alloy's nonce and gas fillers query the **pending** block, and some
    /// nodes (Avalanche coreth, recurringly) intermittently can't serve
    /// pending state — for stretches longer than the whole retry budget. When
    /// the retried fill dies on exactly that error, degrade once: pre-resolve
    /// the nonce and gas against `latest` (which those nodes do serve), then
    /// re-run the fill — only the fee/chain-id lookups remain, and those are
    /// latest-based already.
    pub async fn fill_and_sign(
        &self,
        signer: &PrivateKeySigner,
        tx: TransactionRequest,
    ) -> Result<TxEnvelope> {
        match self.fill_and_sign_once(signer, tx.clone()).await {
            Err(error) if is_pending_state_error(&error) => {
                ui::warn(
                    "fill kept hitting 'state not available for pending block' — \
                     re-filling with nonce+gas pinned to the latest block",
                );
                let pinned = self.prefill_from_latest(tx).await?;
                self.fill_and_sign_once(signer, pinned).await
            }
            // Pre-EIP-1559 chain (Kava): alloy's 1559 fee estimation cannot
            // decode the null `baseFeePerGas`. Degrade once to a legacy type-0
            // tx with an explicit `gas_price`, which routes alloy's filler
            // through the legacy path and skips `eth_feeHistory` entirely.
            Err(error) if is_missing_base_fee_error(&error) => {
                ui::warn(
                    "fill hit a pre-EIP-1559 fee history (null baseFeePerGas) — \
                     re-filling as a legacy type-0 tx with an explicit gas price",
                );
                let gas_price = self.primary().get_gas_price().await?;
                self.fill_and_sign_once(signer, tx.gas_price(gas_price))
                    .await
            }
            other => other,
        }
    }

    /// One retried, fallback-capable pass of alloy's fill + local signing.
    async fn fill_and_sign_once(
        &self,
        signer: &PrivateKeySigner,
        tx: TransactionRequest,
    ) -> Result<TxEnvelope> {
        let signer = signer.clone();
        crate::retry::retry_with_fallback(
            "fill+sign evm tx",
            &self.urls,
            |e: &eyre::Report| crate::retry::is_transient_default(e) || is_pending_state_error(e),
            move |url| {
                let signer = signer.clone();
                let tx = tx.clone();
                async move {
                    let provider = ProviderBuilder::new()
                        .wallet(signer)
                        .connect_client(RpcClient::new(Http::new(url.parse()?), false));
                    match provider.fill(tx).await? {
                        SendableTx::Envelope(envelope) => Ok(envelope),
                        SendableTx::Builder(_) => {
                            eyre::bail!("wallet filler did not produce a signed envelope")
                        }
                    }
                }
            },
        )
        .await
    }

    /// Resolve the fields whose fillers depend on pending-block state — nonce
    /// and gas limit — against `latest` instead, so the subsequent fill skips
    /// the pending-block queries entirely.
    async fn prefill_from_latest(&self, mut tx: TransactionRequest) -> Result<TransactionRequest> {
        let from = tx
            .from
            .ok_or_else(|| eyre::eyre!("prefill_from_latest: tx.from not set"))?;
        if tx.nonce.is_none() {
            let nonce = crate::retry::retry_with_fallback_all(
                "nonce (latest block)",
                &self.providers,
                |provider| async move { provider.get_transaction_count(from).await },
            )
            .await?;
            tx.nonce = Some(nonce);
        }
        if tx.gas.is_none() {
            let estimate = crate::retry::retry_with_fallback_all(
                "estimate gas (latest block)",
                &self.providers,
                |provider| {
                    let tx = tx.clone();
                    async move { provider.estimate_gas(tx).block(BlockId::latest()).await }
                },
            )
            .await?;
            // 20% headroom: a latest-block estimate can undershoot the state
            // the tx actually executes against.
            tx.gas = Some(estimate.saturating_mul(12) / 10);
        }
        Ok(tx)
    }

    /// Broadcast a signed envelope's raw bytes, retrying across endpoints.
    /// Safe to call repeatedly with the same envelope: identical bytes hash to
    /// the same transaction, so re-broadcast can never double-send.
    pub async fn broadcast_raw(&self, envelope: &TxEnvelope) -> Result<TxHash> {
        let raw = envelope.encoded_2718();
        crate::retry::retry_with_fallback(
            "broadcast evm tx",
            &self.providers,
            |e: &eyre::Report| crate::retry::is_transient_default(e) || is_pending_state_error(e),
            move |provider| {
                let raw = raw.clone();
                async move {
                    let pending = provider.send_raw_transaction(&raw).await?;
                    Ok(*pending.tx_hash())
                }
            },
        )
        .await
    }
}

/// Avalanche's coreth rejection when a node cannot serve pending-block state
/// for the gas/nonce fill. Endpoint-specific — the public RPC serves it fine —
/// so it must count as transient for retry/fallback purposes.
pub fn is_pending_state_error<E: std::fmt::Display>(err: &E) -> bool {
    err.to_string()
        .contains("state not available for pending block")
}

/// Whether the fill died because the chain predates EIP-1559.
///
/// Pre-1559 chains (Kava) return a null `baseFeePerGas` from
/// `eth_feeHistory`, and alloy's 1559 estimator deserializes that field as a
/// `u128` and fails hard, before any legacy fallback of its own runs, so the
/// only signal is the deserialization error text.
pub fn is_missing_base_fee_error<E: std::fmt::Display>(err: &E) -> bool {
    let text = err.to_string();
    text.contains("expected a 16 byte hex string") && text.contains("invalid type: null")
}

/// Broadcast rejections that mean the transaction (or its nonce) is already
/// on-chain or in the mempool — i.e. an earlier attempt may have landed even
/// though its RPC response was lost (Hedera's consensus-node "timeout
/// exceeded" landing pattern surfaces as "nonce too low" on the re-send).
/// The pre-known envelope hash then settles the question definitively.
pub fn send_error_may_mean_landed<E: std::fmt::Display>(err: &E) -> bool {
    let message = err.to_string().to_lowercase();
    [
        "nonce too low",
        "already known",
        "already exists",
        "already imported",
        // Cosmos-EVM chains (XRPL EVM) phrase the same rejection via CometBFT.
        "already in mempool",
    ]
    .iter()
    .any(|signature| message.contains(signature))
}

/// Broadcast rejections meaning a *different*, already-pooled transaction owns
/// this nonce at a fee ours does not beat.
///
/// Distinct from [`send_error_may_mean_landed`]: the node never accepted our
/// bytes, so ours cannot have landed, and re-signing immediately just loses the
/// same race again. Re-broadcasting identical bytes yields "already known", not
/// these - so this only fires when something else holds the nonce, which on a
/// wallet shared by concurrent runs means waiting for that transaction to mine
/// or expire and then re-filling at a fresh nonce.
pub fn nonce_held_by_pooled_tx<E: std::fmt::Display>(err: &E) -> bool {
    let message = err.to_string().to_lowercase();
    [
        "replacement transaction underpriced",
        "existing transaction had higher priority",
    ]
    .iter()
    .any(|signature| message.contains(signature))
}

/// Parse the node-expected nonce out of a geth-style "nonce too low: next
/// nonce N, tx nonce M" error so a stale-nonce send can be retried at the
/// value the node actually wants.
pub fn parse_expected_nonce(err: &str) -> Option<u64> {
    let idx = err.find("next nonce ")?;
    let rest = &err[idx + "next nonce ".len()..];
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// Rounds of fill→broadcast→confirm in [`send_tx_robust`]. Each round already
/// carries full per-phase retry + endpoint fallback, so rounds only replay for
/// nonce re-resolution or an unmined-after-timeout re-broadcast.
const SEND_ROUNDS: u32 = 3;

/// Retries spent waiting out a nonce another pooled transaction holds:
/// 1 original try + 5 backed-off retries, the repo-wide retry budget.
/// Separate from [`SEND_ROUNDS`]: losing a nonce race costs a short sleep, not
/// a `receipt_timeout`.
const NONCE_CONTENTION_ATTEMPTS: u32 = 5;

/// Hard ceiling on loop iterations, so no re-sign path can spin forever.
const MAX_SEND_ATTEMPTS: u32 = 16;

/// Backoff for the nth nonce-contention attempt: the shared 4, 8, 16, 32,
/// 64 s schedule ([`crate::retry`]), jittered so two runs colliding on a
/// nonce do not wake together and collide again.
fn nonce_contention_backoff(attempt: u32) -> std::time::Duration {
    crate::retry::backoff_for_attempt(attempt)
}

/// Fee floor carried into a replacement broadcast round: the previous
/// envelope's fees, doubled. Doubling clears both the mempool replacement
/// minimum (typically +10%) and a moderate fee spike in one step, and the
/// rounds are capped by [`SEND_ROUNDS`].
struct ReplacementFees {
    gas_price: Option<u128>,
    max_fee_per_gas: Option<u128>,
    max_priority_fee_per_gas: Option<u128>,
}

impl ReplacementFees {
    fn doubled_from(env: &TxEnvelope) -> Self {
        if let Some(price) = env.gas_price() {
            // Legacy (type-0) envelope.
            Self {
                gas_price: Some(price.saturating_mul(2)),
                max_fee_per_gas: None,
                max_priority_fee_per_gas: None,
            }
        } else {
            Self {
                gas_price: None,
                max_fee_per_gas: Some(env.max_fee_per_gas().saturating_mul(2)),
                max_priority_fee_per_gas: env
                    .max_priority_fee_per_gas()
                    .map(|tip| tip.saturating_mul(2)),
            }
        }
    }

    fn apply(&self, request: &mut TransactionRequest) {
        if let Some(price) = self.gas_price {
            request.gas_price = Some(price);
        }
        if let Some(fee) = self.max_fee_per_gas {
            request.max_fee_per_gas = Some(fee);
        }
        if let Some(tip) = self.max_priority_fee_per_gas {
            request.max_priority_fee_per_gas = Some(tip);
        }
    }
}

/// Send an EVM transaction with retry + private→public fallback at every
/// phase, and at-most-once semantics end to end:
///
/// 1. **Fill + sign** (nonce, gas, fees) — pure reads, retried across
///    endpoints. Signing locally fixes the tx hash *before* any broadcast.
/// 2. **Broadcast raw bytes** — retried across endpoints; identical bytes
///    hash to the same tx, so re-broadcast can never double-send. A
///    "nonce too low"/"already known" rejection is checked against our own
///    pre-known hash: if the tx is on-chain or in the pool the send *already
///    succeeded* (the classic lost-response landing); otherwise the nonce was
///    stale — re-resolve it and re-sign.
/// 3. **Confirm by hash** — receipt polled across endpoints (tolerant of
///    per-endpoint errors) up to `receipt_timeout`; an unmined tx is
///    re-broadcast (same bytes) for another round.
///
/// Returns the mined receipt — reverted (status 0x0) receipts are returned,
/// not errors, so callers keep their own status handling.
pub async fn send_tx_robust(
    endpoints: &EvmEndpoints,
    signer: &PrivateKeySigner,
    tx: TransactionRequest,
    label: &str,
    receipt_timeout: std::time::Duration,
) -> Result<alloy::rpc::types::TransactionReceipt> {
    let mut envelope: Option<TxEnvelope> = None;
    let mut nonce_override: Option<u64> = tx.nonce;
    // Fee floor for a replacement round: an unmined-after-timeout tx is
    // usually underbidding the current market, and re-broadcasting the same
    // bytes cannot change that. The floor doubles the previous envelope's
    // fees so the re-signed tx (same nonce) cleanly replaces the pooled one.
    let mut replacement_fees: Option<ReplacementFees> = None;
    let mut last_hash: Option<TxHash> = None;
    // Broadcast rounds and nonce-contention attempts are budgeted separately:
    // only an unmined-after-`receipt_timeout` tx consumes a broadcast round.
    let mut round: u32 = 0;
    let mut contention: u32 = 0;
    // Contention rotates the preferred endpoint, so a wallet-level race is not
    // re-fought against the same node's mempool view every time.
    let mut pool = endpoints.clone();
    for _ in 0..MAX_SEND_ATTEMPTS {
        if round >= SEND_ROUNDS {
            break;
        }
        let env = match envelope.clone() {
            Some(env) => env,
            None => {
                let mut request = tx.clone();
                request.nonce = nonce_override;
                if let Some(fees) = &replacement_fees {
                    fees.apply(&mut request);
                }
                let env = pool.fill_and_sign(signer, request).await?;
                envelope = Some(env.clone());
                env
            }
        };
        let tx_hash = *env.tx_hash();
        last_hash = Some(tx_hash);

        match pool.broadcast_raw(&env).await {
            Ok(_) => {}
            Err(error) if send_error_may_mean_landed(&error) => {
                if !tx_known_by_hash(&pool, tx_hash).await {
                    // Not ours: the fill used a stale nonce. Re-resolve (from
                    // the node's own report when present) and re-sign.
                    nonce_override = parse_expected_nonce(&error.to_string());
                    envelope = None;
                    replacement_fees = None;
                    ui::warn(&format!(
                        "{label}: nonce conflict ({error}); re-signing at {}",
                        match nonce_override {
                            Some(nonce) => format!("node-reported nonce {nonce}"),
                            None => "freshly-fetched nonce".to_string(),
                        }
                    ));
                    continue;
                }
                // Ours: an earlier broadcast landed despite its lost response.
                // Fall through to confirmation.
            }
            // Something else holds this nonce: a previous run's dropped-but-
            // still-pooled send (Monad), or a concurrent run on the same wallet
            // that got there first (the cron matrix schedules several routes
            // per source chain in parallel). Either way the node refused ours,
            // so it cannot have landed. Wait for the pooled tx to mine or
            // expire, rotate to the next endpoint, and re-fill - the fresh
            // nonce query then lands past it.
            Err(error) if nonce_held_by_pooled_tx(&error) => {
                if contention >= NONCE_CONTENTION_ATTEMPTS {
                    return Err(error.wrap_err(format!(
                        "{label}: nonce still held after {NONCE_CONTENTION_ATTEMPTS} \
                         backoff attempts"
                    )));
                }
                let backoff = nonce_contention_backoff(contention);
                contention += 1;
                pool = endpoints.rotated(contention as usize);
                ui::warn(&format!(
                    "{label}: nonce held by another pooled tx — waiting {:.1}s, then \
                     re-signing at a fresh nonce on the next endpoint \
                     (attempt {contention}/{NONCE_CONTENTION_ATTEMPTS})",
                    backoff.as_secs_f64(),
                ));
                tokio::time::sleep(backoff).await;
                nonce_override = None;
                envelope = None;
                replacement_fees = None;
                continue;
            }
            Err(error) => return Err(error),
        }

        match wait_receipt_by_hash(&pool, tx_hash, receipt_timeout).await {
            Some(receipt) => return Ok(receipt),
            None => {
                round += 1;
                // Re-sign a replacement at the SAME nonce with doubled fees
                // instead of re-broadcasting identical (likely underpriced)
                // bytes. Same-nonce replacement is double-send-safe: at most
                // one of the two can ever mine.
                if let Some(env) = envelope.take() {
                    nonce_override = Some(env.nonce());
                    replacement_fees = Some(ReplacementFees::doubled_from(&env));
                }
                ui::warn(&format!(
                    "{label}: tx {tx_hash:#x} not mined in {}s — re-signing a replacement \
                     at the same nonce with doubled fees (round {}/{SEND_ROUNDS})",
                    receipt_timeout.as_secs(),
                    round + 1,
                ));
            }
        }
    }
    Err(eyre::eyre!(
        "{label}: no receipt after {SEND_ROUNDS} broadcast rounds{}",
        match last_hash {
            Some(hash) => format!(" (last tx {hash:#x})"),
            None => String::new(),
        }
    ))
}

/// Whether the transaction is visible on any endpoint — mined or pending in
/// the mempool. Used to distinguish "our lost-response broadcast landed" from
/// "the nonce was consumed by something else".
async fn tx_known_by_hash(endpoints: &EvmEndpoints, tx_hash: TxHash) -> bool {
    for provider in endpoints.providers() {
        if let Ok(Some(_)) = provider.get_transaction_by_hash(tx_hash).await {
            return true;
        }
    }
    false
}

/// Poll every endpoint for the receipt until `timeout`. Per-endpoint errors
/// (429s, flaky nodes) are tolerated by construction — the next endpoint or
/// the next tick retries.
pub async fn wait_receipt_by_hash(
    endpoints: &EvmEndpoints,
    tx_hash: TxHash,
    timeout: std::time::Duration,
) -> Option<alloy::rpc::types::TransactionReceipt> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        for provider in endpoints.providers() {
            if let Ok(Some(receipt)) = provider.get_transaction_receipt(tx_hash).await {
                return Some(receipt);
            }
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
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

#[cfg(test)]
mod tests {
    use super::{
        is_pending_state_error, nonce_contention_backoff, nonce_held_by_pooled_tx,
        parse_expected_nonce, send_error_may_mean_landed,
    };

    #[test]
    fn parses_geth_nonce_too_low() {
        // The exact Avalanche/QuikNode wording that failed the cron.
        let err = "server returned an error response: error code -32000: nonce too low: \
                   next nonce 246, tx nonce 245";
        assert_eq!(parse_expected_nonce(err), Some(246));
    }

    #[test]
    fn none_when_no_next_nonce_phrase() {
        assert_eq!(parse_expected_nonce("nonce too low"), None);
        assert_eq!(parse_expected_nonce("some unrelated error"), None);
    }

    #[test]
    fn pending_state_error_is_detected() {
        // Avalanche coreth pending-state rejection (the recurring testnet one).
        assert!(is_pending_state_error(
            &"error code -32000: state not available for pending block"
        ));
        assert!(!is_pending_state_error(
            &"execution reverted: TakeTokenFailed"
        ));
    }

    #[test]
    fn landed_signatures_are_detected() {
        // Hedera lost-response landing surfaces as "Nonce too low" on re-send.
        assert!(send_error_may_mean_landed(
            &"HTTP error 400: [Request ID: x] Nonce too low. Provided nonce: 5, current nonce: 6"
        ));
        assert!(send_error_may_mean_landed(&"already known"));
        // XRPL EVM (cosmos-evm) re-broadcast of identical bytes, observed in
        // cron run 32638549694.
        assert!(send_error_may_mean_landed(
            &"server returned an error response: error code -32000: failed to \
              broadcast transaction: : tx already in mempool"
        ));
        assert!(!send_error_may_mean_landed(&"429 too many requests"));
        assert!(!send_error_may_mean_landed(
            &"execution reverted: TakeTokenFailed"
        ));
    }

    #[test]
    fn pooled_nonce_holder_is_not_treated_as_landed() {
        // A concurrent run on the same wallet won the nonce. Ours was never
        // accepted, so this must not take the "may have landed" path - that
        // re-signs immediately and loses the race again.
        let underpriced = "server returned an error response: error code -32000: \
                           replacement transaction underpriced";
        assert!(nonce_held_by_pooled_tx(&underpriced));
        assert!(!send_error_may_mean_landed(&underpriced));

        assert!(nonce_held_by_pooled_tx(
            &"existing transaction had higher priority"
        ));
        assert!(!nonce_held_by_pooled_tx(&"nonce too low"));
        assert!(!nonce_held_by_pooled_tx(&"already known"));
    }

    #[test]
    fn contention_backoff_doubles_within_jitter_and_caps() {
        // 4, 8, 16, 32, 64 s, each within +/-20%.
        for (attempt, base) in [(0, 4.0), (1, 8.0), (2, 16.0), (3, 32.0), (4, 64.0)] {
            for _ in 0..50 {
                let secs = nonce_contention_backoff(attempt).as_secs_f64();
                assert!(
                    secs >= base * 0.8 && secs <= base * 1.2,
                    "attempt {attempt} gave {secs}s, outside +/-20% of {base}s"
                );
            }
        }
        // Past the schedule it stays at the 64 s step rather than growing.
        let capped = nonce_contention_backoff(9).as_secs_f64();
        assert!((51.2..=76.8).contains(&capped), "cap gave {capped}s");
    }

    #[test]
    fn contention_backoff_is_jittered_not_fixed() {
        // Two runs colliding on a nonce must not wait the identical time, or
        // they just collide again on the retry.
        let samples: Vec<u64> = (0..25)
            .map(|_| nonce_contention_backoff(3).as_millis() as u64)
            .collect();
        assert!(
            samples
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len()
                > 1,
            "backoff produced a single fixed value: {samples:?}"
        );
    }

    #[test]
    fn rotation_advances_the_preferred_endpoint() {
        let pool = super::EvmEndpoints::connect(&[
            "http://a.invalid".to_string(),
            "http://b.invalid".to_string(),
            "http://c.invalid".to_string(),
        ])
        .expect("pool");
        assert_eq!(
            pool.urls(),
            ["http://a.invalid", "http://b.invalid", "http://c.invalid"]
        );
        assert_eq!(
            pool.rotated(1).urls(),
            ["http://b.invalid", "http://c.invalid", "http://a.invalid"]
        );
        // Wraps, so an attempt counter past the pool size stays in range.
        assert_eq!(pool.rotated(4).urls(), pool.rotated(1).urls());

        let single =
            super::EvmEndpoints::connect(&["http://only.invalid".to_string()]).expect("pool");
        assert_eq!(single.rotated(3).urls(), ["http://only.invalid"]);
    }
}
