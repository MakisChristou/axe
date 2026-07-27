//! Stellar -> Solana ITS load test.
//!
//! Mirrors `its_stellar_to_evm.rs` but with a Solana destination:
//!   1. Deploy the AXE interchain token on Stellar (or reuse cached token_id)
//!   2. Register it on the Solana destination via `deploy_remote_interchain_token`
//!   3. Wait for the remote-deploy to land on the Solana ITS program
//!   4. Distribute AXE balances to ephemeral Stellar wallets
//!   5. Fire `interchain_transfer` calls (burst or sustained)
//!   6. Verify through Amplifier (voted → hub_approved → routed → approved → executed)

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use eyre::{Result, eyre};
use futures::future::join_all;
use rand::RngCore;
use solana_sdk::signer::Signer;
use tokio::sync::Mutex;

use super::its_stellar_source::{
    self, SustainedTransferArgs, SustainedTransferContext, TransferRequest,
};
use super::metrics::{ComputeUnitSummary, LoadTestReport, ReportInput, TxMetrics};
use super::run_sizing::RunSizing;
use super::sustained;
use super::{LoadTestArgs, finish_report, read_its_cache, save_its_cache, validate_solana_rpc};
use crate::config::ChainsConfig;
use crate::stellar::{StellarClient, StellarWallet};
use crate::ui;

/// AXE token parameters on Stellar — match the EVM/Solana siblings so the
/// human-facing name is consistent across runs.
const TOKEN_NAME: &str = "AXE";
const TOKEN_SYMBOL: &str = "AXE";
/// 7 decimals matches Stellar's native XLM convention. Used only for the
/// FRESH-deploy path; the reuse path adopts the existing token's decimals
/// (queried via `StellarClient::token_decimals`). Token amounts on the
/// destination chain are scaled by ITS during routing.
const TOKEN_DECIMALS: u32 = 7;

/// Whole tokens transferred per tx / seeded per key. Scaled by the resolved
/// token's actual on-chain decimals at runtime — the reused canonical AXE is
/// 18 decimals (100 AXE = 1e20, which overflows u64), so amounts are u128.
const WHOLE_TOKENS_PER_TX: u128 = 1;
/// Distribute 100x per key so cached tokens last across many runs.
const WHOLE_TOKENS_PER_KEY: u128 = WHOLE_TOKENS_PER_TX * 100;
/// Initial supply minted to the deployer at deploy time (fresh-deploy path,
/// TOKEN_DECIMALS = 7). Plenty for many runs without redeploying.
const INITIAL_SUPPLY: u128 = 1_000_000 * 10_000_000;

/// Scale a whole-token count to a token's on-chain decimals (`n * 10^decimals`).
fn scale_to_decimals(whole_tokens: u128, decimals: u32) -> u128 {
    whole_tokens * 10u128.pow(decimals)
}

/// Default cross-chain gas payment in stroops (10 XLM). Matches the GMP
/// runner's default — overridable via `--gas-value`.
const DEFAULT_GAS_STROOPS: u64 = 100_000_000;

pub async fn run(args: LoadTestArgs, _run_start: Instant, sizing: RunSizing) -> Result<()> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;

    let solana_rpc_url = args.destination_rpc.clone();
    validate_solana_rpc(&solana_rpc_url).await?;

    let cfg = ChainsConfig::load(&args.config)?;
    verify_axelar_prerequisites(&cfg, dest)?;

    ui::kv("source", src);
    ui::kv("destination", dest);
    ui::kv("protocol", "ITS (interchainTransfer via hub)");

    let stellar = read_stellar_setup(&args.config, src, &args.source_rpc)?;
    let main_wallet = init_stellar_main_wallet(
        &stellar.client,
        args.private_key.as_deref(),
        stellar.use_friendbot,
    )
    .await?;
    let solana = resolve_solana_target(args.keypair.as_deref(), args.network)?;
    let gas_stroops = parse_gas_stroops(args.gas_value.as_deref())?;

    let (token_id, wallets, amount_per_tx) = prepare_token_and_wallets(
        &args,
        &stellar,
        &main_wallet,
        &solana_rpc_url,
        gas_stroops,
        &sizing,
    )
    .await?;

    let transfer = ItsTransferSpec {
        token_id,
        gas_stroops,
        axelarnet_gw_addr: cfg
            .axelar
            .global_contract_address("AxelarnetGateway")?
            .to_string(),
        amount_per_tx,
    };

    if !sizing.is_burst() {
        run_sustained_pipeline(&args, &stellar, &solana, wallets, &transfer, &sizing).await
    } else {
        run_burst_pipeline(&args, &stellar, &solana, wallets, &transfer, &sizing).await
    }
}

