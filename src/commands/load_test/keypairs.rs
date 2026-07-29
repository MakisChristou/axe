use alloy::network::TransactionBuilder;
use alloy::primitives::U256;
use alloy::primitives::keccak256;
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use anchor_lang::prelude::system_instruction;
use eyre::{Result, eyre};
use indicatif::ProgressBar;
use solana_client::rpc_client::RpcClient;
use solana_commitment_config::CommitmentConfig;
use solana_sdk::{
    message::Message,
    signature::{Keypair, SeedDerivable},
    signer::Signer,
    transaction::Transaction,
};

use super::units::{Lamports, Wei};
use crate::ui;

/// When a key drops below this balance, it gets topped up.
const MIN_LAMPORTS_PER_KEY: u64 = 10_000_000; // 0.01 SOL

/// Top-up target: fund keys to this amount so they last multiple runs.
const TARGET_LAMPORTS_PER_KEY: u64 = 20_000_000; // 0.02 SOL

/// Lamports reserved in the main wallet for transfer fees.
const FUNDING_RESERVE: u64 = 5_000_000;

/// Solana transfer fee per transaction.
const TRANSFER_FEE: u64 = 5_000;

pub fn clone_keypair(keypair: &Keypair) -> Result<Keypair> {
    let bytes = keypair.to_bytes();
    let seed: [u8; 32] = bytes[..32]
        .try_into()
        .map_err(|_| eyre!("Solana keypair did not contain a 32-byte seed"))?;
    Ok(Keypair::new_from_array(seed))
}

/// Deterministically derive `count` keypairs from a main keypair.
///
/// Uses `keccak256(main_secret_key || index)` as seed for each derived key,
/// so the same main keypair always produces the same derived set.
pub fn derive_keypairs(main: &Keypair, count: usize) -> Result<Vec<Keypair>> {
    let main_seed = &main.to_bytes()[..32];
    (0..count)
        .map(|i| {
            let mut seed_input = Vec::with_capacity(40);
            seed_input.extend_from_slice(main_seed);
            seed_input.extend_from_slice(&(i as u64).to_le_bytes());
            let hash = keccak256(&seed_input);
            Keypair::from_seed(hash.as_slice())
                .map_err(|e| eyre!("failed to derive keypair {i}: {e}"))
        })
        .collect()
}

/// Check that all derived keypairs are funded, and fund any that aren't.
///
/// Shows a progress bar during funding. Returns the per-key balance after funding.
pub fn ensure_funded(rpc_url: &str, main: &Keypair, derived: &[Keypair]) -> Result<Vec<u64>> {
    let rpc = RpcClient::new_with_commitment(rpc_url, CommitmentConfig::finalized());
    let main_balance = rpc
        .get_balance(&main.pubkey())
        .map_err(|e| eyre!("failed to read main Solana wallet balance: {e}"))?;

    // Check which keys need funding
    let mut balances: Vec<u64> = Vec::with_capacity(derived.len());
    let mut to_fund: Vec<(usize, u64)> = Vec::new(); // (index, deficit)

    let check_pb = ProgressBar::new(derived.len() as u64);
    check_pb.set_style(
        ui::progress_bar_style("  {bar:40.cyan/dim} {pos}/{len} keys checked")
            .progress_chars("=> "),
    );
    for (i, kp) in derived.iter().enumerate() {
        let balance = rpc
            .get_balance(&kp.pubkey())
            .map_err(|e| eyre!("failed to read derived Solana key {i} balance: {e}"))?;
        balances.push(balance);
        if balance < MIN_LAMPORTS_PER_KEY {
            to_fund.push((i, TARGET_LAMPORTS_PER_KEY - balance));
        }
        check_pb.inc(1);
    }
    check_pb.finish_and_clear();

    if to_fund.is_empty() {
        ui::success(&format!(
            "all {} derived keys are funded (>= {:.4} SOL each)",
            derived.len(),
            MIN_LAMPORTS_PER_KEY as f64 / 1e9,
        ));
        return Ok(balances);
    }

    // Check main wallet has enough
    let total_needed: u64 = to_fund
        .iter()
        .map(|(_, deficit)| deficit + TRANSFER_FEE)
        .sum();
    if main_balance < total_needed + FUNDING_RESERVE {
        let needed_sol = (total_needed + FUNDING_RESERVE) as f64 / 1e9;
        let have_sol = main_balance as f64 / 1e9;
        return Err(eyre!(
            "main wallet has {have_sol:.4} SOL but needs {needed_sol:.4} SOL to fund {} keys.\n  \
             Fund the main wallet first:\n  solana airdrop 2 {}",
            to_fund.len(),
            main.pubkey(),
        ));
    }

    ui::info(&format!(
        "funding {}/{} keys from main wallet ({:.4} SOL)...",
        to_fund.len(),
        derived.len(),
        total_needed as f64 / 1e9,
    ));

    let pb = ProgressBar::new(to_fund.len() as u64);
    pb.set_style(
        ui::progress_bar_style("  {bar:40.cyan/dim} {pos}/{len} keys funded").progress_chars("=> "),
    );

    for (i, amount) in &to_fund {
        let to_pubkey = derived[*i].pubkey();
        transfer_sol(&rpc, main, &to_pubkey, *amount)?;
        balances[*i] += amount;
        pb.inc(1);
    }
    pb.finish_and_clear();

    ui::success(&format!(
        "funded {} keys ({:.4} SOL total)",
        to_fund.len(),
        total_needed as f64 / 1e9,
    ));

    Ok(balances)
}

