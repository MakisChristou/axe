use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long to wait for an EVM tx receipt before giving up.
const EVM_RECEIPT_TIMEOUT: Duration = Duration::from_secs(60);

use alloy::{
    primitives::{Address, Bytes, FixedBytes, U256},
    providers::{Provider, ProviderBuilder},
    signers::local::PrivateKeySigner,
};
use eyre::eyre;
use rand::Rng;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signer::Signer;

use super::its_prerequisites::{self, GatewayRequirement};
use super::keypairs;
use super::metrics::{ComputeUnitSummary, LoadTestReport, ReportInput, TxMetrics};
use super::run_sizing::RunSizing;
use super::submitter::TransactionSubmitter;
use super::{
    LoadTestArgs, check_evm_balance, finish_report, read_its_cache, save_its_cache,
    validate_evm_rpc, validate_solana_rpc,
};
use crate::commands::test_its::{
    extract_contract_call_event, extract_token_deployed_event, generate_salt,
};
use crate::config::ChainsConfig;
use crate::evm::{ERC20, InterchainTokenFactory, InterchainTokenService};
use crate::ui;

const TOKEN_NAME: &str = "AXE";
const TOKEN_SYMBOL: &str = "AXE";
const TOKEN_DECIMALS: u8 = 18;

/// Build Borsh-encoded ITS metadata that triggers the memo program on Solana.
///
/// Format: `[4 bytes metadata_version=0] [1 byte encoding=0x00 Borsh] [borsh payload]`
///
/// The Borsh payload contains the memo string as the execution data and the
/// memo program's counter PDA as a required writable account.
///
/// When `extra_accounts > 0`, additional accounts are appended after the counter
/// PDA to inflate the transaction size and exercise ALT (Address Lookup Table)
/// paths on the relayer. The first extra account is a valid ATA for the ITS
/// token mint (writable); the rest are random pubkeys (read-only).
fn build_its_memo_metadata(
    counter_pda: &Pubkey,
    extra_accounts: u32,
    token_mint_ata: Option<&Pubkey>,
) -> Vec<u8> {
    let mut buf = [0u8; 16];
    rand::thread_rng().fill(&mut buf);
    let memo = format!("axe load test {}", hex::encode(buf));
    let memo_bytes = memo.as_bytes();

    let total_accounts = 1 + extra_accounts;

    // Metadata version (4 bytes, all zero)
    let mut metadata = vec![0u8; 4];
    // Encoding scheme: 0x00 = Borsh
    metadata.push(0x00);
    // Borsh payload: [u32 LE payload_length] [payload] [u32 LE account_count] [accounts...]
    metadata.extend(&(memo_bytes.len() as u32).to_le_bytes());
    metadata.extend_from_slice(memo_bytes);
    // Account count
    metadata.extend(&total_accounts.to_le_bytes());
    // Account 0: counter PDA (writable, not signer)
    metadata.extend_from_slice(&counter_pda.to_bytes());
    metadata.push(0x02); // writable=true, signer=false

    // Extra accounts for ALT testing
    for i in 0..extra_accounts {
        if i == 0
            && let Some(ata) = token_mint_ata
        {
            // First extra account: valid ATA (writable)
            metadata.extend_from_slice(&ata.to_bytes());
            metadata.push(0x02); // writable=true, signer=false
            continue;
        }
        // Remaining: random pubkeys (read-only)
        let mut random_key = [0u8; 32];
        rand::thread_rng().fill(&mut random_key);
        metadata.extend_from_slice(&random_key);
        metadata.push(0x00); // read-only, not signer
    }

    metadata
}

