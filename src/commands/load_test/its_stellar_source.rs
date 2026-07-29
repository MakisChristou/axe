//! Shared source-side sending for Stellar ITS routes.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use eyre::{Result, eyre};
use rand::RngCore;

use super::identifiers::TokenId;
use super::metrics::{TxMetrics, TxOutcome};
use super::run_sizing::{RunSizing, SustainedPlan};
use super::submitter::TransactionSubmitter;
use super::sustained;
use super::units::Stroops;
use crate::stellar::{StellarClient, StellarWallet};
use crate::ui;

const DEFAULT_GAS_STROOPS: u64 = 100_000_000;
const TOKEN_NAME: &str = "AXE";
const TOKEN_SYMBOL: &str = "AXE";
const TOKEN_DECIMALS: u32 = 7;
const WHOLE_TOKENS_PER_TX: u128 = 1;
const WHOLE_TOKENS_PER_KEY: u128 = WHOLE_TOKENS_PER_TX * 100;
const INITIAL_SUPPLY: u128 = 1_000_000 * 10_000_000;

pub(super) fn scale_to_decimals(whole_tokens: u128, decimals: u32) -> u128 {
    whole_tokens * 10u128.pow(decimals)
}

pub(super) fn parse_gas_stroops(gas_value: Option<&str>) -> Result<u64> {
    let gas_stroops = match gas_value {
        Some(value) => value
            .parse::<u64>()
            .map_err(|error| eyre!("invalid --gas-value: {error}"))?,
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

pub(super) fn transfer_amount(decimals: u32) -> u128 {
    scale_to_decimals(WHOLE_TOKENS_PER_TX, decimals) / 100
}

pub(super) fn amount_per_key(sizing: &RunSizing, decimals: u32) -> u128 {
    if sizing.is_burst() {
        scale_to_decimals(WHOLE_TOKENS_PER_KEY, decimals) / 100
    } else {
        let txs_per_key = sizing.transactions_per_key() as u128;
        transfer_amount(decimals)
            .saturating_mul(txs_per_key)
            .saturating_mul(2)
    }
}

pub(super) async fn derive_and_fund_wallets(
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

pub(super) trait RemoteDeploymentVerifier {
    async fn wait_for_remote_deploy(
        &self,
        config: &std::path::Path,
        source_axelar_id: &str,
        destination_axelar_id: &str,
        message_id: &str,
    ) -> Result<()>;

    fn after_remote_deploy(&self, _source_chain: &str, _token_id: [u8; 32]) {}
}

pub(super) struct TokenSetupRequest {
    pub its_contract: String,
    pub gateway_contract: String,
    pub gas_token: String,
    pub gas_stroops: Stroops,
    pub source_chain: String,
    pub destination_chain: String,
    pub destination_axelar_id: String,
    pub token_id_override: Option<String>,
    pub config: std::path::PathBuf,
    pub required_transfers: usize,
}

pub(super) struct TokenSetup {
    pub token_id: [u8; 32],
    pub token_address: String,
    pub decimals: u32,
}

pub(super) async fn setup_token<V>(
    client: &StellarClient,
    main_wallet: &StellarWallet,
    remote_verifier: &V,
    request: TokenSetupRequest,
) -> Result<TokenSetup>
where
    V: RemoteDeploymentVerifier + Sync,
{
    let TokenSetupRequest {
        its_contract,
        gateway_contract,
        gas_token,
        gas_stroops,
        source_chain,
        destination_chain,
        destination_axelar_id,
        token_id_override,
        config,
        required_transfers,
    } = request;
    let its_contract = its_contract.as_str();
    let gateway_contract = gateway_contract.as_str();
    let gas_token = gas_token.as_str();
    let source_chain = source_chain.as_str();
    let destination_chain = destination_chain.as_str();
    let destination_axelar_id = destination_axelar_id.as_str();
    let token_id_override = token_id_override.as_deref();
    let config = config.as_path();

    if let Some(token_id_hex) = token_id_override {
        let token_id = match parse_token_id(token_id_hex) {
            Ok(token_id) => token_id,
            Err(TokenIdParseError::Hex(error)) => {
                return Err(eyre!("invalid --token-id: {error}"));
            }
            Err(TokenIdParseError::Length) => {
                return Err(eyre!("--token-id must be 32 bytes"));
            }
        };
        let token_address = client
            .its_query_token_address(main_wallet, its_contract, token_id)
            .await?
            .ok_or_else(|| eyre!("token id {token_id_hex} not registered on Stellar ITS"))?;
        let decimals = client
            .token_decimals(&main_wallet.public_key_bytes, &token_address)
            .await?;
        ui::kv("token ID (provided)", token_id_hex);
        return Ok(TokenSetup {
            token_id,
            token_address,
            decimals,
        });
    }

    if let Some(token_id) = super::helpers::read_pre_registered_axe_token(config, source_chain)?
        && let Some(token_address) = client
            .its_query_token_address(main_wallet, its_contract, token_id.0)
            .await?
    {
        let decimals = client
            .token_decimals(&main_wallet.public_key_bytes, &token_address)
            .await?;
        let needed = required_balance(decimals, required_transfers);
        let balance = client
            .token_balance(main_wallet, &token_address, &main_wallet.public_key_bytes)
            .await
            .unwrap_or(0);
        if balance >= needed {
            ui::kv("token ID (chains-config)", &format!("{token_id}"));
            ui::address("token contract (Stellar)", &token_address);
            return Ok(TokenSetup {
                token_id: token_id.0,
                token_address,
                decimals,
            });
        }
        ui::warn(&format!(
            "chains-config AXE balance too low ({balance} < {needed}); configured wallet \
             isn't the workflow deployer — deploying fresh..."
        ));
    }

    let cache = super::read_its_cache(source_chain, destination_chain);
    if let Some(cached) = reusable_cached_token(
        client,
        main_wallet,
        its_contract,
        &cache,
        required_transfers,
    )
    .await?
    {
        return Ok(cached);
    }

    let mut salt = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut salt);
    ui::info("deploying new ITS token on Stellar...");
    ui::kv("name", TOKEN_NAME);
    ui::kv("symbol", TOKEN_SYMBOL);
    ui::kv("decimals", &TOKEN_DECIMALS.to_string());
    ui::kv("supply", &INITIAL_SUPPLY.to_string());

    let (deploy_invoked, token_id) = client
        .its_deploy_interchain_token(crate::stellar::DeployInterchainTokenRequest {
            wallet: main_wallet,
            its_contract,
            salt,
            decimals: TOKEN_DECIMALS,
            name: TOKEN_NAME,
            symbol: TOKEN_SYMBOL,
            initial_supply: INITIAL_SUPPLY,
        })
        .await?;
    if !deploy_invoked.success {
        return Err(eyre!("Stellar deploy_interchain_token failed"));
    }
    let token_id = token_id.ok_or_else(|| eyre!("deploy_interchain_token returned no token_id"))?;
    ui::tx_hash("Stellar deploy", &deploy_invoked.tx_hash_hex);
    ui::kv("token ID", &hex::encode(token_id));

    let token_address = client
        .its_query_token_address(main_wallet, its_contract, token_id)
        .await?
        .ok_or_else(|| eyre!("could not resolve interchain_token_address after deploy"))?;
    ui::address("token contract", &token_address);

    ui::info(&format!(
        "deploying remote AXE token to {destination_chain}..."
    ));
    let remote_invoked = client
        .its_deploy_remote_interchain_token(crate::stellar::DeployRemoteInterchainTokenRequest {
            wallet: main_wallet,
            its_contract,
            gateway_contract,
            salt,
            destination_chain: destination_axelar_id,
            gas_token,
            gas_amount: gas_stroops.get(),
        })
        .await?;
    if !remote_invoked.success {
        return Err(eyre!("Stellar deploy_remote_interchain_token failed"));
    }
    ui::tx_hash("Stellar remote-deploy", &remote_invoked.tx_hash_hex);
    let message_id = format!(
        "0x{}-{}",
        remote_invoked.tx_hash_hex.to_lowercase(),
        remote_invoked.event_index.unwrap_or(0)
    );
    let source_axelar_id = super::axelar_id_for_chain(config, source_chain)?;
    remote_verifier
        .wait_for_remote_deploy(
            config,
            &source_axelar_id,
            destination_axelar_id,
            &message_id,
        )
        .await?;

    let mut cache = cache;
    cache["tokenId"] = serde_json::json!(format!("0x{}", hex::encode(token_id)));
    cache["salt"] = serde_json::json!(format!("0x{}", hex::encode(salt)));
    cache["tokenAddress"] = serde_json::json!(token_address);
    super::save_its_cache(source_chain, destination_chain, &cache)?;
    remote_verifier.after_remote_deploy(source_chain, token_id);

    Ok(TokenSetup {
        token_id,
        token_address,
        decimals: TOKEN_DECIMALS,
    })
}

#[derive(Debug, thiserror::Error)]
enum TokenIdParseError {
    #[error(transparent)]
    Hex(#[from] hex::FromHexError),
    #[error("--token-id must be 32 bytes")]
    Length,
}

fn parse_token_id(value: &str) -> std::result::Result<[u8; 32], TokenIdParseError> {
    let bytes = hex::decode(value.strip_prefix("0x").unwrap_or(value))?;
    bytes.try_into().map_err(|_| TokenIdParseError::Length)
}

fn required_balance(decimals: u32, required_transfers: usize) -> u128 {
    scale_to_decimals(WHOLE_TOKENS_PER_KEY, decimals).saturating_mul(required_transfers as u128)
}

async fn reusable_cached_token(
    client: &StellarClient,
    main_wallet: &StellarWallet,
    its_contract: &str,
    cache: &serde_json::Value,
    required_transfers: usize,
) -> Result<Option<TokenSetup>> {
    let Some(token_id_hex) = cache.get("tokenId").and_then(|value| value.as_str()) else {
        return Ok(None);
    };
    let Some(salt_hex) = cache.get("salt").and_then(|value| value.as_str()) else {
        return Ok(None);
    };
    if parse_token_id(salt_hex).is_err() {
        return Ok(None);
    }
    let Ok(token_id) = parse_token_id(token_id_hex) else {
        return Ok(None);
    };

    let Ok(Some(token_address)) = client
        .its_query_token_address(main_wallet, its_contract, token_id)
        .await
    else {
        ui::warn("cached AXE token no longer registered on Stellar ITS, deploying fresh...");
        return Ok(None);
    };
    let decimals = client
        .token_decimals(&main_wallet.public_key_bytes, &token_address)
        .await?;
    let needed = required_balance(decimals, required_transfers);
    let balance = client
        .token_balance(main_wallet, &token_address, &main_wallet.public_key_bytes)
        .await
        .unwrap_or(0);
    if balance < needed {
        ui::warn(&format!(
            "cached AXE token has insufficient supply ({balance} < {needed}), deploying fresh..."
        ));
        return Ok(None);
    }
    ui::info(&format!("reusing cached ITS token: {token_address}"));
    Ok(Some(TokenSetup {
        token_id,
        token_address,
        decimals,
    }))
}

pub(super) async fn distribute_token_balances(
    client: &StellarClient,
    main_wallet: &StellarWallet,
    token_contract: &str,
    wallets: &[StellarWallet],
    amount_per_key: u128,
) -> Result<()> {
    let balance_progress = indicatif::ProgressBar::new(wallets.len() as u64);
    balance_progress.set_style(
        indicatif::ProgressStyle::with_template("  {bar:40.cyan/dim} {pos}/{len} balances checked")
            .unwrap()
            .progress_chars("=> "),
    );
    let mut to_fund = Vec::new();
    for (index, wallet) in wallets.iter().enumerate() {
        let balance = client
            .token_balance(main_wallet, token_contract, &wallet.public_key_bytes)
            .await
            .unwrap_or(0);
        if balance < amount_per_key {
            to_fund.push(index);
        }
        balance_progress.inc(1);
    }
    balance_progress.finish_and_clear();

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
    let funding_progress = indicatif::ProgressBar::new(to_fund.len() as u64);
    funding_progress.set_style(
        indicatif::ProgressStyle::with_template("  {bar:40.cyan/dim} {pos}/{len} keys funded")
            .unwrap()
            .progress_chars("=> "),
    );
    for &index in &to_fund {
        let invoked = client
            .token_transfer(
                main_wallet,
                token_contract,
                &wallets[index].public_key_bytes,
                amount_per_key,
            )
            .await?;
        if !invoked.success {
            return Err(eyre!(
                "AXE transfer failed for key {index} (tx {})",
                invoked.tx_hash_hex
            ));
        }
        funding_progress.inc(1);
    }
    funding_progress.finish_and_clear();
    ui::success(&format!(
        "distributed AXE to {} ephemeral keys",
        to_fund.len()
    ));
    Ok(())
}

pub(super) struct TransferRequest {
    pub client: StellarClient,
    pub wallet: Arc<StellarWallet>,
    pub its_contract: String,
    pub gateway_contract: String,
    pub token_id: [u8; 32],
    pub destination_chain: String,
    pub destination_address_bytes: Vec<u8>,
    pub gas_token: String,
    pub gas_amount_stroops: u64,
    pub transfer_amount: u128,
    pub gmp_dest_address: String,
}

pub(super) async fn submit_transfer(request: TransferRequest) -> TxMetrics {
    let submit_start = Instant::now();
    // ITS emits the `ContractCall` event from the AxelarGateway contract,
    // so VotingVerifier records the ITS contract as the source address.
    let source_addr = request.its_contract.to_string();
    match request
        .client
        .its_interchain_transfer(crate::stellar::InterchainTransferRequest {
            wallet: &request.wallet,
            its_contract: &request.its_contract,
            gateway_contract: &request.gateway_contract,
            token_id: request.token_id,
            destination_chain: &request.destination_chain,
            destination_address_bytes: &request.destination_address_bytes,
            amount: request.transfer_amount,
            data: None,
            gas_token: &request.gas_token,
            gas_amount: request.gas_amount_stroops,
        })
        .await
    {
        Ok(invoked) => {
            let submit_time_ms = submit_start.elapsed().as_millis() as u64;
            let event_index = invoked.event_index.unwrap_or(0);
            let message_id = format!("0x{}-{event_index}", invoked.tx_hash_hex.to_lowercase());
            TxMetrics {
                confirm_time_ms: Some(submit_time_ms),
                latency_ms: Some(submit_time_ms),
                source_address: source_addr,
                gmp_destination_chain: "axelar".to_string(),
                gmp_destination_address: request.gmp_dest_address.to_string(),
                send_instant: Some(submit_start),
                ..TxMetrics::from_outcome(
                    message_id,
                    submit_time_ms,
                    TxOutcome::from_external(invoked.success, None, "interchain_transfer reverted"),
                )
            }
        }
        Err(error) => {
            let elapsed_ms = submit_start.elapsed().as_millis() as u64;
            TxMetrics {
                source_address: source_addr,
                ..TxMetrics::failed("", elapsed_ms, error.to_string())
            }
        }
    }
}

/// Stellar ITS submission capability shared by every destination route.
#[derive(Clone)]
pub(super) struct ItsStellarSubmitter {
    pub client: StellarClient,
    pub its_contract: String,
    pub gateway_contract: String,
    pub token_id: TokenId,
    pub destination_chain: String,
    pub destination_address_bytes: Vec<u8>,
    pub gas_token: String,
    pub gas_stroops: Stroops,
    pub amount_per_tx: u128,
    pub axelarnet_gw_addr: String,
}

#[derive(Clone)]
pub(super) struct ItsStellarSubmitJob {
    pub wallet: Arc<StellarWallet>,
}

impl TransactionSubmitter for ItsStellarSubmitter {
    type Job = ItsStellarSubmitJob;

    async fn submit(&self, job: Self::Job) -> TxMetrics {
        submit_transfer(TransferRequest {
            client: self.client.clone(),
            wallet: job.wallet,
            its_contract: self.its_contract.clone(),
            gateway_contract: self.gateway_contract.clone(),
            token_id: self.token_id.into_bytes(),
            destination_chain: self.destination_chain.clone(),
            destination_address_bytes: self.destination_address_bytes.clone(),
            gas_token: self.gas_token.clone(),
            gas_amount_stroops: self.gas_stroops.get(),
            transfer_amount: self.amount_per_tx,
            gmp_dest_address: self.axelarnet_gw_addr.clone(),
        })
        .await
    }
}

pub(super) async fn run_its_burst(
    submitter: ItsStellarSubmitter,
    wallets: Vec<StellarWallet>,
    max_concurrent: usize,
) -> Result<super::submitter::BurstResult> {
    let jobs = wallets
        .into_iter()
        .map(|wallet| ItsStellarSubmitJob {
            wallet: Arc::new(wallet),
        })
        .collect();
    super::submitter::run_burst(submitter, jobs, max_concurrent).await
}

/// Pacing and reporting inputs for one sustained Stellar-source run.
pub(super) struct SustainedTransferArgs {
    pub submitter: ItsStellarSubmitter,
    pub wallets: Vec<StellarWallet>,
    pub plan: SustainedPlan,
    pub verify_tx: Option<tokio::sync::mpsc::UnboundedSender<super::verify::PendingTx>>,
    pub send_done: Option<Arc<AtomicBool>>,
    pub spinner: indicatif::ProgressBar,
}

pub(super) async fn run_sustained(
    args: SustainedTransferArgs,
) -> Result<sustained::SustainedResult> {
    let jobs = args
        .wallets
        .into_iter()
        .map(|wallet| ItsStellarSubmitJob {
            wallet: Arc::new(wallet),
        })
        .collect();
    let make_task = its_sustained_tasks(args.submitter, jobs, args.verify_tx);

    sustained::run_sustained_loop(args.plan, None, make_task, args.send_done, args.spinner).await
}

fn its_sustained_tasks(
    submitter: ItsStellarSubmitter,
    jobs: Vec<ItsStellarSubmitJob>,
    verify_tx: Option<tokio::sync::mpsc::UnboundedSender<super::verify::PendingTx>>,
) -> sustained::MakeTask {
    sustained::submission_tasks(
        submitter,
        move |key_index, _| jobs[key_index].clone(),
        verify_tx,
        sustained::ItsPendingTxAdapter {
            // Stellar ITS verification starts at the Voted stage.
            has_voting_verifier: true,
        },
    )
}