/// Stellar source-side configuration: the connected client, the contract
/// addresses needed for ITS calls, and whether the network supports
/// Friendbot-based account activation.
struct StellarSetup {
    client: StellarClient,
    its_addr: String,
    gateway_addr: String,
    xlm_addr: String,
    use_friendbot: bool,
}

/// Solana destination identity: the recipient pubkey and the equivalent
/// 32-byte address used as the ITS payload destination.
struct SolanaTarget {
    recipient: solana_sdk::pubkey::Pubkey,
    address_bytes: Vec<u8>,
}

/// Per-transfer payload bits common to burst and sustained modes: the
/// deployed ITS token, the gas value, and the GMP-hub destination
/// (AxelarnetGateway) used for verification routing.
struct ItsTransferSpec {
    token_id: [u8; 32],
    gas_stroops: u64,
    axelarnet_gw_addr: String,
    /// Per-transfer amount, already scaled to the resolved token's decimals.
    amount_per_tx: u128,
}

/// Verify Axelar-side prerequisites (cosmos Gateway for `dest`, global
/// AxelarnetGateway). Bails with the existing error strings if either is
/// missing.
fn verify_axelar_prerequisites(cfg: &ChainsConfig, dest: &str) -> Result<()> {
    if cfg.axelar.contract_address("Gateway", dest).is_err() {
        eyre::bail!(
            "destination chain '{dest}' has no Cosmos Gateway in the config — verification would fail."
        );
    }
    if cfg
        .axelar
        .global_contract_address("AxelarnetGateway")
        .is_err()
    {
        eyre::bail!("no AxelarnetGateway address in config — required for ITS load test");
    }
    Ok(())
}

/// Read the Stellar source-chain config (network type, ITS / gateway / XLM
/// addresses) and emit the matching UI lines. Returns a bundle ready for
/// downstream stages.
fn read_stellar_setup(
    config: &std::path::Path,
    src: &str,
    stellar_rpc: &str,
) -> Result<StellarSetup> {
    let network_type = super::read_stellar_network_type(config, src)?;
    let client = StellarClient::new(stellar_rpc, &network_type)?;
    let its_addr = super::read_stellar_contract_address(config, src, "InterchainTokenService")?;
    let gateway_addr = super::read_stellar_contract_address(config, src, "AxelarGateway")?;
    let xlm_addr = super::read_stellar_token_address(config, src)?;
    ui::address("Stellar ITS", &its_addr);
    ui::address("Stellar AxelarGateway", &gateway_addr);
    ui::address("Stellar XLM token", &xlm_addr);

    let use_friendbot = matches!(network_type.as_str(), "testnet" | "futurenet");
    Ok(StellarSetup {
        client,
        its_addr,
        gateway_addr,
        xlm_addr,
        use_friendbot,
    })
}

/// Load the Stellar main wallet and ensure it is activated. For ITS the main
/// wallet itself signs deploy + distribution txs, so it must be activated.
/// (GMP doesn't need this — ephemeral wallets sign there.) Friendbot it on
/// testnet/futurenet; otherwise leave to the user.
async fn init_stellar_main_wallet(
    client: &StellarClient,
    private_key: Option<&str>,
    use_friendbot: bool,
) -> Result<StellarWallet> {
    let main_wallet = super::load_stellar_main_wallet(private_key)?;
    ui::kv("Stellar wallet", &main_wallet.address());

    if client
        .account_sequence(&main_wallet.address())
        .await?
        .is_none()
    {
        if use_friendbot {
            ui::info("activating Stellar main wallet via Friendbot...");
            client.friendbot_fund(&main_wallet.address()).await?;
            ui::success("main wallet activated");
        } else {
            eyre::bail!(
                "Stellar main wallet {} is not activated — fund it manually (need ≥ 2 XLM \
                 base reserve plus enough for token deploys + per-key distribution).",
                main_wallet.address()
            );
        }
    }
    Ok(main_wallet)
}

