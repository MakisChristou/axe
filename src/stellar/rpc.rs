//! `StellarClient` — the JSON-RPC + simulate + submit + poll wrapper used by
//! axe to drive Soroban `InvokeHostFunction` operations (`AxelarGateway.call_contract`,
//! ITS deploys/transfers, SAC token ops). Also houses the response-parsing
//! helpers that walk `GetTransactionResponse.result_meta` to recover Soroban
//! return values and contract-event indices.

use std::time::{Duration, Instant};

use ed25519_dalek::Signer;
use eyre::{Result, eyre};
use sha2::{Digest, Sha256};
use stellar_rpc_client::Client as RpcClient;
use stellar_xdr::curr::{
    AccountId, Asset, BytesM, ContractId, CreateAccountOp, DecoratedSignature, Hash, HostFunction,
    InvokeContractArgs, InvokeHostFunctionOp, Limits, Memo, MuxedAccount, Operation, OperationBody,
    PaymentOp, Preconditions, PublicKey, ScAddress, ScVal, SequenceNumber, Signature,
    SorobanAuthorizationEntry, Transaction, TransactionEnvelope, TransactionExt, TransactionMeta,
    TransactionSignaturePayload, TransactionSignaturePayloadTaggedTransaction,
    TransactionV1Envelope, Uint256, VecM, WriteXdr,
};

use super::scval::{
    parse_contract_id, scval_address_account, scval_address_from_str, scval_bytes,
    scval_i128_from_u128, scval_string, scval_symbol, scval_to_address_string, scval_to_bytes32,
    scval_to_u32, scval_to_u128, scval_token, scval_token_metadata, scval_void,
};
use super::tx::InvokedTx;
use super::wallet::{StellarWallet, network_passphrase_for};

/// Default poll cadence after submit.
const POLL_INTERVAL: Duration = Duration::from_secs(2);
// Stellar bills `min(submitted_fee, market_clearing_fee)` per op — a high
// submitted fee buys priority but doesn't actually pay more when the
// market clears lower. Horizon `/fee_stats` mainnet p99 is currently
// ~18k stroops; setting floors well above p99 keeps submissions accepted
// during surge pricing without raising the real cost.

/// Higher base fee for ITS transfers — the simulate step still tops it up
/// with the actual resource fee, but a generous floor avoids `txInsufficientFee`
/// rejections when the network bumps fees mid-test.
const BASE_FEE_ITS: u32 = 200_000;
/// Base fee for view-only invokes (`balance`, `interchain_token_address`,
/// gateway `is_message_*`). Simulation discards the fee anyway, so this is
/// just the floor we pass into the envelope before the resource-fee top-up.
const BASE_FEE_VIEW: u32 = 100;
/// Base fee for SAC token ops (`transfer`, `balance`).
const BASE_FEE_TOKEN_OP: u32 = 100_000;
/// Base fee for ITS deploy/register flows. Soroban deploy is heavier than a
/// transfer so the floor is bumped up to avoid `txInsufficientFee` when fees
/// spike.
const BASE_FEE_DEPLOY: u32 = 200_000;
/// Upper bound for waiting on a single tx to validate. Stellar ledger close
/// time is ~5s; a generous window protects against RPC lag without hiding
/// real failures.
const VALIDATE_TIMEOUT: Duration = Duration::from_secs(180);

#[derive(Clone)]
pub struct StellarClient {
    pub rpc: RpcClient,
    pub network_id: [u8; 32],
}

impl StellarClient {
    pub fn new(rpc_url: &str, network_type: &str) -> Result<Self> {
        let rpc = RpcClient::new(rpc_url)
            .map_err(|e| eyre!("failed to build Stellar RPC client: {e}"))?;
        let network_passphrase = network_passphrase_for(network_type);
        let network_id: [u8; 32] = Sha256::digest(network_passphrase.as_bytes()).into();
        Ok(Self { rpc, network_id })
    }

