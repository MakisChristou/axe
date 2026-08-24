//! `XrplClient` — thin wrapper over `xrpl_http_client::Client` that builds,
//! signs, submits and polls XRPL `Payment` transactions, including the
//! Axelar ITS interchain-transfer flow and the `account_tx` scan used to
//! match inbound `message_id` memos on the destination side.

use std::time::Duration;

use eyre::{Result, eyre};
use xrpl_api::{AccountInfoRequest, SubmitRequest, TxRequest};
use xrpl_binary_codec::{serialize, sign::sign_transaction};
use xrpl_types::{AccountId, Amount, PaymentTransaction};

use super::helpers::signed_tx_hash_hex;
use super::wallet::XrplWallet;

/// Poll interval while waiting for a submitted tx to be validated on the
/// ledger. XRPL closes ledgers ~every 3–4s, so 2s is a reasonable cadence.
const POLL_INTERVAL: Duration = Duration::from_secs(2);
/// Upper bound on how long we wait for a single tx to validate.
const VALIDATE_TIMEOUT: Duration = Duration::from_secs(60);

/// `LastLedgerSequence` bump applied on top of whatever
/// `prepare_transaction` autofills. The SDK sets `validated + 4` (~16 s),
/// which expires too easily under any one-ledger delay. xrpl.js autofill
/// defaults to +20; we add +26 here to leave a comfortable window for
/// load-test bursts that may queue behind several congested closes.
pub const LAST_LEDGER_SEQUENCE_BUMP: u32 = 26;

/// Maximum txs returned by `account_tx` when scanning for an inbound
/// `Payment` carrying a particular `message_id` memo. The XRPL public
/// servers cap responses lower (200 is the documented ceiling), so this is
/// also the practical lookback window.
const ACCOUNT_TX_LIMIT: u32 = 200;

#[derive(Clone)]
pub struct XrplClient {
    inner: xrpl_http_client::Client,
    rpc_url: String,
}

impl XrplClient {
    pub fn new(rpc_url: &str) -> Self {
        let inner = xrpl_http_client::Client::builder()
            .base_url(rpc_url)
            .http_client(crate::http::client().clone())
            .build();
        Self {
            inner,
            rpc_url: rpc_url.to_string(),
        }
    }

    pub fn rpc_url(&self) -> &str {
        &self.rpc_url
    }

    pub fn inner(&self) -> &xrpl_http_client::Client {
        &self.inner
    }

    /// Fetch the current balance (drops) and next sequence for an account.
    /// Returns `None` if the account does not exist (unactivated).
    ///
    /// Wrapped in `retry_all`: the public XRPL clusters (e.g.
    /// `s1.ripple.com`) intermittently drop connections, and an
    /// unretried failure here aborts the whole route (observed killing
    /// XRPL EVM → XRPL on a transient `error sending request`). A genuine
    /// `actNotFound` maps to `Ok(None)` *before* the retry layer sees it,
    /// so non-existent accounts don't burn the retry budget.
    pub async fn account_info(&self, address: &str) -> Result<Option<AccountInfo>> {
        crate::retry::retry_all("xrpl.account_info", || async {
            let req = AccountInfoRequest::new(address);
            match self.inner.call(req).await {
                Ok(resp) => Ok(Some(AccountInfo {
                    balance_drops: resp
                        .account_data
                        .balance
                        .as_deref()
                        .and_then(|s| s.parse::<u64>().ok())
                        .unwrap_or(0),
                    sequence: resp.account_data.sequence,
                })),
                Err(e) => {
                    // `actNotFound` → account doesn't exist yet (not an error)
                    if matches!(
                        &e,
                        xrpl_http_client::error::Error::Api(code) if code == "actNotFound"
                    ) {
                        Ok(None)
                    } else {
                        Err(eyre!("account_info({address}) failed: {e}"))
                    }
                }
            }
        })
        .await
    }

