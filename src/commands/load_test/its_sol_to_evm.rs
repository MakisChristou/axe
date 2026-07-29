use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use alloy::primitives::Address;
use alloy::signers::local::PrivateKeySigner;
use eyre::eyre;
use rand::Rng;
use solana_client::rpc_client::RpcClient;
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::message::Message;
use solana_sdk::signature::Keypair;
use solana_sdk::signer::Signer;
use solana_sdk::transaction::Transaction;

use super::LoadTestArgs;
use super::its_prerequisites::{self, GatewayRequirement};
use super::its_sol_source;
use super::its_verification;
use super::its_verification::{EvmItsTarget, ItsBurstReport, finish_burst};
use super::keypairs;
use super::metrics::ComputeUnitSummary;
use super::run_sizing::{RunSizing, SustainedPlan};
use super::verification_session::VerificationSession;
use super::verify::VerificationRoute;
use super::{read_its_cache, save_its_cache, validate_evm_rpc, validate_solana_rpc};
use crate::config::ChainsConfig;
use crate::solana;
use crate::ui;

// Token spec lives in `crate::types::LOAD_TEST_SOL_SPEC`.
/// Whole tokens transferred per tx. Scaled by the mint's on-chain decimals at
/// runtime (see `mint_decimals`) so both fresh-deploy (9 decimals) and the
/// reused canonical AXE (6 decimals) get correct amounts.
const WHOLE_TOKENS_PER_TX: u64 = 1;
/// Distribute 100x per key so cached tokens last across many runs.
const WHOLE_TOKENS_PER_KEY: u64 = WHOLE_TOKENS_PER_TX * 100;

/// Read the SPL mint's `decimals` from chain, defaulting to 9 (the fresh
/// load-test token's decimals) on error to preserve the original behavior.
fn mint_decimals(rpc_client: &RpcClient, mint: &solana_sdk::pubkey::Pubkey) -> u8 {
    rpc_client
        .get_token_supply(mint)
        .map(|supply| supply.decimals)
        .unwrap_or(9)
}

/// Default gas value (per command) for an ITS *transfer* on Solana (in lamports).
/// devnet-amplifier doesn't require gas, stagenet/mainnet do.
///
/// 500k lamports (~0.0005 SOL) covers the destination-side
/// `execute → _giveToken → ERC20.transfer` on a typical EVM relayer quote.
/// Earlier 100k was a hair too low and the public testnet relayer reverted
/// with `availableGasBalance.amount must be positive: -2449`. For very-high-
/// throughput burst tests where the per-tx cost matters, override with
/// `--gas-value`.
fn default_gas_value(network: crate::types::Network) -> u64 {
    match network {
        crate::types::Network::DevnetAmplifier => 0,
        _ => 500_000,
    }
}

/// ITS routes via the hub, so two commands are created (source→hub and
/// hub→destination). The gas payment must cover both legs.
fn hub_gas_value(per_command: u64) -> u64 {
    per_command.saturating_mul(2)
}

/// Multiplier applied to the per-tx gas value for the one-time `deployRemote`
/// call. Destination-side deploys are dramatically more expensive than
/// transfers (they CREATE2 a fresh ITS token contract on the EVM side), and
/// the public testnet relayer was reverting with `availableGasBalance.amount
/// must be positive: -2449` when we paid the same 100k lamports for both.
/// 10× ≈ 0.001 SOL covers the deploy with margin.
const DEPLOY_GAS_MULTIPLIER: u64 = 10;

