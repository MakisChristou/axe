use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use eyre::eyre;
use futures::future::join_all;
use solana_sdk::signature::Keypair;
use solana_sdk::signer::Signer;
use tokio::sync::Mutex;

use super::keypairs;
use super::metrics::{LoadTestReport, TxMetrics};
use super::{
    LoadTestArgs, finish_report, read_its_cache, save_its_cache, validate_evm_rpc,
    validate_solana_rpc,
};
use alloy::primitives::Address;
use crate::cosmos::read_axelar_contract_field;
use crate::solana;
use crate::ui;
use crate::utils::read_contract_address;
use std::path::Path;

const TOKEN_NAME: &str = "AXE Load Test";
const TOKEN_SYMBOL: &str = "AXELT";
const TOKEN_DECIMALS: u8 = 18;
const AMOUNT_PER_TX: u64 = 1_000_000_000_000_000_000; // 1 token (10^18)

/// Default gas value for ITS transfer on Solana (in lamports).
/// devnet-amplifier doesn't require gas, stagenet/mainnet do.
fn default_gas_value() -> u64 {
    #[cfg(feature = "devnet-amplifier")]
    { 0 }
    #[cfg(not(feature = "devnet-amplifier"))]
    { 100_000 }
}

pub async fn run(mut args: LoadTestArgs, run_start: Instant) -> eyre::Result<()> {
    // --source-rpc overrides the Solana (source) RPC
    if let Some(rpc) = args.source_rpc.take() {
        args.solana_rpc = rpc;
    }

    let src = &args.source_chain;
    let dest = &args.destination_chain;

    // --- Read config ---
    let config_content = std::fs::read_to_string(&args.config)
        .map_err(|e| eyre!("failed to read config {}: {e}", args.config.display()))?;
    let config_root: serde_json::Value = serde_json::from_str(&config_content)?;

    let evm_rpc_url = config_root
        .pointer(&format!("/chains/{dest}/rpc"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| eyre!("no rpc URL for destination chain '{dest}' in config"))?
        .to_string();

    // Validate RPCs
    validate_solana_rpc(&args.solana_rpc).await?;
    validate_evm_rpc(&evm_rpc_url).await?;

    // Check verification contracts exist
    if read_axelar_contract_field(
        &args.config,
        &format!("/axelar/contracts/Gateway/{dest}/address"),
    )
    .is_err()
    {
        eyre::bail!(
            "destination chain '{dest}' has no Cosmos Gateway in the config — verification would fail."
        );
    }
    if read_axelar_contract_field(&args.config, "/axelar/contracts/AxelarnetGateway/address")
        .is_err()
    {
        eyre::bail!("no AxelarnetGateway address in config — required for ITS load test");
    }

    ui::kv("source", src);
    ui::kv("destination", dest);
    ui::kv("protocol", "ITS (interchainTransfer via hub)");

    // --- Solana keypair ---
    let main_keypair = solana::load_keypair(args.keypair.as_deref())?;
    let rpc_client = solana_client::rpc_client::RpcClient::new_with_commitment(
        &args.solana_rpc,
        solana_commitment_config::CommitmentConfig::confirmed(),
    );
    let pubkey = main_keypair.pubkey();
    let balance = rpc_client.get_balance(&pubkey).unwrap_or(0);
    #[allow(clippy::float_arithmetic)]
    let sol = balance as f64 / 1e9;
    ui::kv("wallet", &format!("{pubkey} ({sol:.4} SOL)"));
    if balance == 0 {
        return Err(eyre!(
            "wallet ({pubkey}) has no SOL. Fund it first:\n  solana airdrop 2 {pubkey}"
        ));
    }

    // --- Gas value ---
    let gas_value: u64 = match &args.gas_value {
        Some(v) => v.parse().map_err(|e| eyre!("invalid --gas-value: {e}"))?,
        None => default_gas_value(),
    };
    ui::kv("gas value", &format!("{gas_value} lamports"));

    // --- EVM destination address (ITS proxy on destination chain) ---
    let its_proxy_addr = read_contract_address(&args.config, dest, "InterchainTokenService")?;
    ui::address("destination ITS", &format!("{its_proxy_addr}"));
    let dest_address_bytes = its_proxy_addr.as_slice().to_vec();

    // --- EVM gateway for verification ---
    let evm_gateway_addr = read_contract_address(&args.config, dest, "AxelarGateway")?;
    ui::address("EVM gateway", &format!("{evm_gateway_addr}"));

    let num_txs = args.num_txs.max(1) as usize;

    // --- Token setup ---
    let (token_id, _salt, mint) = setup_its_token(
        &args.solana_rpc,
        &main_keypair,
        src,
        dest,
        num_txs,
        gas_value,
        args.token_id.as_deref(),
        &args.config,
        evm_gateway_addr,
        &evm_rpc_url,
    )
    .await?;

    ui::kv("token ID", &hex::encode(token_id));
    ui::address("mint", &mint.to_string());

    // --- Derive and fund keypairs ---
    let keypairs = prepare_keypairs(&args.solana_rpc, num_txs, &main_keypair)?;
    let key_count = keypairs.len();

    // --- Create ATAs and distribute tokens ---
    distribute_its_tokens(
        &args.solana_rpc,
        &main_keypair,
        &keypairs,
        &mint,
        &token_id,
        AMOUNT_PER_TX,
    )?;

    // --- ITS hub routing info ---
    // ITS always routes through "axelar" hub. The GMP destination is the AxelarnetGateway.
    let axelarnet_gw_addr = read_axelar_contract_field(
        &args.config,
        "/axelar/contracts/AxelarnetGateway/address",
    )?;

    // --- Parallel sends ---
    let metrics_list: Arc<Mutex<Vec<TxMetrics>>> = Arc::new(Mutex::new(Vec::new()));
    let confirmed_counter = Arc::new(AtomicU64::new(0));
    let spinner = ui::wait_spinner(&format!("sending (0/{key_count} confirmed)..."));
    let test_start = Instant::now();

    let mut tasks = Vec::with_capacity(key_count);

    for kp in &keypairs {
        let metrics_clone = Arc::clone(&metrics_list);
        let counter = Arc::clone(&confirmed_counter);
        let sp = spinner.clone();
        let total = key_count;
        let rpc = args.solana_rpc.clone();
        let dc = dest.to_string();
        let da = dest_address_bytes.clone();
        let tid = token_id;
        let m = mint;
        let gv = gas_value;
        let kp = kp.clone();
        let gmp_dest_addr = axelarnet_gw_addr.clone();

        let handle = tokio::spawn(async move {
            let submit_start = Instant::now();
            let source_addr = kp.pubkey().to_string();

            // Compute source ATA
            let token_program = solana_sdk::pubkey::Pubkey::from_str_const("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
            let source_ata = solana_sdk::pubkey::Pubkey::find_program_address(
                &[
                    kp.pubkey().as_ref(),
                    token_program.as_ref(),
                    m.as_ref(),
                ],
                &solana_sdk::pubkey::Pubkey::from_str_const(
                    "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL",
                ),
            )
            .0;

            match solana::send_its_interchain_transfer(
                &rpc, &kp, &tid, &source_ata, &m, &dc, &da, AMOUNT_PER_TX, gv,
            ) {
                Ok((_sig, mut metrics)) => {
                    // Format message_id: the ITS program CPI's gateway.call_contract
                    // at inner instruction index 1.4 (discovered empirically).
                    metrics.signature = format!("{}-1.4", metrics.signature);
                    metrics.source_address = source_addr;
                    metrics.send_instant = Some(submit_start);
                    // ITS always routes through the hub
                    metrics.gmp_destination_chain = "axelar".to_string();
                    metrics.gmp_destination_address = gmp_dest_addr.clone();
                    let done = counter.fetch_add(1, Ordering::Relaxed) + 1;
                    sp.set_message(format!("sending ({done}/{total} confirmed)..."));
                    metrics_clone.lock().await.push(metrics);
                }
                Err(e) => {
                    #[allow(clippy::cast_possible_truncation)]
                    let elapsed_ms = submit_start.elapsed().as_millis() as u64;
                    let metrics = TxMetrics {
                        signature: String::new(),
                        submit_time_ms: elapsed_ms,
                        confirm_time_ms: None,
                        latency_ms: None,
                        compute_units: None,
                        slot: None,
                        success: false,
                        error: Some(e.to_string()),
                        payload: Vec::new(),
                        payload_hash: String::new(),
                        source_address: String::new(),
                        gmp_destination_chain: String::new(),
                        gmp_destination_address: String::new(),
                        send_instant: None,
                        amplifier_timing: None,
                    };
                    metrics_clone.lock().await.push(metrics);
                }
            }
        });
        tasks.push(handle);
    }

    let total_submitted = tasks.len() as u64;
    join_all(tasks).await;
    let test_duration = test_start.elapsed().as_secs_f64();

    let confirmed_count = confirmed_counter.load(Ordering::Relaxed);
    spinner.finish_and_clear();
    ui::success(&format!(
        "sent {confirmed_count}/{total_submitted} confirmed"
    ));

    let metrics = metrics_list.lock().await.clone();
    let total_confirmed = metrics.iter().filter(|m| m.success).count() as u64;
    let total_failed = metrics.iter().filter(|m| !m.success).count() as u64;

    if total_failed > 0 {
        let mut error_counts: std::collections::HashMap<String, u64> =
            std::collections::HashMap::new();
        for m in metrics.iter().filter(|m| !m.success) {
            let reason = m
                .error
                .as_deref()
                .unwrap_or("unknown")
                .chars()
                .take(120)
                .collect::<String>();
            *error_counts.entry(reason).or_default() += 1;
        }
        for (reason, count) in &error_counts {
            ui::warn(&format!("{count} txs failed: {reason}"));
        }
    }

    let latencies: Vec<u64> = metrics.iter().filter_map(|m| m.latency_ms).collect();
    let compute_units: Vec<u64> = metrics.iter().filter_map(|m| m.compute_units).collect();

    #[allow(clippy::cast_precision_loss, clippy::float_arithmetic)]
    let mut report = LoadTestReport {
        source_chain: src.to_string(),
        destination_chain: dest.to_string(),
        destination_address: format!("{its_proxy_addr}"),
        num_txs: args.num_txs,
        num_keys: key_count,
        total_submitted,
        total_confirmed,
        total_failed,
        test_duration_secs: test_duration,
        tps_submitted: if test_duration > 0.0 {
            total_submitted as f64 / test_duration
        } else {
            0.0
        },
        tps_confirmed: if test_duration > 0.0 {
            total_confirmed as f64 / test_duration
        } else {
            0.0
        },
        landing_rate: if total_submitted > 0 {
            total_confirmed as f64 / total_submitted as f64
        } else {
            0.0
        },
        avg_latency_ms: if latencies.is_empty() {
            None
        } else {
            Some(latencies.iter().sum::<u64>() as f64 / latencies.len() as f64)
        },
        min_latency_ms: latencies.iter().min().copied(),
        max_latency_ms: latencies.iter().max().copied(),
        avg_compute_units: if compute_units.is_empty() {
            None
        } else {
            Some(compute_units.iter().sum::<u64>() as f64 / compute_units.len() as f64)
        },
        min_compute_units: compute_units.iter().min().copied(),
        max_compute_units: compute_units.iter().max().copied(),
        verification: None,
        transactions: metrics,
    };

    // --- Verify ---
    let verification = super::verify::verify_onchain_evm_its(
        &args.config,
        &args.source_chain,
        &args.destination_chain,
        &format!("{its_proxy_addr}"),
        evm_gateway_addr,
        &evm_rpc_url,
        &mut report.transactions,
    )
    .await?;
    report.verification = Some(verification);

    finish_report(&args, &report, run_start)
}

// ---------------------------------------------------------------------------
// Token setup
// ---------------------------------------------------------------------------

/// Deploy or reuse ITS token. Returns (token_id, salt, mint).
/// When deploying fresh, waits for the remote deploy to propagate through the
/// ITS hub and execute on the EVM destination before returning.
#[allow(clippy::too_many_arguments)]
async fn setup_its_token(
    solana_rpc: &str,
    keypair: &Keypair,
    src: &str,
    dest: &str,
    num_txs: usize,
    gas_value: u64,
    token_id_override: Option<&str>,
    config: &Path,
    evm_gateway_addr: Address,
    evm_rpc_url: &str,
) -> eyre::Result<([u8; 32], [u8; 32], solana_sdk::pubkey::Pubkey)> {
    if let Some(tid_hex) = token_id_override {
        let tid_bytes = hex::decode(tid_hex.strip_prefix("0x").unwrap_or(tid_hex))
            .map_err(|e| eyre!("invalid --token-id: {e}"))?;
        if tid_bytes.len() != 32 {
            return Err(eyre!("--token-id must be 32 bytes"));
        }
        let mut token_id = [0u8; 32];
        token_id.copy_from_slice(&tid_bytes);
        let (its_root, _) = solana::find_its_root_pda();
        let (mint, _) = solana::find_interchain_token_pda(&its_root, &token_id);
        ui::kv("token ID (provided)", tid_hex);
        return Ok((token_id, [0u8; 32], mint));
    }

    // Check cache
    let cache = read_its_cache(src, dest);
    if let Some(tid_hex) = cache.get("tokenId").and_then(|v| v.as_str()) {
        let tid_bytes = hex::decode(tid_hex.strip_prefix("0x").unwrap_or(tid_hex)).ok();
        let salt_hex = cache.get("salt").and_then(|v| v.as_str());
        if let (Some(tid_bytes), Some(salt_hex)) = (tid_bytes, salt_hex)
            && tid_bytes.len() == 32
        {
            let mut token_id = [0u8; 32];
            token_id.copy_from_slice(&tid_bytes);
            let mut salt = [0u8; 32];
            let salt_bytes =
                hex::decode(salt_hex.strip_prefix("0x").unwrap_or(salt_hex)).unwrap_or_default();
            if salt_bytes.len() == 32 {
                salt.copy_from_slice(&salt_bytes);
            }
            let (its_root, _) = solana::find_its_root_pda();
            let (mint, _) = solana::find_interchain_token_pda(&its_root, &token_id);

            // Verify token still exists on-chain
            let rpc_client = solana_client::rpc_client::RpcClient::new_with_commitment(
                solana_rpc,
                solana_commitment_config::CommitmentConfig::confirmed(),
            );
            if rpc_client.get_account_data(&mint).is_ok() {
                ui::info(&format!("reusing cached ITS token: {mint}"));
                return Ok((token_id, salt, mint));
            }
            ui::warn("cached token no longer exists, deploying fresh...");
        }
    }

    // Deploy fresh
    let salt = generate_salt();
    let total_supply = AMOUNT_PER_TX * (num_txs as u64 + 10);

    ui::info("deploying new ITS token on Solana...");
    ui::kv("name", TOKEN_NAME);
    ui::kv("symbol", TOKEN_SYMBOL);
    ui::kv("decimals", &TOKEN_DECIMALS.to_string());
    ui::kv("supply", &total_supply.to_string());

    let deploy_sig = solana::send_its_deploy_interchain_token(
        solana_rpc,
        keypair,
        &salt,
        TOKEN_NAME,
        TOKEN_SYMBOL,
        TOKEN_DECIMALS,
        total_supply,
        Some(&keypair.pubkey()), // deployer as minter for ongoing supply
    )?;
    ui::tx_hash("deploy tx", &deploy_sig);

    let token_id = solana::interchain_token_id(&keypair.pubkey(), &salt);
    let (its_root, _) = solana::find_its_root_pda();
    let (mint, _) = solana::find_interchain_token_pda(&its_root, &token_id);

    ui::kv("token ID", &hex::encode(token_id));
    ui::address("mint", &mint.to_string());

    // Deploy remote to EVM destination
    ui::info(&format!("deploying remote token to {dest}..."));
    let remote_sig = solana::send_its_deploy_remote_interchain_token(
        solana_rpc, keypair, &salt, dest, gas_value,
    )?;
    ui::tx_hash("remote deploy tx", &remote_sig);
    ui::success("remote deploy tx confirmed on Solana");

    // Wait for the remote deploy to propagate through the hub and execute on EVM.
    // The deploy message ID is {signature}-1.3 (empirically determined).
    let deploy_message_id = format!("{remote_sig}-1.3");
    super::verify::wait_for_its_remote_deploy(
        config,
        src,
        dest,
        &deploy_message_id,
        evm_gateway_addr,
        evm_rpc_url,
    )
    .await?;

    // Save cache
    let cache = serde_json::json!({
        "tokenId": hex::encode(token_id),
        "salt": hex::encode(salt),
        "mint": mint.to_string(),
    });
    save_its_cache(src, dest, &cache)?;

    Ok((token_id, salt, mint))
}

/// Generate a random 32-byte salt.
fn generate_salt() -> [u8; 32] {
    use rand::Rng;
    let mut salt = [0u8; 32];
    rand::thread_rng().fill(&mut salt);
    salt
}

// ---------------------------------------------------------------------------
// Keypair preparation (reuses sol_to_evm pattern)
// ---------------------------------------------------------------------------

fn prepare_keypairs(
    solana_rpc: &str,
    num_keys: usize,
    main_keypair: &Keypair,
) -> eyre::Result<Vec<Arc<Keypair>>> {
    if num_keys <= 1 {
        return Ok(vec![Arc::new(Keypair::new_from_array(
            main_keypair.to_bytes()[..32].try_into().unwrap(),
        ))]);
    }

    let derived = keypairs::derive_keypairs(main_keypair, num_keys)?;
    let balances = keypairs::ensure_funded(solana_rpc, main_keypair, &derived)?;

    #[allow(clippy::float_arithmetic)]
    let total_sol: f64 = balances.iter().sum::<u64>() as f64 / 1e9;
    ui::success(&format!(
        "funded {} keys ({:.4} SOL)",
        derived.len(),
        total_sol,
    ));

    Ok(derived.into_iter().map(Arc::new).collect())
}

// ---------------------------------------------------------------------------
// Token distribution: create ATAs and transfer tokens
// ---------------------------------------------------------------------------

fn distribute_its_tokens(
    solana_rpc: &str,
    main_keypair: &Keypair,
    keypairs: &[Arc<Keypair>],
    mint: &solana_sdk::pubkey::Pubkey,
    _token_id: &[u8; 32],
    amount_per_key: u64,
) -> eyre::Result<()> {
    use solana_sdk::instruction::{AccountMeta, Instruction};
    use solana_sdk::message::Message;
    use solana_sdk::transaction::Transaction;

    let rpc_client = solana_client::rpc_client::RpcClient::new_with_commitment(
        solana_rpc,
        solana_commitment_config::CommitmentConfig::confirmed(),
    );

    let token_program = solana_sdk::pubkey::Pubkey::from_str_const("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
    let ata_program = solana_sdk::pubkey::Pubkey::from_str_const(
        "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL",
    );

    let fee_payer = main_keypair.pubkey();
    let source_ata = solana_sdk::pubkey::Pubkey::find_program_address(
        &[fee_payer.as_ref(), token_program.as_ref(), mint.as_ref()],
        &ata_program,
    )
    .0;

    let spinner = ui::wait_spinner(&format!(
        "distributing tokens to {} keys...",
        keypairs.len()
    ));

    for (i, kp) in keypairs.iter().enumerate() {
        let wallet = kp.pubkey();
        let dest_ata = solana_sdk::pubkey::Pubkey::find_program_address(
            &[wallet.as_ref(), token_program.as_ref(), mint.as_ref()],
            &ata_program,
        )
        .0;

        // Check if ATA already has enough tokens
        if let Ok(data) = rpc_client.get_account_data(&dest_ata) {
            // Token-2022 account: amount is at offset 64, 8 bytes LE
            if data.len() >= 72 {
                let balance = u64::from_le_bytes(data[64..72].try_into().unwrap_or([0; 8]));
                if balance >= amount_per_key {
                    continue;
                }
            }
        }

        // Build create-ATA-if-needed + transfer instruction
        let mut instructions = Vec::new();

        // Create ATA (idempotent — CreateIdempotent doesn't fail if it exists)
        // CreateIdempotent is instruction index 1 in the ATA program
        let create_ata_ix = Instruction {
            program_id: ata_program,
            accounts: vec![
                AccountMeta::new(fee_payer, true),
                AccountMeta::new(dest_ata, false),
                AccountMeta::new_readonly(wallet, false),
                AccountMeta::new_readonly(*mint, false),
                AccountMeta::new_readonly(
                    solana_sdk::pubkey::Pubkey::from_str_const(
                        "11111111111111111111111111111111",
                    ),
                    false,
                ),
                AccountMeta::new_readonly(token_program, false),
            ],
            data: vec![1], // CreateIdempotent
        };
        instructions.push(create_ata_ix);

        // Transfer tokens (Token-2022 Transfer instruction = index 3)
        let mut transfer_data = vec![3u8]; // Transfer instruction discriminator
        transfer_data.extend_from_slice(&amount_per_key.to_le_bytes());
        let transfer_ix = Instruction {
            program_id: token_program,
            accounts: vec![
                AccountMeta::new(source_ata, false),
                AccountMeta::new(dest_ata, false),
                AccountMeta::new_readonly(fee_payer, true),
            ],
            data: transfer_data,
        };
        instructions.push(transfer_ix);

        let blockhash = rpc_client.get_latest_blockhash()?;
        let message = Message::new_with_blockhash(&instructions, Some(&fee_payer), &blockhash);
        let mut tx = Transaction::new_unsigned(message);
        tx.sign(&[main_keypair], blockhash);
        rpc_client
            .send_and_confirm_transaction(&tx)
            .map_err(|e| eyre!("failed to distribute tokens to key {i}: {e}"))?;

        spinner.set_message(format!(
            "distributing tokens ({}/{} done)...",
            i + 1,
            keypairs.len()
        ));
    }

    spinner.finish_and_clear();
    ui::success(&format!(
        "distributed tokens to {} keys",
        keypairs.len()
    ));
    Ok(())
}