    /// Fund an XRPL account via the public testnet/devnet faucet.
    ///
    /// * `faucet_url` — e.g. `https://faucet.altnet.rippletest.net/accounts`
    pub async fn fund_from_faucet(&self, address: &str, faucet_url: &str) -> Result<()> {
        let client = crate::http::client();
        let body = serde_json::json!({ "destination": address });
        let resp = client
            .post(faucet_url)
            .json(&body)
            .send()
            .await
            .map_err(|e| eyre!("faucet request failed: {e}"))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(eyre!(
                "faucet returned {status}: {}",
                text.chars().take(200).collect::<String>()
            ));
        }
        Ok(())
    }

    /// Send a simple XRP Payment with no memos. Used by the funding code to
    /// activate ephemeral load-test wallets from the main wallet.
    pub async fn submit_plain_payment(
        &self,
        wallet: &XrplWallet,
        destination: &AccountId,
        amount_drops: u64,
    ) -> Result<String> {
        let amount = Amount::drops(amount_drops)
            .map_err(|e| eyre!("invalid Amount::drops({amount_drops}): {e}"))?;
        let mut tx = PaymentTransaction::new(wallet.account_id, amount, *destination);

        // `prepare_transaction` autofills Sequence/Fee/LastLedgerSequence via
        // read-only RPCs, so it's safe to retry: run each attempt on a clone
        // and commit the prepared copy only on success.
        let prepared = crate::retry::retry_all("xrpl.prepare_transaction", || {
            let mut common = tx.common.clone();
            let inner = &self.inner;
            async move {
                inner
                    .prepare_transaction(&mut common)
                    .await
                    .map(|()| common)
            }
        })
        .await
        .map_err(|e| eyre!("prepare_transaction failed: {e}"))?;
        tx.common = prepared;
        if let Some(lls) = tx.common.last_ledger_sequence {
            tx.common.last_ledger_sequence = Some(lls.saturating_add(LAST_LEDGER_SEQUENCE_BUMP));
        }
        sign_transaction(&mut tx, &wallet.public_key, &wallet.secret_key)
            .map_err(|e| eyre!("sign_transaction failed: {e:?}"))?;

        let tx_bytes = serialize::serialize(&tx).map_err(|e| eyre!("serialize failed: {e:?}"))?;
        let tx_blob = hex::encode_upper(&tx_bytes);
        let tx_hash = signed_tx_hash_hex(&tx_bytes);

        let req = SubmitRequest::new(tx_blob).fail_hard(true);
        self.submit_signed_blob(&req, &tx_hash).await?;
        Ok(tx_hash)
    }

    /// Submit an already-signed transaction blob, retrying transient
    /// transport errors. Re-POSTing the same blob is idempotent — XRPL
    /// dedups on Sequence — so retrying here can never double-send; a
    /// fresh prepare→sign per attempt would mint a new Sequence and could.
    ///
    /// A lost submit response does not mean the tx failed: on transport
    /// exhaustion, or a `tefPAST_SEQ`/`tefALREADY` engine result (this
    /// exact Sequence already consumed), an earlier attempt may have
    /// landed — confirm via the precomputed hash before failing.
    pub async fn submit_signed_blob(&self, req: &SubmitRequest, tx_hash: &str) -> Result<()> {
        let submit_result = crate::retry::retry_async(
            "xrpl.submit",
            crate::retry::FALLBACK_ATTEMPTS,
            crate::retry::is_transient_default,
            || {
                let req = req.clone();
                let inner = &self.inner;
                async move { inner.call(req).await }
            },
        )
        .await;
        match submit_result {
            Ok(resp) => {
                let engine = format!("{:?}", resp.engine_result);
                if engine.contains("tesSUCCESS") {
                    return Ok(());
                }
                let maybe_landed = engine.contains("tefPAST_SEQ") || engine.contains("tefALREADY");
                if maybe_landed && self.landed_by_hash(tx_hash).await {
                    return Ok(());
                }
                Err(eyre!(
                    "submit rejected: {engine}: {}",
                    resp.engine_result_message
                ))
            }
            Err(e) => {
                if self.landed_by_hash(tx_hash).await {
                    Ok(())
                } else {
                    Err(eyre!("submit failed: {e}"))
                }
            }
        }
    }

    /// Bounded check (via [`Self::wait_for_validated`]) that `tx_hash`
    /// validated with `tesSUCCESS` — resolves lost submit responses.
    async fn landed_by_hash(&self, tx_hash: &str) -> bool {
        matches!(self.wait_for_validated(tx_hash).await, Ok(v) if v.success)
    }

    /// Search the recipient account's recent transactions for an incoming
    /// `Payment` carrying a `message_id` memo equal to `target_message_id`
    /// (decoded from hex-encoded UTF-8). Returns the matching tx hash if
    /// found. The XRPL relayer attaches `message_id` and `source_chain`
    /// memos when broadcasting proof-driven payouts to recipients.
    ///
    /// `min_ledger` lets the caller bound the lookback window; pass `None`
    /// to scan the latest 200 txs only.
    pub async fn find_inbound_with_message_id(
        &self,
        recipient: &str,
        target_message_id: &str,
        min_ledger: Option<u32>,
    ) -> Result<Option<String>> {
        let target_lower = target_message_id.trim_start_matches("0x").to_lowercase();

        // Mainnet rippled (e.g. s1.ripple.com:51234) rejects
        // `ledger_index_min/max="-1"` with `invalidParams`. Only set those
        // params when actually constraining the lookback window; otherwise
        // omit them and let the server return the latest validated txs.
        let req = xrpl_api::AccountTxRequest {
            account: recipient.to_string(),
            forward: Some(false),
            ledger_index_min: min_ledger.map(|n| n.to_string()),
            pagination: xrpl_api::RequestPagination {
                limit: Some(ACCOUNT_TX_LIMIT),
                ..Default::default()
            },
            ..Default::default()
        };
        let resp = crate::retry::retry_all("xrpl.account_tx", || {
            let req = req.clone();
            let inner = &self.inner;
            async move { inner.call(req).await }
        })
        .await
        .map_err(|e| eyre!("account_tx({recipient}): {e}"))?;

        for at in resp.transactions {
            if !at.validated {
                continue;
            }
            // We only care about Payments (the relayer broadcasts Payments).
            let common = at.tx.common();
            let Some(memos) = &common.memos else {
                continue;
            };
            for m in memos {
                let memo_type_decoded = m
                    .memo_type
                    .as_deref()
                    .and_then(|h| hex::decode(h).ok())
                    .and_then(|b| String::from_utf8(b).ok());
                if memo_type_decoded.as_deref() != Some("message_id") {
                    continue;
                }
                let memo_data_decoded = m
                    .memo_data
                    .as_deref()
                    .and_then(|h| hex::decode(h).ok())
                    .and_then(|b| String::from_utf8(b).ok());
                if let Some(decoded) = memo_data_decoded
                    && decoded.trim_start_matches("0x").to_lowercase() == target_lower
                    && let Some(hash) = common.hash.clone()
                {
                    return Ok(Some(hash));
                }
            }
        }
        Ok(None)
    }

    /// Poll `tx` until the ledger validates the transaction (or we time out).
    pub async fn wait_for_validated(&self, tx_hash: &str) -> Result<ValidatedTx> {
        let start = std::time::Instant::now();
        loop {
            match self.get_validated_tx(tx_hash).await? {
                Some(v) => return Ok(v),
                None => {
                    if start.elapsed() >= VALIDATE_TIMEOUT {
                        return Err(eyre!(
                            "tx {tx_hash} not validated within {:?}",
                            VALIDATE_TIMEOUT
                        ));
                    }
                    tokio::time::sleep(POLL_INTERVAL).await;
                }
            }
        }
    }

    /// One-shot check: if the tx has been validated, return its result;
    /// otherwise return `None`. Non-`tesSUCCESS` validated txs still return
    /// `Some` with `success=false` so the caller can decide what to do.
    pub async fn get_validated_tx(&self, tx_hash: &str) -> Result<Option<ValidatedTx>> {
        crate::retry::retry_all("xrpl.tx", || async {
            let req = TxRequest::new(tx_hash);
            match self.inner.call(req).await {
                Ok(resp) => {
                    let common = resp.tx.common();
                    if common.validated != Some(true) {
                        return Ok(None);
                    }
                    let success = common
                        .meta
                        .as_ref()
                        .map(|m| m.transaction_result == xrpl_api::TransactionResult::tesSUCCESS)
                        .unwrap_or(false);
                    Ok(Some(ValidatedTx {
                        ledger_index: common.ledger_index,
                        success,
                    }))
                }
                Err(e) => {
                    // `txnNotFound` means the tx is not yet on a validated
                    // ledger (or has been dropped). Mapped to `Ok(None)`
                    // *before* the retry layer sees it, so poll loops don't
                    // burn the retry budget on "not yet".
                    if matches!(
                        &e,
                        xrpl_http_client::error::Error::Api(code) if code == "txnNotFound"
                    ) {
                        Ok(None)
                    } else {
                        Err(eyre!("tx({tx_hash}) failed: {e}"))
                    }
                }
            }
        })
        .await
    }
}

#[derive(Debug, Clone, Copy)]
pub struct AccountInfo {
    pub balance_drops: u64,
    pub sequence: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct ValidatedTx {
    pub ledger_index: Option<u32>,
    pub success: bool,
}