pub async fn run(args: LoadTestArgs, _run_start: Instant) -> eyre::Result<()> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;

    let evm_rpc_url = args.destination_rpc.clone();

    // Validate RPCs
    validate_solana_rpc(&args.source_rpc).await?;
    validate_evm_rpc(&evm_rpc_url).await?;

    let cfg = ChainsConfig::load(&args.config)?;
    its_prerequisites::verify(&cfg, dest, GatewayRequirement::AmplifierOnly)?;

    ui::kv("source", src);
    ui::kv("destination", dest);
    ui::kv("protocol", "ITS (interchainTransfer via hub)");

    let (rpc_client, main_keypair) = init_solana_client_and_main_keypair(
        &args.source_rpc,
        args.keypair.as_deref(),
        args.network,
    )?;

    let gas_value = parse_gas_value(args.gas_value.as_deref(), args.network)?;
    let (evm, dest_address_bytes) =
        resolve_evm_targets_and_receiver(&cfg, dest, args.private_key.as_deref())?;

    let sizing = RunSizing::new(&args)?;

    let (token_id, _salt, mint) = setup_its_token(TokenSetupRequest {
        solana_rpc: &args.source_rpc,
        keypair: &main_keypair,
        network: args.network,
        source_chain: src,
        destination_chain: dest,
        num_txs: sizing.num_keys,
        gas_value,
        token_id_override: args.token_id.as_deref(),
        config: &args.config,
        evm_gateway: evm.evm_gateway_addr,
        evm_rpc_url: &evm_rpc_url,
        rpc_client: &rpc_client,
    })
    .await?;

    ui::kv("token ID", &hex::encode(token_id));
    ui::address("mint", &mint.to_string());

    // Scale the whole-token counts by the mint's actual decimals so reused
    // canonical tokens (6 decimals) and freshly deployed ones (9 decimals)
    // both transfer the right amounts.
    let decimals = mint_decimals(&rpc_client, &mint);
    // /100 → 0.01 whole tokens per tx so the cron's source-side supply lasts.
    let amount_per_tx = WHOLE_TOKENS_PER_TX * 10u64.pow(u32::from(decimals)) / 100;
    let amount_per_key = WHOLE_TOKENS_PER_KEY * 10u64.pow(u32::from(decimals)) / 100;

    // --- Derive and fund keypairs ---
    let keypairs = prepare_keypairs(&args.source_rpc, sizing.num_keys, &main_keypair)?;

    // --- Create ATAs and distribute tokens ---
    distribute_its_tokens(
        &args.source_rpc,
        &main_keypair,
        &keypairs,
        &mint,
        &token_id,
        compute_distribution_amount(sizing, amount_per_tx, amount_per_key),
    )?;

    let transfer = ItsTransferSpec {
        token_id,
        mint,
        gas_value,
        dest_address_bytes,
        amount_per_tx,
    };

    if !sizing.is_burst() {
        run_sustained_pipeline(&args, &evm, &sizing, keypairs, &transfer).await
    } else {
        run_burst_pipeline(&args, &evm, &keypairs, &transfer, &evm_rpc_url).await
    }
}

/// EVM-side addresses resolved from config for the destination chain, plus
/// the global AxelarnetGateway used as the GMP-hub destination.
struct EvmTargets {
    its_proxy_addr: Address,
    evm_gateway_addr: Address,
    axelarnet_gw_addr: String,
}

/// Per-transfer payload bits that are common to burst and sustained modes:
/// the deployed ITS token, its mint, the gas value, and the EVM receiver
/// (already encoded as bytes).
struct ItsTransferSpec {
    token_id: [u8; 32],
    mint: solana_sdk::pubkey::Pubkey,
    gas_value: u64,
    dest_address_bytes: Vec<u8>,
    /// Per-transfer amount, already scaled to the mint's on-chain decimals.
    amount_per_tx: u64,
}

/// Build the Solana RPC client, load the main funding keypair, and log the
/// wallet's address and current SOL balance. Bails with the existing
/// fund-wallet hint if the balance is zero.
fn init_solana_client_and_main_keypair(
    solana_rpc: &str,
    keypair_path: Option<&str>,
    network: crate::types::Network,
) -> eyre::Result<(RpcClient, Keypair)> {
    let main_keypair = solana::load_keypair(keypair_path)?;
    let rpc_client = RpcClient::new_with_commitment(
        solana_rpc,
        solana_commitment_config::CommitmentConfig::finalized(),
    );
    let pubkey = main_keypair.pubkey();
    let balance = rpc_client.get_balance(&pubkey).unwrap_or(0);
    let sol = balance as f64 / 1e9;
    ui::kv("wallet", &format!("{pubkey} ({sol:.4} SOL)"));
    if balance == 0 {
        return Err(eyre!(
            "wallet ({pubkey}) has no SOL. {}",
            match network {
                crate::types::Network::Mainnet =>
                    format!("Fund {pubkey} with mainnet SOL (no faucet) before retrying."),
                _ => format!("Fund it first:\n  solana airdrop 2 {pubkey}"),
            }
        ));
    }
    Ok((rpc_client, main_keypair))
}

