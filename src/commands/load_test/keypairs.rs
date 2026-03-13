use anchor_lang::prelude::system_instruction;
use alloy::primitives::keccak256;
use eyre::{Result, eyre};
use indicatif::{ProgressBar, ProgressStyle};
use solana_client::rpc_client::RpcClient;
use solana_commitment_config::CommitmentConfig;
use solana_sdk::{
    message::Message,
    signature::{Keypair, SeedDerivable},
    signer::Signer,
    transaction::Transaction,
};

use crate::ui;

/// When a key drops below this balance, it gets topped up.
const MIN_LAMPORTS_PER_KEY: u64 = 10_000_000; // 0.01 SOL

/// Top-up target: fund keys to this amount so they last multiple runs.
const TARGET_LAMPORTS_PER_KEY: u64 = 20_000_000; // 0.02 SOL

/// Lamports reserved in the main wallet for transfer fees.
const FUNDING_RESERVE: u64 = 5_000_000;

/// Solana transfer fee per transaction.
const TRANSFER_FEE: u64 = 5_000;

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
#[allow(clippy::cast_precision_loss, clippy::float_arithmetic)]
pub fn ensure_funded(
    rpc_url: &str,
    main: &Keypair,
    derived: &[Keypair],
) -> Result<Vec<u64>> {
    let rpc = RpcClient::new_with_commitment(rpc_url, CommitmentConfig::confirmed());
    let main_balance = rpc.get_balance(&main.pubkey()).unwrap_or(0);

    // Check which keys need funding
    let mut balances: Vec<u64> = Vec::with_capacity(derived.len());
    let mut to_fund: Vec<(usize, u64)> = Vec::new(); // (index, deficit)

    let check_pb = ProgressBar::new(derived.len() as u64);
    check_pb.set_style(
        ProgressStyle::with_template("  {bar:40.cyan/dim} {pos}/{len} keys checked")
            .unwrap()
            .progress_chars("=> "),
    );
    for (i, kp) in derived.iter().enumerate() {
        let balance = rpc.get_balance(&kp.pubkey()).unwrap_or(0);
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
    let total_needed: u64 = to_fund.iter().map(|(_, deficit)| deficit + TRANSFER_FEE).sum();
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
        ProgressStyle::with_template("  {bar:40.cyan/dim} {pos}/{len} keys funded")
            .unwrap()
            .progress_chars("=> "),
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