/// Like `ensure_funded`, but targets a balance that covers `fires_per_key`
/// transactions, each costing `gas_lamports` on top of the base tx fee.
pub fn ensure_funded_for_sustained(
    rpc_url: &str,
    main: &Keypair,
    derived: &[Keypair],
    fires_per_key: u64,
    gas: Lamports,
) -> Result<Vec<u64>> {
    let cost_per_tx = gas.get() + TRANSFER_FEE; // gas + tx fee
    let target = cost_per_tx
        .saturating_mul(fires_per_key)
        .max(TARGET_LAMPORTS_PER_KEY);
    // Top-up trigger must match the actual requirement: half-target lets a
    // key squeak by with enough for only ~half its planned fires, then every
    // subsequent fire fails at `PayGas` with system-program error 0x1
    // (`ResultWithNegativeLamports`).
    let min_needed = target;

    let rpc = RpcClient::new_with_commitment(rpc_url, CommitmentConfig::finalized());
    let main_balance = rpc
        .get_balance(&main.pubkey())
        .map_err(|e| eyre!("failed to read main Solana wallet balance: {e}"))?;

    let mut balances: Vec<u64> = Vec::with_capacity(derived.len());
    let mut to_fund: Vec<(usize, u64)> = Vec::new();

    let check_pb = ProgressBar::new(derived.len() as u64);
    check_pb.set_style(
        ui::progress_bar_style("  {bar:40.cyan/dim} {pos}/{len} keys checked")
            .progress_chars("=> "),
    );
    for (i, kp) in derived.iter().enumerate() {
        let balance = rpc
            .get_balance(&kp.pubkey())
            .map_err(|e| eyre!("failed to read derived Solana key {i} balance: {e}"))?;
        balances.push(balance);
        if balance < min_needed {
            to_fund.push((i, target.saturating_sub(balance)));
        }
        check_pb.inc(1);
    }
    check_pb.finish_and_clear();

    if to_fund.is_empty() {
        ui::success(&format!(
            "all {} derived keys are funded (>= {:.4} SOL each)",
            derived.len(),
            min_needed as f64 / 1e9,
        ));
        return Ok(balances);
    }

    let total_needed: u64 = to_fund
        .iter()
        .map(|(_, deficit)| deficit + TRANSFER_FEE)
        .sum();
    if main_balance < total_needed + FUNDING_RESERVE {
        let needed_sol = (total_needed + FUNDING_RESERVE) as f64 / 1e9;
        let have_sol = main_balance as f64 / 1e9;
        return Err(eyre!(
            "main wallet has {have_sol:.4} SOL but needs {needed_sol:.4} SOL to fund {} keys.\n  \
             Fund the main wallet first:\n  solana airdrop 2 {}",
            to_fund.len(),
            main.pubkey(),
        ));
    }

    ui::info(&format!(
        "funding {}/{} keys from main wallet ({:.4} SOL)...",
        to_fund.len(),
        derived.len(),
        total_needed as f64 / 1e9,
    ));

    let pb = ProgressBar::new(to_fund.len() as u64);
    pb.set_style(
        ui::progress_bar_style("  {bar:40.cyan/dim} {pos}/{len} keys funded").progress_chars("=> "),
    );

    for (i, amount) in &to_fund {
        let to_pubkey = derived[*i].pubkey();
        transfer_sol(&rpc, main, &to_pubkey, *amount)?;
        balances[*i] += amount;
        pb.inc(1);
    }
    pb.finish_and_clear();

    ui::success(&format!(
        "funded {} keys ({:.4} SOL total)",
        to_fund.len(),
        total_needed as f64 / 1e9,
    ));

    Ok(balances)
}