/// Parse the user-supplied gas value (lamports), defaulting to
/// `default_gas_value()`, and emit the matching UI line.
fn parse_gas_value(gas_value: Option<&str>, network: crate::types::Network) -> eyre::Result<u64> {
    let gas_value: u64 = match gas_value {
        Some(v) => v.parse().map_err(|e| eyre!("invalid --gas-value: {e}"))?,
        None => default_gas_value(network),
    };
    ui::kv("gas value", &format!("{gas_value} lamports"));
    Ok(gas_value)
}

/// Resolve the EVM-destination addresses (ITS proxy, gateway, axelarnet
/// gateway) and derive the EVM-side receiver wallet, emitting UI lines in
/// the original order: destination ITS, receiver, EVM gateway. The interleave
/// matters — original error paths log destination ITS and receiver before
/// the gateway parse can fail.
fn resolve_evm_targets_and_receiver(
    cfg: &ChainsConfig,
    dest: &str,
    private_key: Option<&str>,
) -> eyre::Result<(EvmTargets, Vec<u8>)> {
    // --- EVM destination ITS proxy (used by the relayer to dispatch execute) ---
    let dest_cfg = cfg
        .chains
        .get(dest)
        .ok_or_else(|| eyre!("destination chain '{dest}' not found in config"))?;
    let its_proxy_addr: Address = dest_cfg
        .contract_address("InterchainTokenService", dest)?
        .parse()?;
    ui::address("destination ITS", &format!("{its_proxy_addr}"));

    // --- Receiver wallet for the InterchainTransfer ---
    // Must be an EOA on the destination chain — passing the ITS proxy here
    // reverts EVM estimation because ITS won't transfer to its own address.
    // Prefer the EVM_PRIVATE_KEY's derived address so test runs accumulate
    // tokens at a wallet the user owns (no dust burn). Fall back to the
    // canonical dEaD burn address when no key is configured — verify only
    // checks gateway approval/execution, not the receiver's balance.
    let receiver: Address = match private_key {
        Some(pk) => {
            let signer: PrivateKeySigner = pk
                .parse()
                .map_err(|e| eyre!("invalid EVM private key for receiver derivation: {e}"))?;
            signer.address()
        }
        None => crate::types::DEAD_ADDRESS,
    };
    ui::address("receiver", &format!("{receiver}"));
    let dest_address_bytes = receiver.as_slice().to_vec();

    // --- EVM gateway for verification ---
    let evm_gateway_addr: Address = dest_cfg.contract_address("AxelarGateway", dest)?.parse()?;
    ui::address("EVM gateway", &format!("{evm_gateway_addr}"));

    // --- ITS hub routing info ---
    // ITS always routes through "axelar" hub. The GMP destination is the AxelarnetGateway.
    let axelarnet_gw_addr = cfg
        .axelar
        .global_contract_address("AxelarnetGateway")?
        .to_string();

    Ok((
        EvmTargets {
            its_proxy_addr,
            evm_gateway_addr,
            axelarnet_gw_addr,
        },
        dest_address_bytes,
    ))
}

/// Per-key token amount to seed the ephemeral wallets with: `amount_per_key`
/// for burst mode, otherwise enough headroom for the sustained per-key cycle.
/// Both `amount_per_tx` and `amount_per_key` are already scaled to the mint's
/// on-chain decimals.
fn compute_distribution_amount(sizing: RunSizing, amount_per_tx: u64, amount_per_key: u64) -> u64 {
    if sizing.is_burst() {
        amount_per_key
    } else {
        amount_per_tx * sizing.transactions_per_key() * 2
    }
}