/// Resolve the Solana destination: load the keypair (only used for its
/// pubkey — the relayer drives destination-side execution), build the 32-byte
/// address, and emit the matching UI lines.
fn resolve_solana_target(
    keypair: Option<&str>,
    network: crate::types::Network,
) -> Result<SolanaTarget> {
    let sol_keypair = crate::solana::load_keypair(keypair)?;
    let recipient = sol_keypair.pubkey();
    let address_bytes = recipient.to_bytes().to_vec();
    ui::kv("Solana recipient", &recipient.to_string());
    ui::address("Solana ITS program", &network.solana_its_id().to_string());
    Ok(SolanaTarget {
        recipient,
        address_bytes,
    })
}

/// Parse the user-supplied gas value (XLM stroops), defaulting to
/// `DEFAULT_GAS_STROOPS`, and emit the matching UI line. ITS routes via the
/// hub (two commands: source→hub, hub→destination), so we pay 2× the
/// per-command gas value.
fn parse_gas_stroops(gas_value: Option<&str>) -> Result<u64> {
    let gas_stroops: u64 = match gas_value {
        Some(v) => v
            .parse::<u64>()
            .map_err(|e| eyre!("invalid --gas-value: {e}"))?,
        None => DEFAULT_GAS_STROOPS,
    }
    .saturating_mul(2);
    ui::kv(
        "gas",
        &format!(
            "{gas_stroops} stroops ({:.4} XLM)",
            gas_stroops as f64 / 10_000_000.0
        ),
    );
    Ok(gas_stroops)
}

/// Compute the AXE amount each ephemeral wallet should hold so it can run all
/// of its planned transfers, scaled to the resolved token's `decimals`
/// (sustained budgets 2x the txs-per-key cap to absorb retries; burst uses the
/// static per-key amount).
fn compute_amount_per_key(sizing: &RunSizing, key_cycle: u64, decimals: u32) -> u128 {
    if sizing.is_burst() {
        scale_to_decimals(WHOLE_TOKENS_PER_KEY, decimals) / 100
    } else {
        let (_, duration_secs, _) = sizing.sustained().expect("sustained mode");
        let txs_per_key = duration_secs.div_ceil(key_cycle) as u128;
        (scale_to_decimals(WHOLE_TOKENS_PER_TX, decimals) / 100)
            .saturating_mul(txs_per_key)
            .saturating_mul(2)
    }
}

/// Derive `num_keys` ephemeral Stellar wallets from the main wallet's seed
/// and ensure each is activated.
async fn derive_and_fund_ephemeral_wallets(
    client: &StellarClient,
    main_wallet: &StellarWallet,
    num_keys: usize,
    use_friendbot: bool,
    gas_stroops: u64,
    txs_per_key: u64,
) -> Result<Vec<StellarWallet>> {
    ui::info(&format!("deriving {num_keys} Stellar keys..."));
    let main_seed = main_wallet.signing_key.to_bytes();
    let wallets = super::stellar_sender::derive_wallets(&main_seed, num_keys)?;
    let _ = main_seed;
    let mainnet_starting_balance =
        super::stellar_sender::mainnet_per_key_balance_stroops(gas_stroops, txs_per_key);
    super::stellar_sender::ensure_funded(
        client,
        &wallets,
        use_friendbot,
        main_wallet,
        mainnet_starting_balance,
    )
    .await?;
    Ok(wallets)
}

/// Run the cache-or-deploy ITS token setup, derive + fund the ephemeral
/// wallets, then distribute AXE to each wallet according to the run sizing.
/// Returns `(token_id, wallets)` ready for the burst/sustained pipelines.
async fn prepare_token_and_wallets(
    args: &LoadTestArgs,
    stellar: &StellarSetup,
    main_wallet: &StellarWallet,
    solana_rpc_url: &str,
    gas_stroops: u64,
    sizing: &RunSizing,
) -> Result<([u8; 32], Vec<StellarWallet>, u128)> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;

    let (token_id, _salt, token_address, decimals) = setup_its_token(
        &stellar.client,
        main_wallet,
        args.network,
        &stellar.its_addr,
        &stellar.gateway_addr,
        &stellar.xlm_addr,
        gas_stroops,
        src,
        dest,
        &args.destination_axelar_id,
        args.token_id.as_deref(),
        &args.config,
        solana_rpc_url,
        sizing.num_keys,
    )
    .await?;
    ui::kv("token ID", &hex::encode(token_id));
    ui::address("token contract (Stellar)", &token_address);

    let txs_per_key = if sizing.is_burst() { 1 } else { args.key_cycle };
    let wallets = derive_and_fund_ephemeral_wallets(
        &stellar.client,
        main_wallet,
        sizing.num_keys,
        stellar.use_friendbot,
        gas_stroops,
        txs_per_key,
    )
    .await?;

    let amount_per_key = compute_amount_per_key(sizing, args.key_cycle, decimals);
    distribute_token_balances(
        &stellar.client,
        main_wallet,
        &token_address,
        &wallets,
        amount_per_key,
    )
    .await?;

    // /100 → 0.01 whole tokens per tx so the cron's source-side supply lasts.
    let amount_per_tx = scale_to_decimals(WHOLE_TOKENS_PER_TX, decimals) / 100;
    Ok((token_id, wallets, amount_per_tx))
}