/// Deterministically derive `count` EVM signers from a main private key.
///
/// Uses the same `keccak256(main_key || index)` pattern as `derive_keypairs()`.
/// Each 32-byte hash is a valid secp256k1 private key.
pub fn derive_evm_signers(main_key: &[u8; 32], count: usize) -> Result<Vec<PrivateKeySigner>> {
    // A single sender needs no parallelism — use the main wallet itself, so a
    // one-off run never funds (and never leaves gas + the msg.sender gas refund
    // parked in) a throwaway subwallet. Parallel runs (count >= 2) still get
    // distinct derived wallets to avoid nonce contention.
    if count <= 1 {
        let main = PrivateKeySigner::from_bytes(&(*main_key).into())
            .map_err(|e| eyre!("invalid main EVM key: {e}"))?;
        return Ok(vec![main; count]);
    }
    (0..count)
        .map(|i| {
            let mut seed_input = Vec::with_capacity(40);
            seed_input.extend_from_slice(main_key);
            seed_input.extend_from_slice(&(i as u64).to_le_bytes());
            let hash = keccak256(&seed_input);
            PrivateKeySigner::from_bytes(&hash)
                .map_err(|e| eyre!("failed to derive EVM signer {i}: {e}"))
        })
        .collect()
}

/// Per-key native-gas buffer on top of any `extra_wei` the caller adds for
/// cross-chain `msg.value`. Sized to cover ~300k gas at Monad mainnet's
/// ~200 gwei (= 0.06 MON) with headroom — at 0.05 ETH the tx still has
/// budget for one retry or a gas-price spike. The earlier 0.01 ETH value
/// silently underfunded Monad sources: pre-tx balance 13.972786 MON,
/// tx value 13.962786 + max-gas 0.052514 = 14.015300 needed, so the EVM
/// ran the interchainTransfer until gas_used hit gas_limit and reverted
/// with empty data (looks identical to OOG). Cheap-gas chains (HL ~2 gwei,
/// XRPL-EVM ~1 gwei) pay the same flat 0.05 once but have plenty of room.
///
/// MIN is the trigger threshold (top up when below); TARGET is what we
/// top up to. 4:5 ratio gives a small dead band so successive runs don't
/// thrash with refunds.
const MIN_WEI_PER_KEY: u128 = 40_000_000_000_000_000; // 0.04 ETH
const TARGET_WEI_PER_KEY: u128 = 50_000_000_000_000_000; // 0.05 ETH