    /// Fetch the current account sequence number. Returns `None` if the
    /// account is unfunded / missing.
    pub async fn account_sequence(&self, address: &str) -> Result<Option<i64>> {
        match self.rpc.get_account(address).await {
            Ok(entry) => Ok(Some(entry.seq_num.0)),
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("not found")
                    || msg.contains("NotFound")
                    || msg.contains("Account not found")
                {
                    Ok(None)
                } else {
                    Err(eyre!("get_account({address}) failed: {msg}"))
                }
            }
        }
    }

    /// Activate `destination` by submitting a classic `CreateAccount` op
    /// signed by `funder`. Used on mainnet (where Friendbot doesn't exist)
    /// to bootstrap derived ephemeral signing keys from the main wallet.
    ///
    /// `starting_balance_stroops` is the initial XLM transferred (1e7 stroops =
    /// 1 XLM). Stellar requires at least the base reserve (1 XLM today) — the
    /// caller should pick a value above that with enough headroom for the
    /// derived key's expected fees + Soroban auth costs.
    pub async fn create_account_classic(
        &self,
        funder: &StellarWallet,
        destination_pubkey: &[u8; 32],
        starting_balance_stroops: i64,
    ) -> Result<String> {
        let seq = self
            .account_sequence(&funder.address())
            .await?
            .ok_or_else(|| {
                eyre!(
                    "funder Stellar account {} is not activated",
                    funder.address()
                )
            })?;

        let destination = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(
            *destination_pubkey,
        )));
        let op = Operation {
            source_account: None,
            body: OperationBody::CreateAccount(CreateAccountOp {
                destination,
                starting_balance: starting_balance_stroops,
            }),
        };
        let ops: VecM<Operation, 100> = vec![op].try_into().map_err(|e| eyre!("ops: {e}"))?;
        let tx = Transaction {
            source_account: funder.muxed_account(),
            // Classic ops use only the per-op base fee. Stellar mainnet base
            // fee is 100 stroops/op; bump it 10x for headroom against
            // network fee spikes — still cheap (1000 stroops = 0.0001 XLM).
            fee: 1_000,
            seq_num: SequenceNumber(seq + 1),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: ops,
            ext: TransactionExt::V0,
        };
        let signed = self.sign(funder, tx)?;
        let hash = self
            .rpc
            .send_transaction(&signed)
            .await
            .map_err(|e| eyre!("send_transaction: {e}"))?;
        let tx_hash_hex = hex::encode(hash.0);

        let start = Instant::now();
        loop {
            if let Ok(resp) = self.rpc.get_transaction(&hash).await
                && let Some(status) = extract_status(&resp)
            {
                match status.as_str() {
                    "SUCCESS" => return Ok(tx_hash_hex),
                    "FAILED" => {
                        return Err(eyre!("create_account tx {tx_hash_hex} failed on chain"));
                    }
                    _ => {}
                }
            }
            if start.elapsed() >= VALIDATE_TIMEOUT {
                return Err(eyre!(
                    "create_account tx {tx_hash_hex} not validated within {:?}",
                    VALIDATE_TIMEOUT
                ));
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    /// Native-XLM balance of `address` in stroops. Returns `None` if the
    /// account does not exist yet.
    pub async fn native_balance_stroops(&self, address: &str) -> Result<Option<i64>> {
        match self.rpc.get_account(address).await {
            Ok(entry) => Ok(Some(entry.balance)),
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("not found")
                    || msg.contains("NotFound")
                    || msg.contains("Account not found")
                {
                    Ok(None)
                } else {
                    Err(eyre!("get_account({address}) balance: {msg}"))
                }
            }
        }
    }

    /// Send `amount_stroops` of native XLM from `funder` to `destination`
    /// using a classic Payment op. Used to top up derived signers that were
    /// previously activated but are now below the threshold for their next
    /// run (e.g. spent their gas budget).
    pub async fn pay_native_classic(
        &self,
        funder: &StellarWallet,
        destination_pubkey: &[u8; 32],
        amount_stroops: i64,
    ) -> Result<String> {
        // Multiple parallel cron jobs share `STELLAR_PRIVATE_KEY`, so two
        // concurrent `pay_native_classic`s on the same wallet fetch the same
        // server-side sequence and both submit `seq + 1`. One wins, the other
        // gets TxBadSeq. Re-fetch on TxBadSeq up to 4 times — by then the
        // colliding tx has either settled or been dropped.
        const MAX_SEQ_RETRIES: u8 = 4;
        let mut attempt: u8 = 0;
        let hash = loop {
            let seq = self
                .account_sequence(&funder.address())
                .await?
                .ok_or_else(|| {
                    eyre!(
                        "funder Stellar account {} is not activated",
                        funder.address()
                    )
                })?;

            let destination = MuxedAccount::Ed25519(Uint256(*destination_pubkey));
            let op = Operation {
                source_account: None,
                body: OperationBody::Payment(PaymentOp {
                    destination,
                    asset: Asset::Native,
                    amount: amount_stroops,
                }),
            };
            let ops: VecM<Operation, 100> = vec![op].try_into().map_err(|e| eyre!("ops: {e}"))?;
            let tx = Transaction {
                source_account: funder.muxed_account(),
                fee: 1_000,
                seq_num: SequenceNumber(seq + 1),
                cond: Preconditions::None,
                memo: Memo::None,
                operations: ops,
                ext: TransactionExt::V0,
            };
            let signed = self.sign(funder, tx)?;
            match self.rpc.send_transaction(&signed).await {
                Ok(hash) => break hash,
                Err(e) if attempt + 1 < MAX_SEQ_RETRIES && format!("{e}").contains("TxBadSeq") => {
                    crate::ui::warn(&format!(
                        "stellar pay_native_classic: TxBadSeq on attempt {} \
                         (concurrent funder use? re-fetching sequence)",
                        attempt + 1,
                    ));
                    // Brief backoff so the colliding tx settles.
                    tokio::time::sleep(Duration::from_millis(500u64 << attempt)).await;
                    attempt += 1;
                    continue;
                }
                Err(e) => return Err(eyre!("send_transaction: {e}")),
            }
        };
        let tx_hash_hex = hex::encode(hash.0);

        let start = Instant::now();
        loop {
            if let Ok(resp) = self.rpc.get_transaction(&hash).await
                && let Some(status) = extract_status(&resp)
            {
                match status.as_str() {
                    "SUCCESS" => return Ok(tx_hash_hex),
                    "FAILED" => {
                        return Err(eyre!("payment tx {tx_hash_hex} failed on chain"));
                    }
                    _ => {}
                }
            }
            if start.elapsed() >= VALIDATE_TIMEOUT {
                return Err(eyre!(
                    "payment tx {tx_hash_hex} not validated within {:?}",
                    VALIDATE_TIMEOUT
                ));
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    /// Fund a testnet account via Friendbot. On non-testnet networks, this
    /// will error — callers should have a main wallet fund instead.
    pub async fn friendbot_fund(&self, address: &str) -> Result<()> {
        let friendbot = self
            .rpc
            .friendbot_url()
            .await
            .map_err(|e| eyre!("friendbot_url: {e}"))?;
        let url = format!("{friendbot}?addr={address}");
        let resp = reqwest::get(&url)
            .await
            .map_err(|e| eyre!("friendbot GET failed: {e}"))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(eyre!(
                "friendbot returned {status}: {}",
                text.chars().take(300).collect::<String>()
            ));
        }
        Ok(())
    }

    /// One build → simulate → sign → submit attempt for
    /// [`Self::invoke_contract`]. Reads the account sequence fresh, so retrying
    /// it after a `TxBadSeq` picks up the advanced sequence. Returns the
    /// submitted tx hash; a `TxBadSeq` surfaces as an `Err` whose string the
    /// caller matches on to decide whether to retry.
    async fn submit_invoke_once(
        &self,
        wallet: &StellarWallet,
        invoke: &InvokeContractArgs,
        base_fee: u32,
    ) -> Result<Hash> {
        // 1. Read account sequence
        let seq = self
            .account_sequence(&wallet.address())
            .await?
            .ok_or_else(|| {
                eyre!(
                    "Stellar account {} is not activated — fund it via friendbot first",
                    wallet.address()
                )
            })?;

        let op = Operation {
            source_account: None,
            body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
                host_function: HostFunction::InvokeContract(invoke.clone()),
                auth: VecM::default(),
            }),
        };
        let ops: VecM<Operation, 100> =
            vec![op].try_into().map_err(|e| eyre!("tx op build: {e}"))?;

        let mut tx = Transaction {
            source_account: wallet.muxed_account(),
            fee: base_fee,
            seq_num: SequenceNumber(seq + 1),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: ops,
            ext: TransactionExt::V0,
        };

        // 2. Simulate to discover footprint, auth, and resource fee.
        let envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx: tx.clone(),
            signatures: VecM::default(),
        });
        let sim = self
            .rpc
            .simulate_transaction_envelope(&envelope, None)
            .await
            .map_err(|e| eyre!("simulate_transaction_envelope: {e}"))?;
        if let Some(err) = &sim.error {
            return Err(eyre!("Stellar simulation failed: {err}"));
        }

        // 3. Merge footprint + auth + fee back into the tx.
        let soroban_data = sim
            .transaction_data()
            .map_err(|e| eyre!("decode transaction_data: {e}"))?;
        tx.ext = TransactionExt::V1(soroban_data);
        tx.fee = base_fee.saturating_add(sim.min_resource_fee.min(u64::from(u32::MAX)) as u32);

        let sim_results = sim
            .results()
            .map_err(|e| eyre!("decode simulate results: {e}"))?;
        if let Some(first) = sim_results.first()
            && !first.auth.is_empty()
        {
            // VecM is read-only at index — rebuild the ops vec with auth merged.
            let mut ops_vec: Vec<Operation> = tx.operations.to_vec();
            if let Some(op) = ops_vec.first_mut()
                && let OperationBody::InvokeHostFunction(ref mut ihf) = op.body
            {
                let auth_vec: VecM<SorobanAuthorizationEntry> = first
                    .auth
                    .clone()
                    .try_into()
                    .map_err(|e| eyre!("auth too long: {e}"))?;
                ihf.auth = auth_vec;
            }
            tx.operations = ops_vec.try_into().map_err(|e| eyre!("rebuild ops: {e}"))?;
        }

        // 4. Sign.
        let signed = self.sign(wallet, tx)?;

        // 5. Submit.
        self.rpc
            .send_transaction(&signed)
            .await
            .map_err(|e| eyre!("send_transaction: {e}"))
    }

    /// Build + simulate + sign + submit an InvokeContract call, then poll.
    ///
    /// `event_filter` (if provided) is the contract hash of the emitter we
    /// want to locate in the validated tx's event list — axe uses the
    /// `AxelarGateway` hash so the returned `event_index` matches the
    /// `hex_tx_hash_and_event_index` message-id format expected by the
    /// Stellar `VotingVerifier`.
    pub async fn invoke_contract(
        &self,
        wallet: &StellarWallet,
        contract: &str,
        function: &str,
        args: Vec<ScVal>,
        base_fee: u32,
        event_filter: Option<Hash>,
    ) -> Result<InvokedTx> {
        let contract_id = parse_contract_id(contract)?;
        let fn_symbol = scval_symbol(function)?;
        let args_vec: VecM<ScVal> = args
            .try_into()
            .map_err(|e| eyre!("too many args for InvokeContract: {e}"))?;
        let invoke = InvokeContractArgs {
            contract_address: ScAddress::Contract(stellar_xdr::curr::ContractId(contract_id)),
            function_name: fn_symbol,
            args: args_vec,
        };

        // Submit, poll for validation, and — if the tx is *dropped* (accepted
        // by the RPC but never validated within VALIDATE_TIMEOUT) — resubmit,
        // but only when the account sequence proves nothing landed. EVM chains
        // resubmit dropped source txs the same way; the sequence guard makes it
        // safe for the shared deploy path too — a resubmit can never become a
        // second transfer, because an advanced sequence means the slot was
        // already consumed (so we stop instead).
        const MAX_VALIDATE_RESUBMITS: u8 = 2;
        let mut resubmit: u8 = 0;
        loop {
            let seq_before = self
                .account_sequence(&wallet.address())
                .await
                .ok()
                .flatten();

            // A shared `STELLAR_PRIVATE_KEY` (parallel cron jobs) and sequential
            // Soroban txs whose sequence bump the RPC hasn't reflected yet both
            // leave the fetched sequence stale, so a submit races another
            // `seq + 1` and gets TxBadSeq. Re-fetch + re-simulate + resubmit up
            // to 4 times — the same recovery `pay_native_classic` uses.
            const MAX_SEQ_RETRIES: u8 = 4;
            let mut attempt: u8 = 0;
            let hash = loop {
                match self.submit_invoke_once(wallet, &invoke, base_fee).await {
                    Ok(hash) => break hash,
                    Err(e)
                        if attempt + 1 < MAX_SEQ_RETRIES && format!("{e}").contains("TxBadSeq") =>
                    {
                        crate::ui::warn(&format!(
                            "stellar invoke_contract: TxBadSeq on attempt {} \
                             (stale/racing sequence? re-simulating)",
                            attempt + 1,
                        ));
                        // Brief backoff so the colliding tx settles.
                        tokio::time::sleep(Duration::from_millis(500u64 << attempt)).await;
                        attempt += 1;
                        continue;
                    }
                    Err(e) => return Err(e),
                }
            };
            let tx_hash_hex = hex::encode(hash.0);

            if let Some(tx) = self
                .poll_tx_validation(&hash, tx_hash_hex.clone(), event_filter.as_ref())
                .await?
            {
                return Ok(tx);
            }

            // Poll timed out. Resubmit only if the account sequence did NOT
            // advance — proof that no tx from this account landed, so ours is
            // genuinely dropped and a resubmit can't double-send.
            let seq_after = self
                .account_sequence(&wallet.address())
                .await
                .ok()
                .flatten();
            let sequence_advanced = match (seq_before, seq_after) {
                (Some(before), Some(after)) => after > before,
                _ => true, // couldn't read the sequence → don't risk a resubmit
            };
            if sequence_advanced || resubmit + 1 >= MAX_VALIDATE_RESUBMITS {
                return Err(eyre!(
                    "Stellar tx {tx_hash_hex} not validated within {:?}",
                    VALIDATE_TIMEOUT
                ));
            }
            crate::ui::warn(&format!(
                "stellar invoke_contract: tx {tx_hash_hex} not validated in {:?}, account \
                 sequence unchanged (dropped) — resubmitting ({}/{MAX_VALIDATE_RESUBMITS})",
                VALIDATE_TIMEOUT,
                resubmit + 2,
            ));
            resubmit += 1;
        }
    }

    /// Poll a submitted tx until the ledger reports SUCCESS/FAILED, or
    /// `VALIDATE_TIMEOUT` elapses. `Ok(Some(_))` = validated (either outcome);
    /// `Ok(None)` = the deadline passed without the tx appearing (dropped) —
    /// the caller decides whether to resubmit.
    async fn poll_tx_validation(
        &self,
        hash: &Hash,
        tx_hash_hex: String,
        event_filter: Option<&Hash>,
    ) -> Result<Option<InvokedTx>> {
        let start = Instant::now();
        loop {
            if let Ok(resp) = self.rpc.get_transaction(hash).await
                && let Some(status) = extract_status(&resp)
            {
                match status.as_str() {
                    "SUCCESS" => {
                        let event_index = event_filter.and_then(|f| find_event_index(&resp, f));
                        return Ok(Some(InvokedTx {
                            tx_hash_hex,
                            success: true,
                            event_index,
                            return_value: extract_return_value(&resp),
                        }));
                    }
                    "FAILED" => {
                        return Ok(Some(InvokedTx {
                            tx_hash_hex,
                            success: false,
                            event_index: None,
                            return_value: None,
                        }));
                    }
                    _ => {} // NOT_FOUND → keep polling
                }
            }
            if start.elapsed() >= VALIDATE_TIMEOUT {
                return Ok(None);
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    // -----------------------------------------------------------------
    // ITS-specific helpers
    // -----------------------------------------------------------------

    /// `InterchainTokenService.deploy_interchain_token(caller, salt, metadata,
    /// initial_supply, minter)` → returns the new `tokenId` (32 bytes).
    /// Initial supply is minted to `caller`.
    #[allow(clippy::too_many_arguments)]
    pub async fn its_deploy_interchain_token(
        &self,
        wallet: &StellarWallet,
        its_contract: &str,
        salt: [u8; 32],
        decimals: u32,
        name: &str,
        symbol: &str,
        initial_supply: u128,
    ) -> Result<(InvokedTx, Option<[u8; 32]>)> {
        let caller = scval_address_account(&wallet.public_key_bytes);
        let metadata = scval_token_metadata(decimals, name, symbol)?;
        let supply = scval_i128_from_u128(initial_supply);
        let minter = scval_address_account(&wallet.public_key_bytes);
        let salt_arg = scval_bytes(&salt)?;

        let invoked = self
            .invoke_contract(
                wallet,
                its_contract,
                "deploy_interchain_token",
                vec![caller, salt_arg, metadata, supply, minter],
                BASE_FEE_DEPLOY,
                None,
            )
            .await?;
        let token_id = invoked.return_value.as_ref().and_then(scval_to_bytes32);
        Ok((invoked, token_id))
    }

    /// `InterchainTokenService.deploy_remote_interchain_token(caller, salt,
    /// destination_chain, gas_token)` — registers the same token on a
    /// destination chain via the ITS hub.
    #[allow(clippy::too_many_arguments)]
    pub async fn its_deploy_remote_interchain_token(
        &self,
        wallet: &StellarWallet,
        its_contract: &str,
        gateway_contract: &str,
        salt: [u8; 32],
        destination_chain: &str,
        gas_token: &str,
        gas_amount: u64,
    ) -> Result<InvokedTx> {
        let caller = scval_address_account(&wallet.public_key_bytes);
        let dest_chain_v = scval_string(destination_chain)?;
        let gas_v = scval_token(gas_token, gas_amount)?;
        let salt_arg = scval_bytes(&salt)?;

        let gw_filter = parse_contract_id(gateway_contract).ok();
        self.invoke_contract(
            wallet,
            its_contract,
            "deploy_remote_interchain_token",
            vec![caller, salt_arg, dest_chain_v, gas_v],
            BASE_FEE_DEPLOY,
            gw_filter,
        )
        .await
    }

    /// `InterchainTokenService.interchain_transfer(caller, token_id,
    /// destination_chain, destination_address, amount, data, gas_token)`.
    /// `data` is `None` for plain transfers.
    #[allow(clippy::too_many_arguments)]
    pub async fn its_interchain_transfer(
        &self,
        wallet: &StellarWallet,
        its_contract: &str,
        gateway_contract: &str,
        token_id: [u8; 32],
        destination_chain: &str,
        destination_address_bytes: &[u8],
        amount: u128,
        data: Option<&[u8]>,
        gas_token: &str,
        gas_amount: u64,
    ) -> Result<InvokedTx> {
        let caller = scval_address_account(&wallet.public_key_bytes);
        let token_id_v = scval_bytes(&token_id)?;
        let dest_chain_v = scval_string(destination_chain)?;
        let dest_addr_v = scval_bytes(destination_address_bytes)?;
        let amount_v = scval_i128_from_u128(amount);
        let data_v = match data {
            Some(d) => scval_bytes(d)?,
            None => scval_void(),
        };
        let gas_v = scval_token(gas_token, gas_amount)?;

        let gw_filter = parse_contract_id(gateway_contract).ok();
        self.invoke_contract(
            wallet,
            its_contract,
            "interchain_transfer",
            vec![
                caller,
                token_id_v,
                dest_chain_v,
                dest_addr_v,
                amount_v,
                data_v,
                gas_v,
            ],
            BASE_FEE_ITS,
            gw_filter,
        )
        .await
    }

    /// Query `InterchainTokenService.interchain_token_address(token_id)` —
    /// returns the Stellar SAC contract address that owns the token's
    /// balances on this chain.
    pub async fn its_query_token_address(
        &self,
        wallet: &StellarWallet,
        its_contract: &str,
        token_id: [u8; 32],
    ) -> Result<Option<String>> {
        let token_id_v = scval_bytes(&token_id)?;
        let invoked = self
            .invoke_contract(
                wallet,
                its_contract,
                "interchain_token_address",
                vec![token_id_v],
                BASE_FEE_VIEW,
                None,
            )
            .await?;
        Ok(invoked
            .return_value
            .as_ref()
            .and_then(scval_to_address_string))
    }

    /// Simulate `InterchainTokenService.registered_token_address(token_id)`.
    ///
    /// `interchain_token_address` is deterministic and can return an address
    /// before the token is actually registered. This view proves the token ID
    /// is present in ITS storage.
    pub async fn its_registered_token_address_view(
        &self,
        signer_account_pk: &[u8; 32],
        its_contract: &str,
        token_id: [u8; 32],
    ) -> Result<Option<String>> {
        let token_id_v = scval_bytes(&token_id)?;
        let ret = match self
            .simulate_view(
                signer_account_pk,
                its_contract,
                "registered_token_address",
                vec![token_id_v],
            )
            .await
        {
            Ok(ret) => ret,
            Err(e) if is_unregistered_stellar_token_error(&e) => return Ok(None),
            Err(e) => return Err(e),
        };

        let ret = ret.ok_or_else(|| eyre!("registered_token_address returned no result"))?;
        scval_to_address_string(&ret)
            .map(Some)
            .ok_or_else(|| eyre!("registered_token_address returned non-address result"))
    }

    /// Standard SAC `transfer(from, to, amount)`. Used to distribute the AXE
    /// load-test token from the deployer to ephemeral wallets.
    pub async fn token_transfer(
        &self,
        wallet: &StellarWallet,
        token_contract: &str,
        to_account_pk: &[u8; 32],
        amount: u128,
    ) -> Result<InvokedTx> {
        let from = scval_address_account(&wallet.public_key_bytes);
        let to = scval_address_account(to_account_pk);
        let amount_v = scval_i128_from_u128(amount);
        self.invoke_contract(
            wallet,
            token_contract,
            "transfer",
            vec![from, to, amount_v],
            BASE_FEE_TOKEN_OP,
            None,
        )
        .await
    }

    /// Query `balance(account)` on a SAC token contract.
    pub async fn token_balance(
        &self,
        wallet: &StellarWallet,
        token_contract: &str,
        account_pk: &[u8; 32],
    ) -> Result<u128> {
        let account_v = scval_address_account(account_pk);
        let invoked = self
            .invoke_contract(
                wallet,
                token_contract,
                "balance",
                vec![account_v],
                BASE_FEE_VIEW,
                None,
            )
            .await?;
        Ok(invoked
            .return_value
            .as_ref()
            .and_then(scval_to_u128)
            .unwrap_or(0))
    }

    /// Read-only `balance(account)` via simulate — no tx submitted, no fee, no
    /// sequence consumed. Companion to [`Self::token_balance`] (which submits);
    /// use this on read/preflight paths where signing a tx would be wrong.
    pub async fn token_balance_view(
        &self,
        signer_account_pk: &[u8; 32],
        token_contract: &str,
        account_pk: &[u8; 32],
    ) -> Result<u128> {
        let account_v = scval_address_account(account_pk);
        let ret = self
            .simulate_view(
                signer_account_pk,
                token_contract,
                "balance",
                vec![account_v],
            )
            .await?;
        Ok(ret.as_ref().and_then(scval_to_u128).unwrap_or(0))
    }

    /// Simulate `decimals()` on a SAC token contract. Read-only — used to
    /// scale load-test amounts to the actual on-chain token decimals.
    pub async fn token_decimals(
        &self,
        signer_account_pk: &[u8; 32],
        token_contract: &str,
    ) -> Result<u32> {
        let ret = self
            .simulate_view(signer_account_pk, token_contract, "decimals", vec![])
            .await?
            .ok_or_else(|| eyre!("decimals() returned no result"))?;
        scval_to_u32(&ret).ok_or_else(|| eyre!("decimals() returned non-u32 result"))
    }

    // -----------------------------------------------------------------
    // Read-only contract views (simulate-only, no tx submission)
    // -----------------------------------------------------------------

    /// Simulate an `InvokeContract` host function and return the contract's
    /// return ScVal — without submitting a tx. Used for view methods like
    /// `is_message_approved`, `interchain_token_address`, `balance`, etc.
    /// The `signer_account_pk` is just the source account for the simulation
    /// envelope — no authorization needed for read-only calls.
    pub async fn simulate_view(
        &self,
        signer_account_pk: &[u8; 32],
        contract: &str,
        function: &str,
        args: Vec<ScVal>,
    ) -> Result<Option<ScVal>> {
        let contract_id = parse_contract_id(contract)?;
        let fn_symbol = scval_symbol(function)?;
        let args_vec: VecM<ScVal> = args
            .try_into()
            .map_err(|e| eyre!("too many args for InvokeContract: {e}"))?;
        let invoke = InvokeContractArgs {
            contract_address: ScAddress::Contract(stellar_xdr::curr::ContractId(contract_id)),
            function_name: fn_symbol,
            args: args_vec,
        };
        let op = Operation {
            source_account: None,
            body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
                host_function: HostFunction::InvokeContract(invoke),
                auth: VecM::default(),
            }),
        };
        let ops: VecM<Operation, 100> = vec![op].try_into().map_err(|e| eyre!("op vec: {e}"))?;
        let tx = Transaction {
            source_account: MuxedAccount::Ed25519(Uint256(*signer_account_pk)),
            fee: BASE_FEE_VIEW,
            // simulate doesn't need a real seq number; use 1 as a placeholder.
            seq_num: SequenceNumber(1),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: ops,
            ext: TransactionExt::V0,
        };
        let envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: VecM::default(),
        });
        // Read-only simulation: wrap in `retry_all` so a transient Stellar RPC
        // hiccup (connection reset, 5xx) doesn't abort the caller. The load-test
        // verifier polls `is_message_approved`/`is_message_executed` through here;
        // without the retry a single blip aborted the whole run — observed on a
        // mainnet hyperliquid→stellar route that had already executed on-chain
        // (connection reset surfaced here as a hard error). Mirrors the EVM
        // destination view-call retry and the existing XRPL `account_info` retry.
        let sim = crate::retry::retry_all("stellar.simulate_view", || async {
            self.rpc
                .simulate_transaction_envelope(&envelope, None)
                .await
        })
        .await
        .map_err(|e| eyre!("simulate_transaction_envelope: {e}"))?;
        if let Some(err) = sim.error {
            return Err(eyre!("Stellar simulate failed: {err}"));
        }
        let results = sim
            .results()
            .map_err(|e| eyre!("decode simulate results: {e}"))?;
        Ok(results.into_iter().next().map(|r| r.xdr))
    }

    /// `AxelarGateway.is_message_approved(source_chain, message_id,
    /// source_address, contract_address, payload_hash) → bool`. Read-only.
    /// Returns `Some(true)` if approved, `Some(false)` if not (or already
    /// executed), `None` if the simulation could not be parsed.
    #[allow(clippy::too_many_arguments)]
    pub async fn gateway_is_message_approved(
        &self,
        signer_account_pk: &[u8; 32],
        gateway_contract: &str,
        source_chain: &str,
        message_id: &str,
        source_address: &str,
        contract_address: &str,
        payload_hash: [u8; 32],
    ) -> Result<Option<bool>> {
        let args = vec![
            scval_string(source_chain)?,
            scval_string(message_id)?,
            scval_string(source_address)?,
            scval_address_from_str(contract_address)?,
            // BytesN<32>
            ScVal::Bytes(stellar_xdr::curr::ScBytes(
                payload_hash
                    .to_vec()
                    .try_into()
                    .map_err(|e| eyre!("payload hash: {e}"))?,
            )),
        ];
        let ret = self
            .simulate_view(
                signer_account_pk,
                gateway_contract,
                "is_message_approved",
                args,
            )
            .await?;
        Ok(ret.and_then(|v| match v {
            ScVal::Bool(b) => Some(b),
            _ => None,
        }))
    }

    /// `AxelarGateway.is_message_executed(source_chain, message_id) → bool`.
    pub async fn gateway_is_message_executed(
        &self,
        signer_account_pk: &[u8; 32],
        gateway_contract: &str,
        source_chain: &str,
        message_id: &str,
    ) -> Result<Option<bool>> {
        let args = vec![scval_string(source_chain)?, scval_string(message_id)?];
        let ret = self
            .simulate_view(
                signer_account_pk,
                gateway_contract,
                "is_message_executed",
                args,
            )
            .await?;
        Ok(ret.and_then(|v| match v {
            ScVal::Bool(b) => Some(b),
            _ => None,
        }))
    }

    /// Sign a pre-simulated `Transaction` and wrap it in a `TransactionEnvelope`.
    pub fn sign(&self, wallet: &StellarWallet, tx: Transaction) -> Result<TransactionEnvelope> {
        let payload = TransactionSignaturePayload {
            network_id: Hash(self.network_id),
            tagged_transaction: TransactionSignaturePayloadTaggedTransaction::Tx(tx.clone()),
        };
        let payload_bytes = payload
            .to_xdr(Limits::none())
            .map_err(|e| eyre!("encode signature payload: {e}"))?;
        let hash: [u8; 32] = Sha256::digest(&payload_bytes).into();
        let sig = wallet.signing_key.sign(&hash);

        let sig_bytes: BytesM<64> = sig
            .to_bytes()
            .to_vec()
            .try_into()
            .map_err(|e| eyre!("signature encoding: {e}"))?;
        let decorated = DecoratedSignature {
            hint: wallet.signature_hint(),
            signature: Signature(sig_bytes),
        };
        let sigs: VecM<DecoratedSignature, 20> = vec![decorated]
            .try_into()
            .map_err(|e| eyre!("signatures: {e}"))?;

        Ok(TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: sigs,
        }))
    }
}