pub async fn run(args: LoadTestArgs, _run_start: Instant) -> eyre::Result<()> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;

    let evm_rpc_url = args.source_rpc.clone();

    validate_evm_rpc(&evm_rpc_url).await?;
    validate_solana_rpc(&args.destination_rpc).await?;

    let cfg = ChainsConfig::load(&args.config)?;
    its_prerequisites::verify(&cfg, dest, GatewayRequirement::Required)?;

    ui::kv("source", src);
    ui::kv("destination", dest);
    ui::kv(
        "protocol",
        "ITS with data (interchainTransfer + memo execute)",
    );

    let evm = init_evm_signer_and_provider(&evm_rpc_url, args.private_key.as_deref()).await?;
    let its_addrs = resolve_its_addresses(&cfg, src)?;

    let write_provider = ProviderBuilder::new()
        .wallet(evm.signer.clone())
        .connect_http(evm_rpc_url.parse()?);

    let memo_program_id = super::evm_sender::memo_program_id(args.network);
    let (counter_pda, _) = Pubkey::find_program_address(&[b"counter"], &memo_program_id);
    ui::kv("memo program", &memo_program_id.to_string());
    ui::kv("counter PDA", &counter_pda.to_string());

    let (gas_value_wei, gas_value) = super::its_evm_source::standard_gas_value(&args).await?;

    let sizing = RunSizing::new(&args)?;
    let amount_per_tx = U256::from(1_000_000_000_000_000_000u128); // 1 token
    let amount_per_key = amount_per_tx * U256::from(100);

    let (token_id, token_addr, deploy_message_id) = resolve_its_token(
        &args,
        &write_provider,
        &its_addrs,
        evm.deployer_address,
        gas_value,
        &sizing,
        amount_per_key,
    )
    .await?;

    wait_for_remote_deploy_if_needed(&args, deploy_message_id.as_deref()).await?;

    let extra_accounts = args.extra_accounts;
    let token_mint_ata =
        setup_extra_accounts_ata(&args, token_id, memo_program_id, extra_accounts)?;

    let derived = keypairs::derive_evm_signers(&evm.main_key, sizing.num_keys)?;
    ui::info(&format!("derived {} EVM signing keys", derived.len()));

    let gas_extra_per_key = compute_gas_extra_per_key(sizing, gas_value_wei);
    fund_and_distribute(
        &evm_rpc_url,
        &evm.signer,
        &derived,
        token_addr,
        amount_per_key,
        gas_extra_per_key,
    )
    .await?;

    // Destination is the memo program (not a wallet), since we want it to
    // execute with the interchain token.
    let receiver_bytes = Bytes::from(memo_program_id.to_bytes().to_vec());

    let transfer_ctx = TransferContext {
        its_proxy_addr: its_addrs.its_proxy_addr,
        token_id,
        receiver_bytes,
        amount_per_tx,
        gas_value,
        counter_pda,
        extra_accounts,
        token_mint_ata,
    };

    if !sizing.is_burst() {
        run_sustained_pipeline(&args, &cfg, &evm_rpc_url, &derived, &sizing, &transfer_ctx).await
    } else {
        run_burst_pipeline(&args, &evm_rpc_url, &derived, &sizing, &transfer_ctx).await
    }
}

/// EVM signer + read provider bundle, threaded through the orchestrator.
struct EvmSetup {
    signer: PrivateKeySigner,
    deployer_address: Address,
    main_key: [u8; 32],
}

/// ITS factory + service addresses resolved from config for the source chain.
struct ItsAddrs {
    its_factory_addr: Address,
    its_proxy_addr: Address,
}

/// Per-tx parameters threaded into the sustained / burst pipelines.
struct TransferContext {
    its_proxy_addr: Address,
    token_id: FixedBytes<32>,
    receiver_bytes: Bytes,
    amount_per_tx: U256,
    gas_value: U256,
    counter_pda: Pubkey,
    extra_accounts: u32,
    token_mint_ata: Option<Pubkey>,
}

/// Build the EVM signer, validate the deployer balance, and emit the wallet
/// UI line.
async fn init_evm_signer_and_provider(
    evm_rpc_url: &str,
    private_key: Option<&str>,
) -> eyre::Result<EvmSetup> {
    let private_key = private_key.ok_or_else(|| {
        eyre!("EVM private key required. Set EVM_PRIVATE_KEY env var or use --private-key")
    })?;
    let signer: PrivateKeySigner = private_key.parse()?;
    let deployer_address = signer.address();
    let read_provider = ProviderBuilder::new().connect_http(evm_rpc_url.parse()?);
    check_evm_balance(&read_provider, deployer_address).await?;

    let main_key: [u8; 32] = signer.to_bytes().into();

    {
        let balance: u128 = read_provider.get_balance(deployer_address).await?.to();
        let eth = balance as f64 / 1e18;
        ui::kv("wallet", &format!("{deployer_address} ({eth:.6} ETH)"));
    }

    Ok(EvmSetup {
        signer,
        deployer_address,
        main_key,
    })
}