/// Check that all derived EVM signers are funded, and fund any that aren't.
/// Each key needs gas + `extra_wei` (e.g. for cross-chain gas value).
/// (e.g. for `msg.value` in cross-chain gas payment).
pub async fn ensure_funded_evm_with_extra<P: Provider>(
    provider: &P,
    main_signer: &PrivateKeySigner,
    derived: &[PrivateKeySigner],
    extra: Wei,
) -> Result<()> {
    let extra_wei: u128 = extra
        .as_u256()
        .try_into()
        .map_err(|_| eyre!("per-key EVM funding amount exceeds u128"))?;
    let min_needed = MIN_WEI_PER_KEY + extra_wei;
    let target = TARGET_WEI_PER_KEY + extra_wei;

    let mut to_fund: Vec<(usize, u128)> = Vec::new();

    let check_pb = ProgressBar::new(derived.len() as u64);
    check_pb.set_style(
        ui::progress_bar_style("  {bar:40.cyan/dim} {pos}/{len} keys checked")
            .progress_chars("=> "),
    );
    for (i, signer) in derived.iter().enumerate() {
        let address = signer.address();
        let balance = provider
            .get_balance(address)
            .await
            .map_err(|e| eyre!("failed to read derived EVM key {i} balance ({address}): {e}"))?;
        let bal: u128 = balance.to();
        if bal < min_needed {
            let deficit = target.saturating_sub(bal);
            to_fund.push((i, deficit));
        }
        check_pb.inc(1);
    }
    check_pb.finish_and_clear();

    if to_fund.is_empty() {
        ui::success(&format!(
            "all {} derived EVM keys are funded (>= {:.4} ETH each)",
            derived.len(),
            min_needed as f64 / 1e18,
        ));
        return Ok(());
    }

    let main_balance: u128 = provider.get_balance(main_signer.address()).await?.to();
    let total_needed: u128 = to_fund.iter().map(|(_, deficit)| deficit).sum();
    if main_balance < total_needed {
        let needed_eth = total_needed as f64 / 1e18;
        let have_eth = main_balance as f64 / 1e18;
        return Err(eyre!(
            "main wallet has {have_eth:.6} ETH but needs {needed_eth:.6} ETH to fund {} keys.\n  \
             Fund the main wallet first.",
            to_fund.len(),
        ));
    }

    ui::info(&format!(
        "funding {}/{} keys from main wallet ({:.6} ETH)...",
        to_fund.len(),
        derived.len(),
        total_needed as f64 / 1e18,
    ));

    let pb = ProgressBar::new(to_fund.len() as u64);
    pb.set_style(
        ui::progress_bar_style("  {bar:40.cyan/dim} {pos}/{len} keys funded").progress_chars("=> "),
    );

    // Legacy (pre-1559) chains break alloy's default fee estimation; send the
    // funding txs as type-0 with an explicit gas_price when needed. No-op on
    // 1559 chains.
    let fee_mode = super::gas_mode::EvmFeeMode::detect(provider).await?;

    for (i, amount) in &to_fund {
        let tx = fee_mode.apply(
            TransactionRequest::default()
                .with_to(derived[*i].address())
                .with_value(U256::from(*amount)),
        );
        let pending = provider
            .send_transaction(tx)
            .await
            .map_err(|e| eyre!("failed to fund key {i}: {e}"))?;
        pending
            .get_receipt()
            .await
            .map_err(|e| eyre!("funding tx for key {i} failed: {e}"))?;
        pb.inc(1);
    }
    pb.finish_and_clear();

    ui::success(&format!(
        "funded {} EVM keys ({:.6} ETH total)",
        to_fund.len(),
        total_needed as f64 / 1e18,
    ));

    Ok(())
}