// `GetTransactionResponse.status` is a plain `String` ("SUCCESS" / "FAILED"
// / "NOT_FOUND") in stellar-rpc-client 23.x. Keep normalized (uppercase,
// trimmed) so minor format changes don't break the caller.
fn extract_status(resp: &stellar_rpc_client::GetTransactionResponse) -> Option<String> {
    Some(resp.status.trim().to_uppercase())
}

fn is_unregistered_stellar_token_error(error: &eyre::Report) -> bool {
    let message = error.to_string();
    message.contains("registered_token_address")
        && (message.contains("InvalidTokenId")
            || message.contains("InvalidAction")
            || message.contains("UnreachableCodeReached"))
}

/// Extract the Soroban contract's return value from a validated tx, if any.
/// The Soroban return value lives in `result_meta.soroban_meta.return_value`
/// (V3) or `result_meta.soroban_meta.return_value` (V4 — same location).
fn extract_return_value(resp: &stellar_rpc_client::GetTransactionResponse) -> Option<ScVal> {
    let meta = resp.result_meta.as_ref()?;
    match meta {
        TransactionMeta::V3(m) => m.soroban_meta.as_ref().map(|s| s.return_value.clone()),
        // V4's SorobanTransactionMetaV2 wraps return_value in Option<ScVal>.
        TransactionMeta::V4(m) => m.soroban_meta.as_ref().and_then(|s| s.return_value.clone()),
        _ => None,
    }
}