/// Drive the sustained-mode pipeline: spawn the streaming verifier, run the
/// Solana sustained loop, stitch amplifier timings back into the report, and
/// hand off to `finish_report`.
async fn run_sustained_pipeline(
    args: &LoadTestArgs,
    evm: &EvmTargets,
    sizing: &RunSizing,
    keypairs: Vec<Arc<Keypair>>,
    transfer: &ItsTransferSpec,
) -> eyre::Result<()> {
    let dest = &args.destination_chain;
    let evm_rpc_url = args.destination_rpc.clone();
    let SustainedPlan {
        tps: tps_n,
        duration_secs,
        key_cycle,
    } = sizing.sustained().expect("sustained mode");

    let mut verification = VerificationSession::start(
        VerificationRoute::from_args(args),
        EvmItsTarget {
            gateway_addr: evm.evm_gateway_addr,
            rpc_url: evm_rpc_url,
        },
    );

    let spinner = ui::wait_spinner(&format!(
        "[0/{duration_secs}s] starting sustained ITS send..."
    ));
    verification.attach_spinner(spinner.clone())?;

    let test_start = Instant::now();

    let jobs = keypairs
        .iter()
        .map(|keypair| its_sol_source::ItsSolanaSubmitJob {
            keypair: Arc::clone(keypair),
            source_account: its_sol_source::source_account(keypair, &transfer.mint),
        })
        .collect();
    let make_task = its_sol_source::its_sustained_tasks(
        its_sol_source::ItsSolanaSubmitter {
            rpc_url: args.source_rpc.clone(),
            network: args.network,
            token_id: transfer.token_id.into(),
            mint: transfer.mint,
            destination_chain: dest.to_string(),
            destination_address: transfer.dest_address_bytes.clone(),
            amount: transfer.amount_per_tx,
            gas_value: super::units::Lamports::new(hub_gas_value(transfer.gas_value)),
            metric_context: its_sol_source::MetricContext::HubRouted {
                hub_address: evm.axelarnet_gw_addr.clone(),
            },
        },
        jobs,
        Some(verification.sender()),
    );

    let result = super::sustained::run_sustained_loop(
        SustainedPlan {
            tps: tps_n,
            duration_secs,
            key_cycle,
        },
        None,
        make_task,
        Some(verification.send_done()),
        spinner,
    )
    .await?;
    its_verification::finish_sustained(
        verification,
        args,
        result,
        &format!("{}", evm.its_proxy_addr),
        sizing.total_expected,
        sizing.num_keys,
        test_start,
    )
    .await
}

/// Drive the burst-mode pipeline: fan out the Solana ITS transfers, batch-
/// verify on the EVM destination, and hand off to `finish_report`.
async fn run_burst_pipeline(
    args: &LoadTestArgs,
    evm: &EvmTargets,
    keypairs: &[Arc<Keypair>],
    transfer: &ItsTransferSpec,
    evm_rpc_url: &str,
) -> eyre::Result<()> {
    let dest = &args.destination_chain;
    let key_count = keypairs.len();
    let test_start = Instant::now();
    let jobs = keypairs
        .iter()
        .map(|keypair| its_sol_source::ItsSolanaSubmitJob {
            keypair: Arc::clone(keypair),
            source_account: its_sol_source::source_account(keypair, &transfer.mint),
        })
        .collect();
    let burst = its_sol_source::run_its_burst(
        its_sol_source::ItsSolanaSubmitter {
            rpc_url: args.source_rpc.clone(),
            network: args.network,
            token_id: transfer.token_id.into(),
            mint: transfer.mint,
            destination_chain: dest.to_string(),
            destination_address: transfer.dest_address_bytes.clone(),
            amount: transfer.amount_per_tx,
            gas_value: super::units::Lamports::new(transfer.gas_value),
            metric_context: its_sol_source::MetricContext::HubRouted {
                hub_address: evm.axelarnet_gw_addr.clone(),
            },
        },
        jobs,
        100,
    )
    .await?;
    let total_failed = burst.metrics.iter().filter(|m| !m.is_success()).count() as u64;

    if total_failed > 0 {
        let mut error_counts: std::collections::HashMap<String, (u64, String)> =
            std::collections::HashMap::new();
        for m in burst.metrics.iter().filter(|m| !m.is_success()) {
            // Group by a short key (deduplicates identical failures) but
            // print the full error message — Solana program-log dumps are
            // multi-line and a 120-char cap drops the actionable part.
            let full = m.error().unwrap_or("unknown").to_string();
            let key: String = full.chars().take(80).collect();
            error_counts.entry(key).or_insert((0u64, full)).0 += 1;
        }
        for (count, full) in error_counts.values() {
            ui::warn(&format!("{count} txs failed:\n{full}"));
        }
    }

    finish_burst(
        args,
        EvmItsTarget {
            gateway_addr: evm.evm_gateway_addr,
            rpc_url: evm_rpc_url.to_string(),
        },
        burst,
        ItsBurstReport {
            destination_address: format!("{}", evm.its_proxy_addr),
            num_txs: args.num_txs,
            num_keys: key_count,
            compute_unit_summary: ComputeUnitSummary::Include,
        },
        test_start,
    )
    .await
}

// ---------------------------------------------------------------------------
// Token setup
// ---------------------------------------------------------------------------