/// Transfer SOL from one keypair to a destination pubkey.
fn transfer_sol(
    rpc: &RpcClient,
    from: &Keypair,
    to: &solana_sdk::pubkey::Pubkey,
    lamports: u64,
) -> Result<()> {
    let ix = system_instruction::transfer(&from.pubkey(), to, lamports);
    let blockhash = rpc.get_latest_blockhash()?;
    let msg = Message::new_with_blockhash(&[ix], Some(&from.pubkey()), &blockhash);
    let mut tx = Transaction::new_unsigned(msg);
    tx.sign(&[from], blockhash);
    rpc.send_and_confirm_transaction(&tx)
        .map_err(|e| eyre!("failed to fund {}: {e}", to))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// XRPL key derivation + funding
// ---------------------------------------------------------------------------

use crate::xrpl::{XrplClient, XrplWallet};

/// Deterministically derive `count` XRPL wallets from a main 32-byte seed.
///
/// Uses the same `keccak256(main_seed || index)` pattern as the Solana/EVM
/// derivation, so re-runs always get the same ephemeral wallets and we can
/// reuse their on-chain reserves.
pub fn derive_xrpl_wallets(main_seed: &[u8; 32], count: usize) -> Result<Vec<XrplWallet>> {
    // Single sender → use the main wallet (no throwaway wallet to fund, and no
    // ~10 XRP base reserve left parked in an ephemeral account).
    if count <= 1 {
        let main =
            XrplWallet::from_bytes(main_seed).map_err(|e| eyre!("invalid main XRPL seed: {e}"))?;
        return Ok(vec![main; count]);
    }
    (0..count)
        .map(|i| {
            let mut seed_input = Vec::with_capacity(40);
            seed_input.extend_from_slice(main_seed);
            seed_input.extend_from_slice(&(i as u64).to_le_bytes());
            let hash = keccak256(&seed_input);
            XrplWallet::from_bytes(hash.as_ref())
                .map_err(|e| eyre!("failed to derive XRPL wallet {i}: {e}"))
        })
        .collect()
}

/// Ensure every derived XRPL wallet is activated and holds at least
/// `target_drops` of XRP. Tops up any short wallet, either via the public
/// faucet (devnet/testnet) or via direct `Payment` from `main_wallet`.
///
/// The XRPL base reserve (~10 XRP) is implicitly part of `target_drops` —
/// callers should include it when deciding how much to request.
pub async fn ensure_funded_xrpl(
    client: &XrplClient,
    main_wallet: Option<&XrplWallet>,
    derived: &[XrplWallet],
    target_drops: u64,
    faucet_url: Option<&str>,
) -> Result<()> {
    // Check balances in parallel — XRPL account_info is cheap.
    let mut balances = Vec::with_capacity(derived.len());
    let check_pb = ProgressBar::new(derived.len() as u64);
    check_pb.set_style(
        ui::progress_bar_style("  {bar:40.cyan/dim} {pos}/{len} XRPL keys checked")
            .progress_chars("=> "),
    );
    for w in derived {
        let info = client.account_info(&w.address()).await?;
        balances.push(info.map(|i| i.balance_drops).unwrap_or(0));
        check_pb.inc(1);
    }
    check_pb.finish_and_clear();

    let to_fund: Vec<(usize, u64)> = balances
        .iter()
        .enumerate()
        .filter_map(|(i, &bal)| {
            if bal < target_drops {
                Some((i, target_drops.saturating_sub(bal)))
            } else {
                None
            }
        })
        .collect();

    if to_fund.is_empty() {
        ui::success(&format!(
            "all {} derived XRPL keys are funded (>= {:.2} XRP each)",
            derived.len(),
            target_drops as f64 / 1_000_000.0,
        ));
        return Ok(());
    }

    let total_needed: u64 = to_fund.iter().map(|(_, d)| *d).sum();
    ui::info(&format!(
        "funding {}/{} XRPL keys ({:.2} XRP total)...",
        to_fund.len(),
        derived.len(),
        total_needed as f64 / 1_000_000.0,
    ));

    let pb = ProgressBar::new(to_fund.len() as u64);
    pb.set_style(
        ui::progress_bar_style("  {bar:40.cyan/dim} {pos}/{len} XRPL keys funded")
            .progress_chars("=> "),
    );

    // Path 1: public faucet (devnet/testnet). The faucet drops a fixed amount
    // per call and ignores our target, so call it once per short wallet.
    // Path 2: direct Payment from main wallet (stagenet/mainnet).
    match (faucet_url, main_wallet) {
        (Some(faucet), _) => {
            for (i, _) in &to_fund {
                client
                    .fund_from_faucet(&derived[*i].address(), faucet)
                    .await
                    .map_err(|e| eyre!("faucet funding failed for key {i}: {e}"))?;
                pb.inc(1);
            }
        }
        (None, Some(main)) => {
            // Ensure main has enough (+ small fee buffer per tx).
            let main_info = client.account_info(&main.address()).await?;
            let main_balance = main_info
                .map(|i| i.balance_drops)
                .ok_or_else(|| eyre!("XRPL main wallet {} is not activated", main.address()))?;
            let fee_buffer = 1_000u64.saturating_mul(to_fund.len() as u64); // ~100 drops/tx overhead
            if main_balance < total_needed + fee_buffer {
                return Err(eyre!(
                    "XRPL main wallet has {:.4} XRP but needs {:.4} XRP to fund {} keys. \
                     Fund the main wallet ({}) first.",
                    main_balance as f64 / 1_000_000.0,
                    (total_needed + fee_buffer) as f64 / 1_000_000.0,
                    to_fund.len(),
                    main.address(),
                ));
            }
            for (i, amount) in &to_fund {
                client
                    .submit_plain_payment(main, &derived[*i].account_id, *amount)
                    .await
                    .map_err(|e| eyre!("funding payment failed for key {i}: {e}"))?;
                pb.inc(1);
            }
        }
        (None, None) => {
            return Err(eyre!(
                "no XRPL faucet available and no main wallet provided to fund {} ephemeral XRPL keys",
                to_fund.len()
            ));
        }
    }
    pb.finish_and_clear();

    ui::success(&format!(
        "funded {} XRPL keys ({:.2} XRP total)",
        to_fund.len(),
        total_needed as f64 / 1_000_000.0,
    ));
    Ok(())
}