/// Resolve the ITS factory and service addresses from config and emit the
/// matching UI lines.
fn resolve_its_addresses(cfg: &ChainsConfig, src: &str) -> eyre::Result<ItsAddrs> {
    let src_cfg = cfg
        .chains
        .get(src)
        .ok_or_else(|| eyre!("source chain '{src}' not found in config"))?;
    let its_factory_addr: alloy::primitives::Address = src_cfg
        .contract_address("InterchainTokenFactory", src)?
        .parse()?;
    let its_proxy_addr: alloy::primitives::Address = src_cfg
        .contract_address("InterchainTokenService", src)?
        .parse()?;

    ui::address("ITS factory", &format!("{its_factory_addr}"));
    ui::address("ITS service", &format!("{its_proxy_addr}"));

    Ok(ItsAddrs {
        its_factory_addr,
        its_proxy_addr,
    })
}

/// Parse the user-supplied gas value (wei), defaulting to
/// `default_gas_value_wei`, and emit the matching UI line.
/// Resolve the ITS token: prefer `--token-id`, then a pre-registered AXE from
/// chains-config (`contracts.AXE.tokenId` on the source chain), then a cached
/// entry with sufficient supply, otherwise deploy a fresh interchain token.
async fn resolve_its_token<P: Provider>(
    args: &LoadTestArgs,
    write_provider: &P,
    its_addrs: &ItsAddrs,
    deployer_address: Address,
    gas_value: U256,
    sizing: &RunSizing,
    amount_per_key: U256,
) -> eyre::Result<(FixedBytes<32>, Address, Option<String>)> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;
    let total_supply = U256::from(1_000_000) * U256::from(1_000_000_000_000_000_000u128);

    let its_service = InterchainTokenService::new(its_addrs.its_proxy_addr, write_provider);

    if let Some(ref tid) = args.token_id {
        let token_id: FixedBytes<32> = tid.parse().map_err(|e| eyre!("invalid --token-id: {e}"))?;
        let addr = its_service
            .interchainTokenAddress(token_id)
            .call()
            .await
            .map_err(|e| eyre!("failed to look up token address for {token_id}: {e}"))?;
        ui::kv("token ID (provided)", &format!("{token_id}"));
        ui::address("token address", &format!("{addr}"));
        return Ok((token_id, addr, None));
    }

    let needed = amount_per_key * U256::from(sizing.num_keys);
    if let Some((tid, addr)) = super::helpers::reusable_config_axe(
        &args.config,
        src,
        its_addrs.its_proxy_addr,
        write_provider,
        deployer_address,
        needed,
    )
    .await?
    {
        ui::kv("token ID (chains-config)", &format!("{tid}"));
        ui::address("token address", &format!("{addr}"));
        return Ok((tid, addr, None));
    }

    let cache = read_its_cache(src, dest);
    let cached = cache
        .get("tokenId")
        .and_then(|v| v.as_str())
        .and_then(|tid| tid.parse::<FixedBytes<32>>().ok())
        .zip(
            cache
                .get("tokenAddress")
                .and_then(|v| v.as_str())
                .and_then(|a| a.parse::<Address>().ok()),
        );

    if let Some((tid, addr)) = cached {
        let token = ERC20::new(addr, write_provider);
        let needed = amount_per_key * U256::from(sizing.num_keys);
        let balance = token
            .balanceOf(deployer_address)
            .call()
            .await
            .unwrap_or_default();
        if balance >= needed {
            ui::info(&format!("reusing cached ITS token: {addr}"));
            ui::kv("token ID (cached)", &format!("{tid}"));
            return Ok((tid, addr, None));
        }
        ui::warn("cached token supply insufficient, deploying fresh...");
    }

    deploy_its_token(
        write_provider,
        its_addrs.its_factory_addr,
        deployer_address,
        dest,
        total_supply,
        src,
        gas_value,
    )
    .await
}

/// If a fresh remote deploy was just sent, block until it propagates through
/// the hub to Solana. No-op when the token came from cache or `--token-id`.
async fn wait_for_remote_deploy_if_needed(
    args: &LoadTestArgs,
    deploy_message_id: Option<&str>,
) -> eyre::Result<()> {
    if let Some(deploy_msg_id) = deploy_message_id {
        super::verify::wait_for_its_remote_deploy_to_solana(
            &args.config,
            &args.source_chain,
            &args.destination_chain,
            deploy_msg_id,
            &args.destination_rpc,
            args.network,
        )
        .await?;
    }
    Ok(())
}