/// Drive the sustained-mode pipeline: spawn the streaming verifier, run the
/// Stellar sustained sender, stitch amplifier timings back into the report,
/// and hand off to `finish_report`.
async fn run_sustained_pipeline(
    args: &LoadTestArgs,
    stellar: &StellarSetup,
    solana: &SolanaTarget,
    wallets: Vec<StellarWallet>,
    transfer: &ItsTransferSpec,
    sizing: &RunSizing,
) -> Result<()> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;
    let (tps_n, duration_secs, key_cycle) = sizing.sustained().expect("sustained mode");

    let (verify_tx, verify_rx) = tokio::sync::mpsc::unbounded_channel();
    let send_done = Arc::new(AtomicBool::new(false));
    let (spinner_tx, spinner_rx) = tokio::sync::oneshot::channel::<indicatif::ProgressBar>();

    let vconfig = args.config.clone();
    let vsource = args.source_axelar_id.clone();
    let vdest = args.destination_axelar_id.clone();
    let vdest_rpc = args.destination_rpc.clone();
    let vdone = Arc::clone(&send_done);
    let vnetwork = args.network;
    let verify_handle = tokio::spawn(async move {
        let spinner = spinner_rx.await.expect("spinner channel dropped");
        super::verify::verify_onchain_solana_its_streaming(
            &vconfig, &vsource, &vdest, &vdest_rpc, vnetwork, verify_rx, vdone, spinner,
        )
        .await
    });

    let spinner = ui::wait_spinner(&format!(
        "[0/{duration_secs}s] starting sustained Stellar ITS send..."
    ));
    let _ = spinner_tx.send(spinner.clone());

    let test_start = Instant::now();
    let result = its_stellar_source::run_sustained(SustainedTransferArgs {
        context: SustainedTransferContext {
            client: stellar.client.clone(),
            wallets,
            its_contract: stellar.its_addr.clone(),
            gateway_contract: stellar.gateway_addr.clone(),
            token_id: transfer.token_id,
            destination_chain: args.destination_axelar_id.clone(),
            destination_address_bytes: solana.address_bytes.clone(),
            gas_token: stellar.xlm_addr.clone(),
            gas_stroops: transfer.gas_stroops,
            amount_per_tx: transfer.amount_per_tx,
            axelarnet_gw_addr: transfer.axelarnet_gw_addr.clone(),
        },
        tps: tps_n,
        duration_secs,
        key_cycle,
        verify_tx: Some(verify_tx),
        send_done: Some(send_done),
        spinner,
    })
    .await;

    let mut report = sustained::build_sustained_report(
        result,
        src,
        dest,
        &solana.recipient.to_string(),
        sizing.total_expected,
        sizing.num_keys,
    );
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