/// Find the flat index of the first `ContractEvent` whose `contract_id`
/// matches `target` in a validated tx's events. The Amplifier Stellar
/// VotingVerifier uses this same flat index in its `hex_tx_hash_and_event_index`
/// message-id format.
///
/// On testnet/mainnet today the RPC returns `TransactionMeta::V3`, whose
/// contract events live under `soroban_meta.events`. `stellar-rpc-client`
/// 23.x only flattens those into `GetTransactionEvents::contract_events`
/// for V4 metas, so we walk the raw `result_meta` ourselves.
fn find_event_index(
    resp: &stellar_rpc_client::GetTransactionResponse,
    target: &Hash,
) -> Option<u32> {
    let target_cid = ContractId(target.clone());
    let meta = resp.result_meta.as_ref()?;

    // Collect the flat list of contract events for this tx. Layout differs
    // by meta version: V3 uses `soroban_meta.events`; V4 attaches them to
    // each per-op `events` vec as `TransactionEvent`s wrapping ContractEvent.
    let events: Vec<&stellar_xdr::curr::ContractEvent> = match meta {
        TransactionMeta::V3(m) => m
            .soroban_meta
            .as_ref()
            .map(|s| s.events.iter().collect())
            .unwrap_or_default(),
        TransactionMeta::V4(m) => m
            .operations
            .iter()
            .flat_map(|op| op.events.iter())
            .collect(),
        _ => return None,
    };

    for (i, ev) in events.iter().enumerate() {
        if ev.contract_id.as_ref() == Some(&target_cid) {
            return Some(i as u32);
        }
    }
    None
}