/// When `extra_accounts > 0`, derive the ITS token mint ATA for the memo
/// program on Solana, ensure it exists, and return it. Otherwise no-op.
fn setup_extra_accounts_ata(
    args: &LoadTestArgs,
    token_id: FixedBytes<32>,
    memo_program_id: Pubkey,
    extra_accounts: u32,
) -> eyre::Result<Option<Pubkey>> {
    if extra_accounts == 0 {
        return Ok(None);
    }

    // Derive the ITS token mint on Solana and compute an ATA for the memo program.
    let (its_root, _) = crate::solana::find_its_root_pda(args.network);
    let (sol_mint, _) =
        crate::solana::find_interchain_token_pda(args.network, &its_root, token_id.as_slice());
    let token_program = Pubkey::from_str_const("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
    let ata_program = Pubkey::from_str_const("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
    let ata = Pubkey::find_program_address(
        &[
            memo_program_id.as_ref(),
            token_program.as_ref(),
            sol_mint.as_ref(),
        ],
        &ata_program,
    )
    .0;
    ui::kv("extra accounts", &extra_accounts.to_string());
    ui::address("first extra (ATA)", &ata.to_string());

    // Create the ATA on Solana if it doesn't exist yet, so the memo program
    // can transfer tokens to it during execution.
    let sol_keypair = crate::solana::load_keypair(args.keypair.as_deref())?;
    let sol_rpc = solana_client::rpc_client::RpcClient::new_with_commitment(
        &args.destination_rpc,
        solana_commitment_config::CommitmentConfig::finalized(),
    );
    if sol_rpc.get_account_data(&ata).is_err() {
        ui::info("creating ATA on Solana for memo program...");
        let create_ata_ix = solana_sdk::instruction::Instruction {
            program_id: ata_program,
            accounts: vec![
                solana_sdk::instruction::AccountMeta::new(sol_keypair.pubkey(), true),
                solana_sdk::instruction::AccountMeta::new(ata, false),
                solana_sdk::instruction::AccountMeta::new_readonly(memo_program_id, false),
                solana_sdk::instruction::AccountMeta::new_readonly(sol_mint, false),
                solana_sdk::instruction::AccountMeta::new_readonly(
                    Pubkey::from_str_const("11111111111111111111111111111111"),
                    false,
                ),
                solana_sdk::instruction::AccountMeta::new_readonly(token_program, false),
            ],
            data: vec![1], // CreateIdempotent
        };
        let blockhash = sol_rpc.get_latest_blockhash()?;
        let tx = solana_sdk::transaction::Transaction::new_signed_with_payer(
            &[create_ata_ix],
            Some(&sol_keypair.pubkey()),
            &[&sol_keypair],
            blockhash,
        );
        sol_rpc
            .send_and_confirm_transaction(&tx)
            .map_err(|e| eyre!("failed to create ATA: {e}"))?;
        ui::success("ATA created");
    } else {
        ui::info("ATA already exists");
    }

    Ok(Some(ata))
}

/// Per-key extra gas budget: 1x in burst mode, buffered rounds-per-key in
/// sustained mode. ITS hub routing pays 2× gas_value per transfer (two
/// commands).
fn compute_gas_extra_per_key(sizing: RunSizing, gas_value_wei: u128) -> u128 {
    let hub_gas_value_wei = gas_value_wei.saturating_mul(2);
    if sizing.is_burst() {
        hub_gas_value_wei
    } else {
        let rounds = sizing.transactions_per_key();
        let buffered = rounds + rounds / 5 + 1;
        hub_gas_value_wei.saturating_mul(buffered as u128)
    }
}

/// Fund the derived EVM wallets with the gas budget for the planned run and
/// distribute the per-key ITS token allocation.
async fn fund_and_distribute(
    evm_rpc_url: &str,
    signer: &PrivateKeySigner,
    derived: &[PrivateKeySigner],
    token_addr: Address,
    amount_per_key: U256,
    gas_extra_per_key: u128,
) -> eyre::Result<()> {
    let funding_provider = ProviderBuilder::new()
        .wallet(signer.clone())
        .connect_http(evm_rpc_url.parse()?);
    keypairs::ensure_funded_evm_with_extra(&funding_provider, signer, derived, gas_extra_per_key)
        .await?;

    let token_provider = ProviderBuilder::new()
        .wallet(signer.clone())
        .connect_http(evm_rpc_url.parse()?);
    super::its_evm_source::distribute_tokens(&token_provider, token_addr, derived, amount_per_key)
        .await?;
    Ok(())
}

/// Drive the sustained-mode pipeline: spawn the streaming verifier, run the
/// sustained sender, stitch amplifier timings into the report, and hand off
/// to `finish_report`.
async fn run_sustained_pipeline(
    args: &LoadTestArgs,
    cfg: &ChainsConfig,
    evm_rpc_url: &str,
    derived: &[PrivateKeySigner],
    sizing: &RunSizing,
    transfer_ctx: &TransferContext,
) -> eyre::Result<()> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;
    let (tps, duration_secs, key_cycle) = sizing.sustained().expect("sustained mode");
    let nonce_provider = ProviderBuilder::new().connect_http(evm_rpc_url.parse()?);
    let mut nonces: Vec<u64> = Vec::with_capacity(sizing.num_keys);
    for signer in derived {
        let n = nonce_provider
            .get_transaction_count(signer.address())
            .await?;
        nonces.push(n);
    }

    // Streaming verification: run concurrently with sends.
    let (verify_tx, verify_rx) = tokio::sync::mpsc::unbounded_channel();
    let send_done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (spinner_tx, spinner_rx) = tokio::sync::oneshot::channel::<indicatif::ProgressBar>();

    // Check if source chain has a voting verifier.
    let has_voting_verifier = cfg
        .axelar
        .contract_address("VotingVerifier", &args.source_chain)
        .is_ok();

    let vconfig = args.config.clone();
    let vsource = args.source_axelar_id.clone();
    let vdest = args.destination_axelar_id.clone();
    let vdest_rpc = args.destination_rpc.clone();
    let vdone = std::sync::Arc::clone(&send_done);
    let vnetwork = args.network;
    let verify_handle = tokio::spawn(async move {
        let spinner = spinner_rx.await.expect("spinner channel dropped");
        super::verify::verify_onchain_solana_its_streaming(super::verify::StreamingVerification {
            route: super::verify::VerificationRoute {
                config: &vconfig,
                source_chain: &vsource,
                destination_chain: &vdest,
                network: vnetwork,
            },
            destination: super::verify::SolanaItsDestination {
                rpc_url: &vdest_rpc,
            },
            rx: verify_rx,
            send_done: vdone,
            spinner,
        })
        .await
    });

    let spinner = ui::wait_spinner(&format!(
        "[0/{duration_secs}s] starting sustained ITS-with-data send..."
    ));
    let _ = spinner_tx.send(spinner.clone());

    let test_start = Instant::now();
    let make_task = its_with_data_sustained_tasks(
        ItsEvmWithDataSubmitter::new(evm_rpc_url, dest, transfer_ctx)?,
        derived.to_vec(),
        verify_tx,
        has_voting_verifier,
    );

    let result = super::sustained::run_sustained_loop(
        tps,
        duration_secs,
        key_cycle,
        Some(nonces),
        make_task,
        Some(send_done),
        spinner,
    )
    .await?;

    let mut report = super::sustained::build_sustained_report(
        result,
        src,
        dest,
        &format!("{}", transfer_ctx.its_proxy_addr),
        sizing.total_expected,
        sizing.num_keys,
    );

    // Wait for verification to finish.
    let (verification, timings) = verify_handle.await??;
    for (msg_id, timing) in timings {
        if let Some(tx) = report
            .transactions
            .iter_mut()
            .find(|t| t.signature == msg_id)
        {
            tx.amplifier_timing = Some(timing);
        }
    }
    report.verification = Some(verification);

    finish_report(args, &mut report, test_start)
}

/// Drive the burst-mode pipeline: fan out the EVM ITS-with-data transfers,
/// batch-verify on Solana, and hand off to `finish_report`.
async fn run_burst_pipeline(
    args: &LoadTestArgs,
    evm_rpc_url: &str,
    derived: &[PrivateKeySigner],
    sizing: &RunSizing,
    transfer_ctx: &TransferContext,
) -> eyre::Result<()> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;
    let num_txs = sizing
        .burst_count()
        .expect("burst pipeline requires burst sizing");
    let its_proxy_addr = transfer_ctx.its_proxy_addr;

    let test_start = Instant::now();
    let jobs = derived
        .iter()
        .cloned()
        .map(|signer| ItsEvmWithDataSubmitJob {
            signer,
            explicit_nonce: None,
            retry_rate_limits: true,
        })
        .collect();
    let burst = super::submitter::run_burst(
        ItsEvmWithDataSubmitter::new(evm_rpc_url, dest, transfer_ctx)?,
        jobs,
        100,
    )
    .await?;
    let metrics = burst.metrics;
    let total_failed = metrics.iter().filter(|m| !m.is_success()).count() as u64;

    if total_failed > 0 {
        let mut error_counts: std::collections::HashMap<String, u64> =
            std::collections::HashMap::new();
        for m in metrics.iter().filter(|m| !m.is_success()) {
            let reason = m
                .error()
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

    let mut report = LoadTestReport::from_transactions(
        ReportInput {
            source_chain: src.to_string(),
            destination_chain: dest.to_string(),
            destination_address: format!("{its_proxy_addr}"),
            num_txs: args.num_txs,
            num_keys: num_txs,
            total_submitted: burst.total_submitted,
            test_duration_secs: burst.test_duration_secs,
            compute_unit_summary: ComputeUnitSummary::Omit,
        },
        metrics,
    );

    let verification =
        super::verify::verify_onchain_solana_its(super::verify::ItsBatchVerification {
            route: super::verify::VerificationRoute {
                config: &args.config,
                source_chain: &args.source_axelar_id,
                destination_chain: &args.destination_axelar_id,
                network: args.network,
            },
            destination: super::verify::SolanaItsDestination {
                rpc_url: &args.destination_rpc,
            },
            metrics: &mut report.transactions,
        })
        .await?;
    report.verification = Some(verification);

    finish_report(args, &mut report, test_start)
}

/// Deploy a new interchain token and its remote counterpart.
async fn deploy_its_token<P: Provider>(
    provider: &P,
    factory_addr: Address,
    deployer: Address,
    dest_chain: &str,
    total_supply: U256,
    source_chain: &str,
    gas_value: U256,
) -> eyre::Result<(FixedBytes<32>, Address, Option<String>)> {
    let salt = generate_salt();

    ui::info("deploying new ITS token...");
    ui::kv("name", TOKEN_NAME);
    ui::kv("symbol", TOKEN_SYMBOL);
    ui::kv("decimals", &TOKEN_DECIMALS.to_string());
    ui::kv("supply", &format!("{total_supply}"));

    let factory = InterchainTokenFactory::new(factory_addr, provider);

    let deploy_call = factory
        .deployInterchainToken(
            salt,
            TOKEN_NAME.to_string(),
            TOKEN_SYMBOL.to_string(),
            TOKEN_DECIMALS,
            total_supply,
            deployer,
        )
        .value(U256::ZERO);

    let pending = deploy_call.send().await?;
    let tx_hash = *pending.tx_hash();
    ui::tx_hash("deploy tx", &format!("{tx_hash}"));

    let receipt = tokio::time::timeout(Duration::from_secs(120), pending.get_receipt())
        .await
        .map_err(|_| eyre!("deploy tx timed out after 120s"))??;

    let (token_id, token_addr) = extract_token_deployed_event(&receipt)?;
    ui::kv("token ID", &format!("{token_id}"));
    ui::address("token address", &format!("{token_addr}"));

    ui::info(&format!("deploying remote token to {dest_chain}..."));

    // ITS routes via the hub, so two commands are created (source→hub and
    // hub→destination). Pay 2× gas_value so both legs are covered.
    let hub_gas = gas_value * U256::from(2);
    let remote_call = factory
        .deployRemoteInterchainToken(salt, dest_chain.to_string(), hub_gas)
        .value(hub_gas);

    let pending = remote_call.send().await?;
    let tx_hash = *pending.tx_hash();
    ui::tx_hash("remote deploy tx", &format!("{tx_hash}"));

    let receipt = tokio::time::timeout(Duration::from_secs(120), pending.get_receipt())
        .await
        .map_err(|_| eyre!("remote deploy tx timed out after 120s"))??;

    ui::success(&format!(
        "remote deploy confirmed in block {}",
        receipt.block_number.unwrap_or(0)
    ));

    let deploy_message_id = match extract_contract_call_event(&receipt) {
        Ok((event_index, _, _, _, _)) => {
            let msg_id = format!("{tx_hash:#x}-{event_index}");
            ui::kv("remote deploy message ID", &msg_id);
            Some(msg_id)
        }
        Err(_) => None,
    };

    let cache = serde_json::json!({
        "tokenId": format!("{token_id}"),
        "tokenAddress": format!("{token_addr}"),
        "salt": format!("{salt}"),
    });
    save_its_cache(source_chain, dest_chain, &cache)?;

    Ok((token_id, token_addr, deploy_message_id))
}

/// EVM submission capability for the Solana ITS-with-data route.
///
/// The route still owns memo-account selection and destination verification;
/// this adapter owns provider construction, transaction submission, and the
/// route's rate-limit policy.
#[derive(Clone)]
struct ItsEvmWithDataSubmitter {
    rpc_url: reqwest::Url,
    its_proxy: Address,
    token_id: FixedBytes<32>,
    destination_chain: String,
    receiver: Bytes,
    amount: U256,
    gas_value: U256,
    counter_pda: Pubkey,
    extra_accounts: u32,
    token_mint_ata: Option<Pubkey>,
}

impl ItsEvmWithDataSubmitter {
    fn new(
        evm_rpc_url: &str,
        destination_chain: &str,
        transfer: &TransferContext,
    ) -> eyre::Result<Self> {
        Ok(Self {
            rpc_url: evm_rpc_url.parse()?,
            its_proxy: transfer.its_proxy_addr,
            token_id: transfer.token_id,
            destination_chain: destination_chain.to_string(),
            receiver: transfer.receiver_bytes.clone(),
            amount: transfer.amount_per_tx,
            gas_value: transfer.gas_value,
            counter_pda: transfer.counter_pda,
            extra_accounts: transfer.extra_accounts,
            token_mint_ata: transfer.token_mint_ata,
        })
    }
}

struct ItsEvmWithDataSubmitJob {
    signer: PrivateKeySigner,
    explicit_nonce: Option<u64>,
    retry_rate_limits: bool,
}

impl TransactionSubmitter for ItsEvmWithDataSubmitter {
    type Job = ItsEvmWithDataSubmitJob;

    async fn submit(&self, job: Self::Job) -> TxMetrics {
        let provider = ProviderBuilder::new()
            .wallet(job.signer)
            .connect_http(self.rpc_url.clone());
        let submit = || {
            execute_interchain_transfer_with_data(InterchainTransferWithDataRequest {
                provider: &provider,
                its_proxy: self.its_proxy,
                token_id: self.token_id,
                destination_chain: &self.destination_chain,
                receiver: &self.receiver,
                amount: self.amount,
                gas_value: self.gas_value,
                counter_pda: &self.counter_pda,
                extra_accounts: self.extra_accounts,
                token_mint_ata: self.token_mint_ata.as_ref(),
                explicit_nonce: job.explicit_nonce,
            })
        };
        if job.retry_rate_limits {
            super::retry::rate_limited(submit).await
        } else {
            submit().await
        }
    }
}

fn its_with_data_sustained_tasks(
    submitter: ItsEvmWithDataSubmitter,
    signers: Vec<PrivateKeySigner>,
    verify_tx: tokio::sync::mpsc::UnboundedSender<super::verify::PendingTx>,
    has_voting_verifier: bool,
) -> super::sustained::MakeTask {
    let submitter = Arc::new(submitter);
    let signers = Arc::new(signers);
    Box::new(move |key_idx: usize, nonce: Option<u64>| {
        let submitter = Arc::clone(&submitter);
        let signers = Arc::clone(&signers);
        let verify_tx = verify_tx.clone();
        Box::pin(async move {
            let mut result = submitter
                .submit(ItsEvmWithDataSubmitJob {
                    signer: signers[key_idx].clone(),
                    explicit_nonce: nonce,
                    retry_rate_limits: false,
                })
                .await;
            if result.is_success() {
                match super::verify::tx_to_pending_its(&result, has_voting_verifier) {
                    Ok(pending) => {
                        let _ = verify_tx.send(pending);
                    }
                    Err(error) => {
                        result.mark_failed(format!("failed to build verification state: {error}"));
                    }
                }
            }
            result
        })
    })
}

/// Send a single interchainTransfer with metadata that triggers the memo program.
struct InterchainTransferWithDataRequest<'a, P> {
    provider: &'a P,
    its_proxy: Address,
    token_id: FixedBytes<32>,
    destination_chain: &'a str,
    receiver: &'a Bytes,
    amount: U256,
    gas_value: U256,
    counter_pda: &'a Pubkey,
    extra_accounts: u32,
    token_mint_ata: Option<&'a Pubkey>,
    explicit_nonce: Option<u64>,
}

async fn execute_interchain_transfer_with_data<P: Provider>(
    request: InterchainTransferWithDataRequest<'_, P>,
) -> TxMetrics {
    let InterchainTransferWithDataRequest {
        provider,
        its_proxy,
        token_id,
        destination_chain: dest_chain,
        receiver: receiver_bytes,
        amount,
        gas_value,
        counter_pda,
        extra_accounts,
        token_mint_ata,
        explicit_nonce,
    } = request;
    let submit_start = Instant::now();

    // Build unique metadata per tx (random memo string)
    let metadata = Bytes::from(build_its_memo_metadata(
        counter_pda,
        extra_accounts,
        token_mint_ata,
    ));

    // ITS routes via the hub, so two commands are created (source→hub and
    // hub→destination). Pay 2× gas_value so both legs are covered.
    let hub_gas = gas_value * U256::from(2);
    let its = InterchainTokenService::new(its_proxy, provider);
    let base_call = its
        .interchainTransfer(
            token_id,
            dest_chain.to_string(),
            receiver_bytes.clone(),
            amount,
            metadata,
            hub_gas,
        )
        .value(hub_gas);
    let call = match explicit_nonce {
        Some(n) => base_call.nonce(n),
        None => base_call,
    };

    match call.send().await {
        Ok(pending) => {
            let tx_hash = *pending.tx_hash();
            match tokio::time::timeout(EVM_RECEIPT_TIMEOUT, pending.get_receipt()).await {
                Ok(Ok(receipt)) => {
                    let latency_ms = submit_start.elapsed().as_millis() as u64;

                    match extract_contract_call_event(&receipt) {
                        Ok((
                            event_index,
                            _payload,
                            payload_hash_bytes,
                            dest_chain,
                            dest_address,
                        )) => {
                            let message_id = format!("{tx_hash:#x}-{event_index}");
                            let source_address = format!("{its_proxy}");
                            let payload_hash = alloy::hex::encode(payload_hash_bytes.as_slice());

                            TxMetrics {
                                signature: message_id,
                                submit_time_ms: 0,
                                confirm_time_ms: Some(latency_ms),
                                latency_ms: Some(latency_ms),
                                compute_units: Some(receipt.gas_used),
                                slot: receipt.block_number,
                                outcome: TxMetrics::succeeded_outcome(),
                                payload: Vec::new(),
                                payload_hash,
                                source_address,
                                gmp_destination_chain: dest_chain,
                                gmp_destination_address: dest_address,
                                send_instant: Some(submit_start),
                                amplifier_timing: None,
                            }
                        }
                        Err(e) => {
                            make_failure(submit_start, &format!("no ContractCall event: {e}"))
                        }
                    }
                }
                Ok(Err(e)) => make_failure_with_hash(submit_start, &e.to_string(), Some(tx_hash)),
                Err(_) => make_failure_with_hash(submit_start, "tx timed out", Some(tx_hash)),
            }
        }
        Err(e) => make_failure(submit_start, &e.to_string()),
    }
}

fn make_failure(submit_start: Instant, error: &str) -> TxMetrics {
    make_failure_with_hash(submit_start, error, None)
}

fn make_failure_with_hash(
    submit_start: Instant,
    error: &str,
    tx_hash: Option<alloy::primitives::TxHash>,
) -> TxMetrics {
    let elapsed_ms = submit_start.elapsed().as_millis() as u64;
    TxMetrics {
        signature: tx_hash.map_or_else(String::new, |h| format!("{h:#x}")),
        submit_time_ms: elapsed_ms,
        confirm_time_ms: None,
        latency_ms: None,
        compute_units: None,
        slot: None,
        outcome: TxMetrics::failed_outcome(error.to_string()),
        payload: Vec::new(),
        payload_hash: String::new(),
        source_address: String::new(),
        gmp_destination_chain: String::new(),
        gmp_destination_address: String::new(),
        send_instant: None,
        amplifier_timing: None,
    }
}