/// Drive the burst-mode pipeline: fan out the Stellar transfers, build the
/// load-test report, run the Solana ITS verifier on the confirmed batch, and
/// hand off to `finish_report`.
async fn run_burst_pipeline(
    args: &LoadTestArgs,
    stellar: &StellarSetup,
    solana: &SolanaTarget,
    wallets: Vec<StellarWallet>,
    transfer: &ItsTransferSpec,
    sizing: &RunSizing,
) -> Result<()> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;
    let num_keys = sizing.num_keys;
    let token_id = transfer.token_id;
    let gas_stroops = transfer.gas_stroops;
    let amount_per_tx = transfer.amount_per_tx;

    let test_start = Instant::now();
    let metrics_list: Arc<Mutex<Vec<TxMetrics>>> = Arc::new(Mutex::new(Vec::new()));
    let confirmed = Arc::new(AtomicU64::new(0));
    let spinner = ui::wait_spinner(&format!("sending (0/{num_keys} confirmed)..."));

    let client = Arc::new(stellar.client.clone());
    let stellar_its_arc = Arc::new(stellar.its_addr.clone());
    let stellar_gw_arc = Arc::new(stellar.gateway_addr.clone());
    let stellar_xlm_arc = Arc::new(stellar.xlm_addr.clone());
    let dest_chain_arc = Arc::new(args.destination_axelar_id.clone());
    let dest_addr_arc = Arc::new(solana.address_bytes.clone());
    let axelarnet_gw_arc = Arc::new(transfer.axelarnet_gw_addr.clone());

    let mut tasks = Vec::with_capacity(num_keys);
    for w in wallets {
        let c = Arc::clone(&client);
        let its = Arc::clone(&stellar_its_arc);
        let gw = Arc::clone(&stellar_gw_arc);
        let xlm = Arc::clone(&stellar_xlm_arc);
        let dc = Arc::clone(&dest_chain_arc);
        let da = Arc::clone(&dest_addr_arc);
        let gmp_dest_addr = Arc::clone(&axelarnet_gw_arc);
        let metrics_clone = Arc::clone(&metrics_list);
        let counter = Arc::clone(&confirmed);
        let sp = spinner.clone();
        let total = num_keys;

        let handle = tokio::spawn(async move {
            let m = its_stellar_source::submit_transfer(TransferRequest {
                client: &c,
                wallet: &w,
                its_contract: &its,
                gateway_contract: &gw,
                token_id,
                destination_chain: &dc,
                destination_address_bytes: &da,
                gas_token: &xlm,
                gas_amount_stroops: gas_stroops,
                transfer_amount: amount_per_tx,
                gmp_dest_address: &gmp_dest_addr,
            })
            .await;
            if m.success {
                let done = counter.fetch_add(1, Ordering::Relaxed) + 1;
                sp.set_message(format!("sending ({done}/{total} confirmed)..."));
            }
            metrics_clone.lock().await.push(m);
        });
        tasks.push(handle);
    }

    let total_submitted = tasks.len() as u64;
    join_all(tasks).await;
    let test_duration = test_start.elapsed().as_secs_f64();
    let confirmed_count = confirmed.load(Ordering::Relaxed);
    spinner.finish_and_clear();
    ui::success(&format!(
        "sent {confirmed_count}/{total_submitted} confirmed"
    ));

    let metrics = metrics_list.lock().await.clone();
    let mut report = LoadTestReport::from_transactions(
        ReportInput {
            source_chain: src.to_string(),
            destination_chain: dest.to_string(),
            destination_address: solana.recipient.to_string(),
            num_txs: total_submitted,
            num_keys,
            total_submitted,
            test_duration_secs: test_duration,
            compute_unit_summary: ComputeUnitSummary::Omit,
        },
        metrics,
    );

    let verification = super::verify::verify_onchain_solana_its(
        &args.config,
        &args.source_axelar_id,
        &args.destination_axelar_id,
        &solana.recipient.to_string(),
        &args.destination_rpc,
        args.network,
        &mut report.transactions,
    )
    .await?;
    report.verification = Some(verification);

    finish_report(args, &mut report, test_start)
}