/// SPL (Token-2022) balance held by `owner`'s associated token account for
/// `mint`. Returns 0 when the ATA is absent or unreadable. The amount lives at
/// bytes 64..72 of the SPL token-account layout.
fn deployer_spl_balance(
    rpc_client: &solana_client::rpc_client::RpcClient,
    owner: &solana_sdk::pubkey::Pubkey,
    mint: &solana_sdk::pubkey::Pubkey,
) -> u64 {
    let token_program =
        solana_sdk::pubkey::Pubkey::from_str_const("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
    let ata = solana_sdk::pubkey::Pubkey::find_program_address(
        &[owner.as_ref(), token_program.as_ref(), mint.as_ref()],
        &solana_sdk::pubkey::Pubkey::from_str_const("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL"),
    )
    .0;
    rpc_client
        .get_account_data(&ata)
        .ok()
        .filter(|data| data.len() >= 72)
        .map(|data| u64::from_le_bytes(data[64..72].try_into().unwrap_or([0; 8])))
        .unwrap_or(0)
}

/// Deploy or reuse ITS token. Returns (token_id, salt, mint).
/// When deploying fresh, waits for the remote deploy to propagate through the
/// ITS hub and execute on the EVM destination before returning.
struct TokenSetupRequest<'a> {
    solana_rpc: &'a str,
    keypair: &'a Keypair,
    network: crate::types::Network,
    source_chain: &'a str,
    destination_chain: &'a str,
    num_txs: usize,
    gas_value: u64,
    token_id_override: Option<&'a str>,
    config: &'a Path,
    evm_gateway: Address,
    evm_rpc_url: &'a str,
    rpc_client: &'a solana_client::rpc_client::RpcClient,
}