// ---------------------------------------------------------------------------
// Token setup
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn setup_its_token(
    client: &StellarClient,
    main_wallet: &StellarWallet,
    network: crate::types::Network,
    its_contract: &str,
    gateway_contract: &str,
    xlm_token: &str,
    gas_stroops: u64,
    src: &str,
    dest: &str,
    dest_axelar_id: &str,
    token_id_override: Option<&str>,
    config: &std::path::Path,
    solana_rpc_url: &str,
    num_txs: usize,
) -> Result<([u8; 32], [u8; 32], String, u32)> {
    if let Some(tid_hex) = token_id_override {
        let tid_bytes = hex::decode(tid_hex.strip_prefix("0x").unwrap_or(tid_hex))
            .map_err(|e| eyre!("invalid --token-id: {e}"))?;
        if tid_bytes.len() != 32 {
            return Err(eyre!("--token-id must be 32 bytes"));
        }
        let mut token_id = [0u8; 32];
        token_id.copy_from_slice(&tid_bytes);
        let token_addr = client
            .its_query_token_address(main_wallet, its_contract, token_id)
            .await?
            .ok_or_else(|| eyre!("token id {tid_hex} not registered on Stellar ITS"))?;
        let decimals = client
            .token_decimals(&main_wallet.public_key_bytes, &token_addr)
            .await?;
        ui::kv("token ID (provided)", tid_hex);
        return Ok((token_id, [0u8; 32], token_addr, decimals));
    }

    // chains-config pre-registered AXE: per-source override that lets CI skip
    // the full deploy + hub-routed remote-deploy and collapse to a single
    // interchainTransfer — but only when the configured wallet actually holds
    // enough AXE. A wallet with no balance falls through to the local cache /
    // fresh-deploy path. Assumes the AXE in config already has a Solana remote
    // registered (the fresh-deploy path below registers one); salt is unknown
    // for an adopted token, so the second return value is the zero salt.
    if let Some(tid) = super::helpers::read_pre_registered_axe_token(config, src)?
        && let Some(token_addr) = client
            .its_query_token_address(main_wallet, its_contract, tid.0)
            .await?
    {
        let decimals = client
            .token_decimals(&main_wallet.public_key_bytes, &token_addr)
            .await?;
        let needed =
            scale_to_decimals(WHOLE_TOKENS_PER_KEY, decimals).saturating_mul(num_txs as u128);
        let bal = client
            .token_balance(main_wallet, &token_addr, &main_wallet.public_key_bytes)
            .await
            .unwrap_or(0);
        if bal >= needed {
            ui::kv("token ID (chains-config)", &format!("{tid}"));
            ui::address("token contract (Stellar)", &token_addr);
            return Ok((tid.0, [0u8; 32], token_addr, decimals));
        }
        ui::warn(&format!(
            "chains-config AXE balance too low ({bal} < {needed}); configured wallet \
             isn't the workflow deployer — deploying fresh..."
        ));
    }

    let cache = read_its_cache(src, dest);
    if let Some(tid_hex) = cache.get("tokenId").and_then(|v| v.as_str())
        && let Some(salt_hex) = cache.get("salt").and_then(|v| v.as_str())
    {
        let tid_bytes = hex::decode(tid_hex.strip_prefix("0x").unwrap_or(tid_hex)).ok();
        let salt_bytes_v = hex::decode(salt_hex.strip_prefix("0x").unwrap_or(salt_hex)).ok();
        if let (Some(tid), Some(s)) = (tid_bytes, salt_bytes_v)
            && tid.len() == 32
            && s.len() == 32
        {
            let mut token_id = [0u8; 32];
            token_id.copy_from_slice(&tid);
            let mut salt = [0u8; 32];
            salt.copy_from_slice(&s);
            // Verify token still exists + deployer has enough supply.
            if let Ok(Some(token_addr)) = client
                .its_query_token_address(main_wallet, its_contract, token_id)
                .await
            {
                let decimals = client
                    .token_decimals(&main_wallet.public_key_bytes, &token_addr)
                    .await?;
                let needed = scale_to_decimals(WHOLE_TOKENS_PER_KEY, decimals)
                    .saturating_mul(num_txs as u128);
                let bal = client
                    .token_balance(main_wallet, &token_addr, &main_wallet.public_key_bytes)
                    .await
                    .unwrap_or(0);
                if bal >= needed {
                    ui::info(&format!("reusing cached ITS token: {token_addr}"));
                    return Ok((token_id, salt, token_addr, decimals));
                }
                ui::warn(&format!(
                    "cached AXE token has insufficient supply ({bal} < {needed}), deploying fresh..."
                ));
            } else {
                ui::warn(
                    "cached AXE token no longer registered on Stellar ITS, deploying fresh...",
                );
            }
        }
    }

    // Deploy fresh.
    let mut salt = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut salt);

    ui::info("deploying new ITS token on Stellar...");
    ui::kv("name", TOKEN_NAME);
    ui::kv("symbol", TOKEN_SYMBOL);
    ui::kv("decimals", &TOKEN_DECIMALS.to_string());
    ui::kv("supply", &INITIAL_SUPPLY.to_string());

    let (deploy_invoked, token_id_opt) = client
        .its_deploy_interchain_token(
            main_wallet,
            its_contract,
            salt,
            TOKEN_DECIMALS,
            TOKEN_NAME,
            TOKEN_SYMBOL,
            INITIAL_SUPPLY,
        )
        .await?;
    if !deploy_invoked.success {
        return Err(eyre!("Stellar deploy_interchain_token failed"));
    }
    let token_id =
        token_id_opt.ok_or_else(|| eyre!("deploy_interchain_token returned no token_id"))?;
    ui::tx_hash("Stellar deploy", &deploy_invoked.tx_hash_hex);
    ui::kv("token ID", &hex::encode(token_id));

    let token_address = client
        .its_query_token_address(main_wallet, its_contract, token_id)
        .await?
        .ok_or_else(|| eyre!("could not resolve interchain_token_address after deploy"))?;
    ui::address("token contract", &token_address);

    // Register on Solana destination via ITS hub.
    ui::info(&format!("deploying remote AXE token to {dest}..."));
    let remote_invoked = client
        .its_deploy_remote_interchain_token(
            main_wallet,
            its_contract,
            gateway_contract,
            salt,
            dest_axelar_id,
            xlm_token,
            gas_stroops,
        )
        .await?;
    if !remote_invoked.success {
        return Err(eyre!("Stellar deploy_remote_interchain_token failed"));
    }
    ui::tx_hash("Stellar remote-deploy", &remote_invoked.tx_hash_hex);
    let event_index = remote_invoked.event_index.unwrap_or(0);
    let deploy_message_id = format!(
        "0x{}-{event_index}",
        remote_invoked.tx_hash_hex.to_lowercase()
    );

    // Wait for it to land on Solana.
    super::verify::wait_for_its_remote_deploy_to_solana(
        config,
        &super::axelar_id_for_chain(config, src)?,
        dest_axelar_id,
        &deploy_message_id,
        solana_rpc_url,
        network,
    )
    .await?;

    // Cache.
    let mut cache = cache;
    cache["tokenId"] = serde_json::json!(format!("0x{}", hex::encode(token_id)));
    cache["salt"] = serde_json::json!(format!("0x{}", hex::encode(salt)));
    cache["tokenAddress"] = serde_json::json!(token_address);
    save_its_cache(src, dest, &cache)?;

    Ok((token_id, salt, token_address, TOKEN_DECIMALS))
}

// ---------------------------------------------------------------------------
// Distribution
// ---------------------------------------------------------------------------

async fn distribute_token_balances(
    client: &StellarClient,
    main_wallet: &StellarWallet,
    token_contract: &str,
    wallets: &[StellarWallet],
    amount_per_key: u128,
) -> Result<()> {
    // First, see who needs topping up (skip wallets that already have enough).
    let pb_check = indicatif::ProgressBar::new(wallets.len() as u64);
    pb_check.set_style(
        indicatif::ProgressStyle::with_template("  {bar:40.cyan/dim} {pos}/{len} balances checked")
            .unwrap()
            .progress_chars("=> "),
    );
    let mut to_fund: Vec<usize> = Vec::new();
    for (i, w) in wallets.iter().enumerate() {
        let bal = client
            .token_balance(main_wallet, token_contract, &w.public_key_bytes)
            .await
            .unwrap_or(0);
        if bal < amount_per_key {
            to_fund.push(i);
        }
        pb_check.inc(1);
    }
    pb_check.finish_and_clear();

    if to_fund.is_empty() {
        ui::success(&format!(
            "all {} ephemeral wallets already hold ≥ {amount_per_key} AXE",
            wallets.len()
        ));
        return Ok(());
    }

    ui::info(&format!(
        "distributing AXE to {}/{} keys...",
        to_fund.len(),
        wallets.len()
    ));
    let pb = indicatif::ProgressBar::new(to_fund.len() as u64);
    pb.set_style(
        indicatif::ProgressStyle::with_template("  {bar:40.cyan/dim} {pos}/{len} keys funded")
            .unwrap()
            .progress_chars("=> "),
    );
    for &i in &to_fund {
        let invoked = client
            .token_transfer(
                main_wallet,
                token_contract,
                &wallets[i].public_key_bytes,
                amount_per_key,
            )
            .await?;
        if !invoked.success {
            return Err(eyre!(
                "AXE transfer failed for key {i} (tx {})",
                invoked.tx_hash_hex
            ));
        }
        pb.inc(1);
    }
    pb.finish_and_clear();
    ui::success(&format!(
        "distributed AXE to {} ephemeral keys",
        to_fund.len()
    ));
    Ok(())
}