async fn setup_its_token(
    request: TokenSetupRequest<'_>,
) -> eyre::Result<([u8; 32], [u8; 32], solana_sdk::pubkey::Pubkey)> {
    let TokenSetupRequest {
        solana_rpc,
        keypair,
        network,
        source_chain: src,
        destination_chain: dest,
        num_txs,
        gas_value,
        token_id_override,
        config,
        evm_gateway: evm_gateway_addr,
        evm_rpc_url,
        rpc_client,
    } = request;
    if let Some(tid_hex) = token_id_override {
        let tid_bytes = hex::decode(tid_hex.strip_prefix("0x").unwrap_or(tid_hex))
            .map_err(|e| eyre!("invalid --token-id: {e}"))?;
        if tid_bytes.len() != 32 {
            return Err(eyre!("--token-id must be 32 bytes"));
        }
        let mut token_id = [0u8; 32];
        token_id.copy_from_slice(&tid_bytes);
        let (its_root, _) = solana::find_its_root_pda(network);
        let (mint, _) = solana::find_interchain_token_pda(network, &its_root, &token_id);
        ui::kv("token ID (provided)", tid_hex);
        return Ok((token_id, [0u8; 32], mint));
    }

    // chains-config pre-registered AXE: reuse the canonical token when the
    // configured wallet actually holds enough of it on Solana; otherwise fall
    // through to the cache / fresh-deploy path (matches the EVM/Stellar
    // resolvers). Salt is unknown for an adopted token, so return the zero salt.
    if let Some(tid) = super::helpers::read_pre_registered_axe_token(config, src)? {
        let (its_root, _) = solana::find_its_root_pda(network);
        let (mint, _) = solana::find_interchain_token_pda(network, &its_root, &tid.0);
        if rpc_client.get_account_data(&mint).is_ok() {
            let decimals = mint_decimals(rpc_client, &mint);
            let needed = WHOLE_TOKENS_PER_KEY
                .saturating_mul(10u64.pow(u32::from(decimals)))
                .saturating_mul(num_txs as u64);
            let balance = deployer_spl_balance(rpc_client, &keypair.pubkey(), &mint);
            if balance >= needed {
                ui::kv("token ID (chains-config)", &format!("{tid}"));
                ui::address("mint", &mint.to_string());
                return Ok((tid.0, [0u8; 32], mint));
            }
            ui::warn(&format!(
                "chains-config AXE balance too low ({balance} < {needed}); configured wallet \
                 isn't the workflow deployer — deploying fresh..."
            ));
        }
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
            let (its_root, _) = solana::find_its_root_pda(network);
            let (mint, _) = solana::find_interchain_token_pda(network, &its_root, &token_id);

            // Verify token still exists on-chain and deployer has enough supply
            if rpc_client.get_account_data(&mint).is_ok() {
                let decimals = mint_decimals(rpc_client, &mint);
                let needed = WHOLE_TOKENS_PER_KEY
                    .saturating_mul(10u64.pow(u32::from(decimals)))
                    .saturating_mul(num_txs as u64);
                let deployer_balance = deployer_spl_balance(rpc_client, &keypair.pubkey(), &mint);

                if deployer_balance >= needed {
                    ui::info(&format!("reusing cached ITS token: {mint}"));
                    return Ok((token_id, salt, mint));
                }
                ui::warn(&format!(
                    "cached token has insufficient supply ({deployer_balance} < {needed}), deploying fresh..."
                ));
            } else {
                ui::warn("cached token no longer exists, deploying fresh...");
            }
        }
    }

    // Deploy fresh
    let salt = generate_salt();
    // Mint a large fixed supply so the token can be reused across runs without redeploying.
    let total_supply: u64 = 1_000_000 * 1_000_000_000; // 1M tokens (9 decimals)
    let spec = crate::types::LOAD_TEST_SOL_SPEC;

    ui::info("deploying new ITS token on Solana...");
    ui::kv("name", spec.name);
    ui::kv("symbol", spec.symbol);
    ui::kv("decimals", &spec.decimals.to_string());
    ui::kv("supply", &total_supply.to_string());

    let minter = keypair.pubkey();
    let deploy_sig =
        solana::send_its_deploy_interchain_token(solana::DeployInterchainTokenRequest {
            rpc_url: solana_rpc,
            keypair,
            network,
            salt: &salt,
            name: spec.name,
            symbol: spec.symbol,
            decimals: spec.decimals,
            initial_supply: total_supply,
            minter: Some(&minter),
        })?;
    ui::tx_hash("deploy tx", &deploy_sig);

    let token_id = solana::interchain_token_id(network, &keypair.pubkey(), &salt);
    let (its_root, _) = solana::find_its_root_pda(network);
    let (mint, _) = solana::find_interchain_token_pda(network, &its_root, &token_id);

    ui::kv("token ID", &hex::encode(token_id));
    ui::address("mint", &mint.to_string());

    // Deploy remote to EVM destination. Deploys consume ~10× the
    // destination-side gas of a transfer because they CREATE2 a fresh ITS
    // token contract on the EVM chain, so multiply the per-tx gas budget.
    let deploy_gas_value = gas_value.saturating_mul(DEPLOY_GAS_MULTIPLIER);
    ui::info(&format!(
        "deploying remote token to {dest} (gas: {deploy_gas_value} lamports)..."
    ));
    // Pass `deploy_gas_value` (= gas_value × DEPLOY_GAS_MULTIPLIER) directly,
    // without an additional `hub_gas_value` doubling. The 10× multiplier was
    // calibrated for the post-hub world the relayer presents — it already
    // covers the destination-side CREATE2 deploy, which dominates the cost.
    // Doubling on top would overpay by ~10×.
    let remote_sig = solana::send_its_deploy_remote_interchain_token(
        solana_rpc,
        keypair,
        network,
        &salt,
        dest,
        deploy_gas_value,
    )?;
    ui::tx_hash("remote deploy tx", &remote_sig);
    ui::success("remote deploy tx confirmed on Solana");

    // Wait for the remote deploy to propagate through the hub and execute on EVM.
    // The deploy message ID is {signature}-{top_ix}.{inner_ix} where the inner
    // index varies by program version. We MUST extract it from the tx logs —
    // a wrong fallback ID would silently send the verifier into a 5-minute
    // pipeline timeout waiting for a message that does not exist.
    let deploy_message_id = solana::extract_its_message_id(solana_rpc, network, &remote_sig)
        .map_err(|e| {
            eyre!(
                "could not extract remote-deploy message ID from tx logs: {e}\n\
                 Tip: the public Solana devnet RPC is rate-limited and slow to index. \
                 Pass --source-rpc <faster-rpc-url> (e.g. a QuickNode/Helius endpoint) \
                 to fix this."
            )
        })?;
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
    let rpc_client = solana_client::rpc_client::RpcClient::new_with_commitment(
        solana_rpc,
        solana_commitment_config::CommitmentConfig::finalized(),
    );

    let token_program =
        solana_sdk::pubkey::Pubkey::from_str_const("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
    let ata_program =
        solana_sdk::pubkey::Pubkey::from_str_const("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");

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
                    solana_sdk::pubkey::Pubkey::from_str_const("11111111111111111111111111111111"),
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
    ui::success(&format!("distributed tokens to {} keys", keypairs.len()));
    Ok(())
}
