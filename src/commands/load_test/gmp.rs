//! Per-pair GMP load-test orchestrators (the `run_*` functions).
//!
//! Each function drives one source × destination pair end-to-end:
//! deploy or reuse a `SenderReceiver`, fund derived signing keys, fire
//! the configured number of `callContract` txs, and then hand off to the
//! `verify` module for batch / streaming amplifier verification.
//!
//! Shape is consistent across pairs (validate RPCs → load wallet →
//! deploy/reuse SR → derive signers → distribute funds → fire → verify).
//! See the `// ===` headers for the four-quadrant layout: native pairs at
//! the top, then EVM↔Stellar, EVM↔Solana via Stellar, Sui sources, and
//! finally Sui destinations.

use std::time::Instant;

use alloy::{
    providers::{Provider, ProviderBuilder},
    signers::local::PrivateKeySigner,
    sol_types::SolValue,
};
use eyre::Result;

use super::evm_sender;
use super::gmp_verification;
use super::helpers::list_gateway_chains;
use super::metrics::{ComputeUnitSummary, LoadTestReport, ReportInput, RunIdentity};
use super::run_sizing::{RunSizing, SustainedPlan};
use super::sol_sender;
use super::stellar_sender;
use super::sustained;
use super::verification_session;
use super::verify;
use super::{
    LoadTestArgs, check_evm_balance, deploy_or_reuse_sender_receiver, deploy_sender_receiver,
    ensure_evm_contract_deployed, ensure_sender_receiver, finish_report, load_stellar_main_wallet,
    load_sui_main_wallet, read_cache, read_stellar_contract_address, read_stellar_network_type,
    read_stellar_token_address, save_cache, validate_evm_rpc, validate_solana_rpc,
};
use crate::config::ChainsConfig;
use crate::ui;

mod sui_destination;

pub(super) use self::sui_destination::{run_evm_to_sui, run_sol_to_sui, run_stellar_to_sui};

async fn load_evm_signer(
    args: &LoadTestArgs,
    rpc_url: &str,
) -> Result<(PrivateKeySigner, [u8; 32])> {
    let private_key = args.private_key.as_ref().ok_or_else(|| {
        eyre::eyre!("EVM private key required. Set EVM_PRIVATE_KEY env var or use --private-key")
    })?;
    let signer: PrivateKeySigner = private_key.parse()?;
    let signer_address = signer.address();
    let provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);
    check_evm_balance(&provider, signer_address).await?;
    let balance: u128 = provider.get_balance(signer_address).await?.to();
    let eth = balance as f64 / 1e18;
    ui::kv("wallet", &format!("{signer_address} ({eth:.6} ETH)"));
    let main_key = signer.to_bytes().into();
    Ok((signer, main_key))
}

async fn prepare_sol_to_evm_destination(
    args: &LoadTestArgs,
    rpc_url: &str,
    gateway_addr: alloy::primitives::Address,
    gas_service_addr: alloy::primitives::Address,
) -> Result<(alloy::primitives::Address, impl Provider)> {
    let destination = &args.destination_chain;
    let cache = read_cache(destination);
    if let Some(addr_str) = cache.sender_receiver_address.as_deref() {
        let read_provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);
        let addr = addr_str.parse()?;
        let code = read_provider.get_code_at(addr).await?;
        let needs_redeploy = if code.is_empty() {
            ui::warn("cached SenderReceiver has no code, redeploying...");
            true
        } else {
            let sender_receiver = crate::evm::SenderReceiver::new(addr, &read_provider);
            match sender_receiver.gateway().call().await {
                Ok(onchain_gateway) if onchain_gateway != gateway_addr => {
                    ui::warn(&format!(
                        "cached SenderReceiver points to old gateway {onchain_gateway}, expected \
                         {gateway_addr}, redeploying..."
                    ));
                    true
                }
                Err(_) => {
                    ui::warn("cached SenderReceiver gateway check failed, redeploying...");
                    true
                }
                _ => false,
            }
        };
        if !needs_redeploy {
            ui::info(&format!("SenderReceiver: reusing {addr}"));
            let private_key = args.private_key.as_deref().ok_or_else(|| {
                eyre::eyre!(
                    "EVM private key required to use the cached SenderReceiver. \
                     Set EVM_PRIVATE_KEY env var or use --private-key"
                )
            })?;
            let signer = private_key.parse::<PrivateKeySigner>()?;
            let provider = ProviderBuilder::new()
                .wallet(signer)
                .connect_http(rpc_url.parse()?);
            return Ok((addr, provider));
        }
        let signer = deployment_signer(args, &read_provider).await?;
        let provider = ProviderBuilder::new()
            .wallet(signer)
            .connect_http(rpc_url.parse()?);
        let addr = deploy_sender_receiver(&provider, gateway_addr, gas_service_addr).await?;
        let mut cache = cache;
        cache.sender_receiver_address = Some(addr.to_string());
        save_cache(destination, &cache)?;
        return Ok((addr, provider));
    }

    let read_provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);
    let signer = deployment_signer(args, &read_provider).await?;
    ui::info("deploying SenderReceiver on destination chain...");
    let provider = ProviderBuilder::new()
        .wallet(signer)
        .connect_http(rpc_url.parse()?);
    let addr = deploy_sender_receiver(&provider, gateway_addr, gas_service_addr).await?;
    let mut cache = cache;
    cache.sender_receiver_address = Some(addr.to_string());
    save_cache(destination, &cache)?;
    Ok((addr, provider))
}

async fn deployment_signer<P: Provider>(
    args: &LoadTestArgs,
    provider: &P,
) -> Result<PrivateKeySigner> {
    let private_key = args.private_key.as_ref().ok_or_else(|| {
        eyre::eyre!(
            "EVM private key required to deploy SenderReceiver. Set EVM_PRIVATE_KEY env var or use --private-key"
        )
    })?;
    let signer: PrivateKeySigner = private_key.parse()?;
    check_evm_balance(provider, signer.address()).await?;
    Ok(signer)
}

pub(super) async fn run_sol_to_evm(
    args: LoadTestArgs,
    _run_start: Instant,
    sizing: RunSizing,
) -> Result<()> {
    let dest = &args.destination_chain;
    let src = &args.source_chain;

    let cfg = ChainsConfig::load(&args.config)?;

    let rpc_url = &args.destination_rpc;

    // Validate RPCs before doing any work
    validate_solana_rpc(&args.source_rpc).await?;
    validate_evm_rpc(rpc_url).await?;

    // Check that verification contracts exist for this chain pair before doing any work.
    // A legacy (consensus) EVM destination has no Cosmos Gateway and is verified
    // on its on-chain gateway instead, so it's exempt.
    let (_, dst_legacy) =
        verify::classify_route(&cfg, &args.source_axelar_id, &args.destination_axelar_id);
    if !dst_legacy && cfg.axelar.contract_address("Gateway", dest).is_err() {
        eyre::bail!(
            "destination chain '{dest}' has no Cosmos Gateway in the config — \
             verification would fail. Pick a chain that has a Gateway entry, e.g.:\n  {}",
            list_gateway_chains(&cfg).join(", ")
        );
    }

    ui::kv("source", src);
    ui::kv("destination", dest);

    let gateway_addr = cfg
        .chains
        .get(dest)
        .ok_or_else(|| eyre::eyre!("chain '{}' not found in config", dest))?
        .contract_address("AxelarGateway", dest)?
        .parse()?;
    let gas_service_addr = cfg
        .chains
        .get(dest)
        .ok_or_else(|| eyre::eyre!("chain '{}' not found in config", dest))?
        .contract_address("AxelarGasService", dest)?
        .parse()?;

    ui::address("EVM gateway", &format!("{gateway_addr}"));

    // Without this check, a missing/undeployed EVM gateway causes the verifier
    // to silently report 30/30 executed (eth_call on an EOA returns 0x →
    // alloy decodes it as `false` → our pipeline interprets that as "approval
    // consumed = executed"). Fail fast instead.
    ensure_evm_contract_deployed(rpc_url, "destination AxelarGateway", gateway_addr).await?;

    let (sender_receiver_addr, provider) =
        prepare_sol_to_evm_destination(&args, rpc_url, gateway_addr, gas_service_addr).await?;
    ui::address("SenderReceiver", &format!("{sender_receiver_addr}"));
    let destination_address = format!("{sender_receiver_addr}");

    let test_start = Instant::now();
    let mut report = if let Some(plan) = sizing.sustained() {
        let mut verification = verification_session::VerificationSession::start(
            verify::VerificationRoute::from_args(&args),
            gmp_verification::EvmGmpStreamingTarget {
                address: destination_address.clone(),
                gateway_addr,
                rpc_url: args.destination_rpc.clone(),
                legacy: false,
                message_matcher: verification_session::MessageMatcher::Solana,
            },
        );

        let mut report = sol_sender::run_sustained_load_test_with_metrics(
            &args,
            plan,
            true,
            &destination_address,
            Some(verification.sender()),
            Some(verification.send_done()),
            verification.spinner_sender()?,
        )
        .await?;

        verification.finish(&mut report, args.network).await?;
        report
    } else {
        let mut report = sol_sender::run_load_test_with_metrics(
            &args,
            sizing.require_burst("GMP sol-to-evm")?,
            &destination_address,
            true,
        )
        .await?;
        gmp_verification::attach_batch(
            &args,
            gmp_verification::EvmGmpTarget {
                address: destination_address.to_string(),
                gateway_addr,
                provider: &provider,
                source_type: verify::SourceChainType::Svm,
                legacy: false,
            },
            &mut report,
        )
        .await?;
        report
    };

    finish_report(&mut report, test_start)
}

pub(super) async fn run_evm_to_sol(
    args: LoadTestArgs,
    _run_start: Instant,
    sizing: RunSizing,
) -> Result<()> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;

    let cfg = ChainsConfig::load(&args.config)?;

    let evm_rpc_url = args.source_rpc.clone();

    // Validate RPCs before doing any work
    validate_evm_rpc(&evm_rpc_url).await?;
    validate_solana_rpc(&args.destination_rpc).await?;

    // Check that verification contracts exist for this chain pair before doing any work
    if cfg.axelar.contract_address("Gateway", dest).is_err() {
        eyre::bail!(
            "destination chain '{dest}' has no Cosmos Gateway in the config — \
             verification would fail. Pick a chain that has a Gateway entry, e.g.:\n  {}",
            list_gateway_chains(&cfg).join(", ")
        );
    }

    ui::kv("source", src);
    ui::kv("destination", dest);

    let gateway_addr = cfg
        .chains
        .get(src)
        .ok_or_else(|| eyre::eyre!("chain '{}' not found in config", src))?
        .contract_address("AxelarGateway", src)?
        .parse()?;
    let gas_service_addr = cfg
        .chains
        .get(src)
        .ok_or_else(|| eyre::eyre!("chain '{}' not found in config", src))?
        .contract_address("AxelarGasService", src)?
        .parse()?;
    ui::address("EVM gateway", &format!("{gateway_addr}"));

    let (signer, main_key) = load_evm_signer(&args, &evm_rpc_url).await?;
    let read_provider = ProviderBuilder::new().connect_http(evm_rpc_url.parse()?);
    let write_provider = ProviderBuilder::new()
        .wallet(signer.clone())
        .connect_http(evm_rpc_url.parse()?);

    // --- Deploy/reuse SenderReceiver on source chain ---
    let cache_key = &format!("{src}-evm-to-sol");
    let cache = read_cache(cache_key);
    let sender_receiver_addr = deploy_or_reuse_sender_receiver(
        &cache,
        cache_key,
        &read_provider,
        &write_provider,
        gateway_addr,
        gas_service_addr,
        "source",
    )
    .await?;
    ui::address("SenderReceiver", &format!("{sender_receiver_addr}"));

    // Destination on Solana: memo program (resolved from the target network)
    let destination_address = evm_sender::memo_program_id(args.network).to_string();
    let destination_address = destination_address.as_str();
    ui::kv("destination program", destination_address);

    let test_start = Instant::now();
    let mut report = if let Some(plan) = sizing.sustained() {
        let mut verification = verification_session::VerificationSession::start(
            verify::VerificationRoute::from_args(&args),
            gmp_verification::SolanaGmpStreamingTarget {
                address: destination_address.to_string(),
                rpc_url: args.destination_rpc.clone(),
            },
        );

        let mut report =
            evm_sender::run_sustained_load_test_with_metrics(evm_sender::SustainedLoadRequest {
                args: args.clone(),
                plan,
                sender_receiver: sender_receiver_addr,
                main_key,
                evm_rpc_url: evm_rpc_url.clone(),
                destination_address: destination_address.to_string(),
                verify_tx: Some(verification.sender()),
                send_done: Some(verification.send_done()),
                verify_spinner_tx: verification.spinner_sender()?,
                evm_destination: false,
            })
            .await?;

        verification.finish(&mut report, args.network).await?;
        report
    } else {
        let mut report = evm_sender::run_load_test_with_metrics(
            &args,
            sizing.require_burst("GMP evm-to-sol")?,
            sender_receiver_addr,
            &main_key,
            &evm_rpc_url,
            destination_address,
            false,
        )
        .await?;

        gmp_verification::attach_batch(
            &args,
            gmp_verification::SolanaGmpTarget {
                address: destination_address.to_string(),
                rpc_url: args.destination_rpc.to_string(),
                source_type: verify::SourceChainType::Evm,
            },
            &mut report,
        )
        .await?;
        report
    };

    finish_report(&mut report, test_start)
}

#[derive(Clone, Copy)]
struct EvmGmpContracts {
    gateway: alloy::primitives::Address,
    gas_service: alloy::primitives::Address,
}

fn resolve_evm_gmp_contracts(
    cfg: &ChainsConfig,
    chain: &str,
    gateway_label: &str,
) -> Result<EvmGmpContracts> {
    let chain_config = cfg
        .chains
        .get(chain)
        .ok_or_else(|| eyre::eyre!("chain '{}' not found in config", chain))?;
    let contracts = EvmGmpContracts {
        gateway: chain_config
            .contract_address("AxelarGateway", chain)?
            .parse()?,
        gas_service: chain_config
            .contract_address("AxelarGasService", chain)?
            .parse()?,
    };
    ui::address(gateway_label, &format!("{}", contracts.gateway));
    Ok(contracts)
}

async fn prepare_evm_gmp_source(
    chain: &str,
    rpc_url: &str,
    signer: PrivateKeySigner,
    contracts: EvmGmpContracts,
) -> Result<alloy::primitives::Address> {
    let read_provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);
    let write_provider = ProviderBuilder::new()
        .wallet(signer)
        .connect_http(rpc_url.parse()?);
    let cache_key = format!("{chain}-evm-to-evm");
    let sender_receiver = deploy_or_reuse_sender_receiver(
        &read_cache(&cache_key),
        &cache_key,
        &read_provider,
        &write_provider,
        contracts.gateway,
        contracts.gas_service,
        "source",
    )
    .await?;
    ui::address("SenderReceiver (source)", &format!("{sender_receiver}"));
    Ok(sender_receiver)
}

async fn prepare_evm_gmp_destination(
    chain: &str,
    rpc_url: &str,
    signer: PrivateKeySigner,
    contracts: EvmGmpContracts,
) -> Result<(alloy::primitives::Address, impl Provider)> {
    ensure_evm_contract_deployed(rpc_url, "destination AxelarGateway", contracts.gateway).await?;
    let signer_address = signer.address();
    let read_provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);
    if read_provider.get_balance(signer_address).await? == alloy::primitives::U256::ZERO {
        eyre::bail!(
            "EVM wallet {signer_address} has no funds on destination chain '{chain}'. \
             Fund it first."
        );
    }
    let write_provider = ProviderBuilder::new()
        .wallet(signer)
        .connect_http(rpc_url.parse()?);
    let cache_key = format!("{chain}-evm-to-evm-dest");
    let sender_receiver = deploy_or_reuse_sender_receiver(
        &read_cache(&cache_key),
        &cache_key,
        &read_provider,
        &write_provider,
        contracts.gateway,
        contracts.gas_service,
        "destination",
    )
    .await?;
    ui::address(
        "SenderReceiver (destination)",
        &format!("{sender_receiver}"),
    );
    Ok((sender_receiver, read_provider))
}

pub(super) async fn run_evm_to_evm(
    args: LoadTestArgs,
    _run_start: Instant,
    sizing: RunSizing,
) -> Result<()> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;

    let cfg = ChainsConfig::load(&args.config)?;

    // Classify each end as legacy (consensus) or Amplifier. A legacy-touching
    // route is verified entirely on the destination gateway, so it does not
    // need the Amplifier Cosmos contracts the standard path requires.
    let (src_legacy, dst_legacy) =
        verify::classify_route(&cfg, &args.source_axelar_id, &args.destination_axelar_id);
    let legacy_route = src_legacy || dst_legacy;

    let source_rpc_url = args.source_rpc.clone();
    let dest_rpc_url = args.destination_rpc.clone();

    // Validate RPCs before doing any work
    validate_evm_rpc(&source_rpc_url).await?;
    validate_evm_rpc(&dest_rpc_url).await?;

    // Amplifier destinations need a Cosmos Gateway for verification; legacy
    // (consensus) destinations are verified directly on their on-chain gateway.
    if !dst_legacy && cfg.axelar.contract_address("Gateway", dest).is_err() {
        eyre::bail!(
            "destination chain '{dest}' has no Cosmos Gateway in the config — \
             verification would fail. Pick a chain that has a Gateway entry, e.g.:\n  {}",
            list_gateway_chains(&cfg).join(", ")
        );
    }

    ui::kv("source", src);
    ui::kv("destination", dest);

    let (signer, main_key) = load_evm_signer(&args, &source_rpc_url).await?;
    let source_contracts = resolve_evm_gmp_contracts(&cfg, src, "source gateway")?;
    let sender_receiver_addr =
        prepare_evm_gmp_source(src, &source_rpc_url, signer.clone(), source_contracts).await?;
    let destination_contracts = resolve_evm_gmp_contracts(&cfg, dest, "destination gateway")?;
    let (dest_sender_receiver, dest_read_provider) =
        prepare_evm_gmp_destination(dest, &dest_rpc_url, signer, destination_contracts).await?;
    let dest_gateway_addr = destination_contracts.gateway;

    let destination_address = format!("{dest_sender_receiver}");

    let test_start = Instant::now();
    let mut report = if let Some(plan) = sizing.sustained() {
        let mut verification = verification_session::VerificationSession::start(
            verify::VerificationRoute::from_args(&args),
            gmp_verification::EvmGmpStreamingTarget {
                address: destination_address.clone(),
                gateway_addr: dest_gateway_addr,
                rpc_url: dest_rpc_url.clone(),
                legacy: legacy_route,
                message_matcher: verification_session::MessageMatcher::Exact,
            },
        );

        let mut report =
            evm_sender::run_sustained_load_test_with_metrics(evm_sender::SustainedLoadRequest {
                args: args.clone(),
                plan,
                sender_receiver: sender_receiver_addr,
                main_key,
                evm_rpc_url: source_rpc_url.clone(),
                destination_address: destination_address.clone(),
                verify_tx: Some(verification.sender()),
                send_done: Some(verification.send_done()),
                verify_spinner_tx: verification.spinner_sender()?,
                evm_destination: true,
            })
            .await?;

        verification.finish(&mut report, args.network).await?;
        report
    } else {
        let mut report = evm_sender::run_load_test_with_metrics(
            &args,
            sizing.require_burst("GMP evm-to-evm")?,
            sender_receiver_addr,
            &main_key,
            &source_rpc_url,
            &destination_address,
            true,
        )
        .await?;

        gmp_verification::attach_batch(
            &args,
            gmp_verification::EvmGmpTarget {
                address: destination_address.to_string(),
                gateway_addr: dest_gateway_addr,
                provider: &dest_read_provider,
                source_type: verify::SourceChainType::Evm,
                legacy: legacy_route,
            },
            &mut report,
        )
        .await?;
        report
    };

    finish_report(&mut report, test_start)
}

pub(super) async fn run_sol_to_sol(
    args: LoadTestArgs,
    _run_start: Instant,
    sizing: RunSizing,
) -> Result<()> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;

    // Validate RPCs
    validate_solana_rpc(&args.source_rpc).await?;
    validate_solana_rpc(&args.destination_rpc).await?;

    let cfg = ChainsConfig::load(&args.config)?;

    // Check that verification contracts exist
    if cfg.axelar.contract_address("Gateway", dest).is_err() {
        eyre::bail!(
            "destination chain '{dest}' has no Cosmos Gateway in the config — \
             verification would fail. Pick a chain that has a Gateway entry, e.g.:\n  {}",
            list_gateway_chains(&cfg).join(", ")
        );
    }

    ui::kv("source", src);
    ui::kv("destination", dest);

    // Destination is the Solana memo program
    let destination_address = evm_sender::memo_program_id(args.network).to_string();
    let destination_address = destination_address.as_str();
    ui::kv("destination program", destination_address);

    let test_start = Instant::now();
    let mut report = if let Some(plan) = sizing.sustained() {
        let mut verification = verification_session::VerificationSession::start(
            verify::VerificationRoute::from_args(&args),
            gmp_verification::SolanaGmpStreamingTarget {
                address: destination_address.to_string(),
                rpc_url: args.destination_rpc.clone(),
            },
        );

        let mut report = sol_sender::run_sustained_load_test_with_metrics(
            &args,
            plan,
            false,
            destination_address,
            Some(verification.sender()),
            Some(verification.send_done()),
            verification.spinner_sender()?,
        )
        .await?;

        verification.finish(&mut report, args.network).await?;
        report
    } else {
        let mut report = sol_sender::run_load_test_with_metrics(
            &args,
            sizing.require_burst("GMP sol-to-sol")?,
            destination_address,
            false,
        )
        .await?;

        gmp_verification::attach_batch(
            &args,
            gmp_verification::SolanaGmpTarget {
                address: destination_address.to_string(),
                rpc_url: args.destination_rpc.to_string(),
                source_type: verify::SourceChainType::Svm,
            },
            &mut report,
        )
        .await?;
        report
    };

    finish_report(&mut report, test_start)
}

// ---------------------------------------------------------------------------
// Stellar -> EVM (GMP)
// ---------------------------------------------------------------------------

struct StellarGmpSource {
    client: crate::stellar::StellarClient,
    wallets: Vec<crate::stellar::StellarWallet>,
    example_contract: String,
    gateway_contract: String,
    gas_token_contract: String,
    gas_amount: super::units::Stroops,
    payload_override: Option<Vec<u8>>,
}

async fn prepare_cached_evm_destination(
    args: &LoadTestArgs,
    cfg: &ChainsConfig,
    missing_key_error: &str,
) -> Result<(
    alloy::primitives::Address,
    impl Provider,
    alloy::primitives::Address,
)> {
    let destination = &args.destination_chain;
    let contracts = resolve_evm_gmp_contracts(cfg, destination, "EVM gateway")?;
    ensure_evm_contract_deployed(
        &args.destination_rpc,
        "destination AxelarGateway",
        contracts.gateway,
    )
    .await?;
    let private_key = args
        .private_key
        .clone()
        .or_else(|| std::env::var("EVM_PRIVATE_KEY").ok());
    let cache = read_cache(destination);
    if cache.sender_receiver_address.is_none() && private_key.is_none() {
        eyre::bail!("{missing_key_error}");
    }
    let (sender_receiver, provider) = ensure_sender_receiver(
        args,
        &args.destination_rpc,
        contracts.gateway,
        contracts.gas_service,
        cache,
        private_key.as_deref(),
    )
    .await?;
    ui::address("SenderReceiver", &format!("{sender_receiver}"));
    Ok((sender_receiver, provider, contracts.gateway))
}

async fn prepare_stellar_gmp_source(
    args: &LoadTestArgs,
    sizing: RunSizing,
) -> Result<StellarGmpSource> {
    let source = &args.source_chain;
    let network_type = read_stellar_network_type(&args.config, source)?;
    let use_friendbot = matches!(network_type.as_str(), "testnet" | "futurenet");
    let client = crate::stellar::StellarClient::new(&args.source_rpc, &network_type)?;
    ui::kv("network", &network_type);

    let example_contract = read_stellar_contract_address(&args.config, source, "AxelarExample")?;
    let gateway_contract = read_stellar_contract_address(&args.config, source, "AxelarGateway")?;
    let gas_token_contract = read_stellar_token_address(&args.config, source)?;
    ui::address("Stellar AxelarExample", &example_contract);
    ui::address("Stellar AxelarGateway", &gateway_contract);
    ui::address("Stellar XLM token", &gas_token_contract);

    let gas_amount = stellar_sender::parse_gas_stroops(args.gas_value.as_deref())?;
    ui::kv(
        "gas",
        &format!(
            "{gas_amount} stroops ({:.4} XLM)",
            gas_amount.get() as f64 / 10_000_000.0
        ),
    );
    let main_wallet = load_stellar_main_wallet(args.private_key.as_deref())?;
    ui::info(&format!("deriving {} Stellar keys...", sizing.num_keys));
    let wallets =
        stellar_sender::derive_wallets(&main_wallet.signing_key.to_bytes(), sizing.num_keys)?;
    let starting_balance =
        stellar_sender::mainnet_per_key_balance_stroops(gas_amount, sizing.transactions_per_key());
    stellar_sender::ensure_funded(
        &client,
        &wallets,
        use_friendbot,
        &main_wallet,
        starting_balance,
    )
    .await?;
    let payload_override = args
        .payload
        .as_deref()
        .map(|value| hex::decode(value.strip_prefix("0x").unwrap_or(value)))
        .transpose()?;
    Ok(StellarGmpSource {
        client,
        wallets,
        example_contract,
        gateway_contract,
        gas_token_contract,
        gas_amount,
        payload_override,
    })
}

async fn run_stellar_gmp_sustained(
    args: &LoadTestArgs,
    cfg: &ChainsConfig,
    source: StellarGmpSource,
    destination_address: &str,
    gateway_addr: alloy::primitives::Address,
    sender_receiver_addr: alloy::primitives::Address,
    sizing: RunSizing,
) -> Result<LoadTestReport> {
    let SustainedPlan {
        tps,
        duration_secs,
        key_cycle,
    } = sizing
        .sustained()
        .ok_or_else(|| eyre::eyre!("sustained plan required"))?;
    let mut verification = verification_session::VerificationSession::start(
        verify::VerificationRoute::from_args(args),
        gmp_verification::EvmGmpStreamingTarget {
            address: destination_address.to_string(),
            gateway_addr,
            rpc_url: args.destination_rpc.clone(),
            legacy: false,
            message_matcher: verification_session::MessageMatcher::Exact,
        },
    );
    let spinner = ui::wait_spinner(&format!(
        "[0/{duration_secs}s] starting sustained Stellar GMP send..."
    ));
    verification
        .spinner_sender()?
        .send(spinner.clone())
        .map_err(|_| eyre::eyre!("GMP verification task stopped before sending"))?;
    let result = stellar_sender::run_sustained(stellar_sender::SustainedRequest {
        client: source.client,
        wallets: source.wallets,
        example_contract: source.example_contract,
        gateway_contract: source.gateway_contract,
        destination_chain: args.destination_axelar_id.clone(),
        destination_address: destination_address.to_string(),
        payload_override: source.payload_override,
        tps,
        duration_secs,
        key_cycle,
        verify_tx: Some(verification.sender()),
        send_done: Some(verification.send_done()),
        spinner,
        has_voting_verifier: cfg
            .axelar
            .contract_address("VotingVerifier", &args.source_chain)
            .is_ok(),
        destination_contract_addr: sender_receiver_addr,
        gas_token_contract: source.gas_token_contract,
        gas_amount: source.gas_amount,
    })
    .await?;
    let mut report = sustained::build_sustained_report(
        result,
        RunIdentity::from_sizing(args, sizing),
        destination_address,
        sizing.total_expected,
        sizing.num_keys,
    );
    verification.finish(&mut report, args.network).await?;
    Ok(report)
}

async fn run_stellar_gmp_burst<P: Provider>(
    args: &LoadTestArgs,
    source: StellarGmpSource,
    destination_address: &str,
    gateway_addr: alloy::primitives::Address,
    provider: &P,
    sizing: RunSizing,
) -> Result<LoadTestReport> {
    let mut report = stellar_sender::run_burst(stellar_sender::BurstRequest {
        client: source.client,
        wallets: source.wallets,
        example_contract: source.example_contract,
        gateway_contract: source.gateway_contract,
        destination_chain: args.destination_axelar_id.clone(),
        destination_address: destination_address.to_string(),
        payload_override: source.payload_override,
        run: RunIdentity::from_sizing(args, sizing),
        gas_token_contract: source.gas_token_contract,
        gas_amount: source.gas_amount,
    })
    .await?;
    gmp_verification::attach_batch(
        args,
        gmp_verification::EvmGmpTarget {
            address: destination_address.to_string(),
            gateway_addr,
            provider,
            source_type: verify::SourceChainType::Stellar,
            legacy: false,
        },
        &mut report,
    )
    .await?;
    Ok(report)
}

pub(super) async fn run_stellar_to_evm(
    args: LoadTestArgs,
    _run_start: Instant,
    run_sizing: RunSizing,
) -> Result<()> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;
    let cfg = ChainsConfig::load(&args.config)?;

    let evm_rpc_url = args.destination_rpc.clone();
    validate_evm_rpc(&evm_rpc_url).await?;

    // A legacy (consensus) EVM destination has no Cosmos Gateway and is verified
    // on its on-chain gateway instead, so it's exempt from this check.
    let (_, dst_legacy) =
        verify::classify_route(&cfg, &args.source_axelar_id, &args.destination_axelar_id);
    if !dst_legacy && cfg.axelar.contract_address("Gateway", dest).is_err() {
        eyre::bail!(
            "destination chain '{dest}' has no Cosmos Gateway in the config — verification would fail."
        );
    }

    ui::kv("source", src);
    ui::kv("destination", dest);
    ui::kv("protocol", "GMP (call_contract)");

    let source = prepare_stellar_gmp_source(&args, run_sizing).await?;
    let missing_key_error = format!(
        "no SenderReceiver cached for '{dest}' and no EVM private key available. \
         Set EVM_PRIVATE_KEY (in .env or via --private-key) so axe can deploy the destination \
         SenderReceiver on first run."
    );
    let (sender_receiver_addr, provider, gateway_addr) =
        prepare_cached_evm_destination(&args, &cfg, &missing_key_error).await?;
    let destination_address = format!("{sender_receiver_addr}");

    let test_start = Instant::now();
    let mut report = if run_sizing.sustained().is_some() {
        run_stellar_gmp_sustained(
            &args,
            &cfg,
            source,
            &destination_address,
            gateway_addr,
            sender_receiver_addr,
            run_sizing,
        )
        .await?
    } else {
        run_stellar_gmp_burst(
            &args,
            source,
            &destination_address,
            gateway_addr,
            &provider,
            run_sizing,
        )
        .await?
    };

    finish_report(&mut report, test_start)
}

// ===========================================================================
// EVM <-> Stellar GMP and Solana <-> Stellar GMP
// ===========================================================================
//
// All four functions are burst-only for simplicity in the initial cut. They
// share the same skeleton: derive ephemeral source signers, fund them, send
// in parallel with a semaphore, then run the appropriate destination
// verifier.

pub(super) async fn run_evm_to_stellar(
    args: LoadTestArgs,
    _run_start: Instant,
    sizing: RunSizing,
) -> Result<()> {
    sizing.require_burst("GMP evm-to-stellar")?;
    let src = &args.source_chain;
    let dest = &args.destination_chain;
    let cfg = ChainsConfig::load(&args.config)?;
    let evm_rpc_url = args.source_rpc.clone();
    validate_evm_rpc(&evm_rpc_url).await?;

    // EVM source ITS proxy is reused for GMP — we send via the destination
    // contract address (Stellar AxelarExample). EVM emits ContractCall.
    let signer = args
        .private_key
        .as_ref()
        .ok_or_else(|| {
            eyre::eyre!("EVM private key required. Set EVM_PRIVATE_KEY or use --private-key")
        })?
        .parse::<PrivateKeySigner>()?;
    let read_provider = ProviderBuilder::new().connect_http(evm_rpc_url.parse()?);
    check_evm_balance(&read_provider, signer.address()).await?;

    let stellar_rpc = &args.destination_rpc;
    let stellar_network_type = read_stellar_network_type(&args.config, dest)?;
    let stellar_gateway_addr = read_stellar_contract_address(&args.config, dest, "AxelarGateway")?;
    let stellar_example_addr = read_stellar_contract_address(&args.config, dest, "AxelarExample")?;
    ui::address("Stellar gateway", &stellar_gateway_addr);
    ui::address("Stellar AxelarExample", &stellar_example_addr);

    // Reuse the EVM-source GMP runner (sol_sender for sol-to-X has its
    // EVM analogue in evm_sender). For sustained, we'd add streaming;
    // burst-only here.
    let evm_gateway_addr = cfg
        .chains
        .get(src)
        .ok_or_else(|| eyre::eyre!("chain '{}' not found in config", src))?
        .contract_address("AxelarGateway", src)?
        .parse()?;
    let evm_gas_service_addr = cfg
        .chains
        .get(src)
        .ok_or_else(|| eyre::eyre!("chain '{}' not found in config", src))?
        .contract_address("AxelarGasService", src)?
        .parse()?;
    ui::address("EVM gateway", &format!("{evm_gateway_addr}"));

    // Deploy/reuse SenderReceiver as the EVM-side caller (existing pattern).
    let cache = read_cache(src);
    let evm_pk = args.private_key.clone();
    let (sender_receiver_addr, _provider) = ensure_sender_receiver(
        &args,
        &evm_rpc_url,
        evm_gateway_addr,
        evm_gas_service_addr,
        cache,
        evm_pk.as_deref(),
    )
    .await?;
    ui::address("EVM SenderReceiver", &format!("{sender_receiver_addr}"));

    // The destination address for the EVM-side callContract is the Stellar
    // `AxelarExample` C-address as a UTF-8 string.
    let main_key: [u8; 32] = signer.to_bytes().into();
    let test_start = Instant::now();
    let mut report = evm_sender::run_load_test_with_metrics(
        &args,
        sizing.require_burst("GMP evm-to-stellar")?,
        sender_receiver_addr,
        &main_key,
        &evm_rpc_url,
        &stellar_example_addr,
        true, // EVM-style ABI-encoded payload (Stellar AxelarExample.execute accepts raw bytes)
    )
    .await?;

    let signer_pk: [u8; 32] = alloy::primitives::keccak256(signer.address().as_slice()).into();
    gmp_verification::attach_batch(
        &args,
        gmp_verification::StellarGmpTarget {
            contract: stellar_example_addr.to_string(),
            rpc_url: stellar_rpc.to_string(),
            network_type: stellar_network_type.to_string(),
            gateway_contract: stellar_gateway_addr.to_string(),
            signer_pk,
            source_type: verify::SourceChainType::Evm,
        },
        &mut report,
    )
    .await?;

    finish_report(&mut report, test_start)
}

pub(super) async fn run_stellar_to_sol(
    args: LoadTestArgs,
    _run_start: Instant,
    sizing: RunSizing,
) -> Result<()> {
    let num_txs = sizing.require_burst("GMP stellar-to-sol")?;
    let src = &args.source_chain;
    let dest = &args.destination_chain;

    let solana_rpc = args.destination_rpc.clone();
    validate_solana_rpc(&solana_rpc).await?;

    ui::kv("source", src);
    ui::kv("destination", dest);
    ui::kv("protocol", "GMP (Stellar AxelarExample.send → Solana memo)");

    // Stellar source setup
    let stellar_rpc = &args.source_rpc;
    let network_type = read_stellar_network_type(&args.config, src)?;
    let stellar_client = crate::stellar::StellarClient::new(stellar_rpc, &network_type)?;
    let stellar_example = read_stellar_contract_address(&args.config, src, "AxelarExample")?;
    let stellar_gateway = read_stellar_contract_address(&args.config, src, "AxelarGateway")?;
    let stellar_xlm = read_stellar_token_address(&args.config, src)?;

    let main_wallet = load_stellar_main_wallet(args.private_key.as_deref())?;
    let use_friendbot = matches!(network_type.as_str(), "testnet" | "futurenet");
    if stellar_client
        .account_sequence(&main_wallet.address())
        .await?
        .is_none()
        && use_friendbot
    {
        ui::info("activating Stellar main wallet via Friendbot...");
        stellar_client
            .friendbot_fund(&main_wallet.address())
            .await?;
    }

    // Solana destination = the Solana memo program. The memo program decodes
    // an `ExecutablePayload`: a Borsh-shaped struct carrying the memo bytes
    // plus the `counter` PDA as a writable account. axe's existing
    // `evm_sender::make_executable_payload` produces exactly this shape, so
    // we reuse it and pass it through as the payload override (otherwise
    // stellar_sender's default EVM-ABI payload causes a Borsh deserialize
    // error on the destination side).
    let memo_program = evm_sender::memo_program_id(args.network);
    let destination_address = memo_program.to_string();
    ui::address("Solana memo program", &destination_address);

    let (counter_pda, _) =
        solana_sdk::pubkey::Pubkey::find_program_address(&[b"counter"], &memo_program);
    let user_payload: Option<Vec<u8>> = match &args.payload {
        Some(hex_str) => Some(hex::decode(hex_str.strip_prefix("0x").unwrap_or(hex_str))?),
        None => None,
    };
    let payload_override: Option<Vec<u8>> = Some(evm_sender::make_executable_payload(
        &user_payload,
        &counter_pda,
    ));

    let num_keys = usize::try_from(num_txs)?;
    let gas_stroops = stellar_sender::parse_gas_stroops(args.gas_value.as_deref())?;
    ui::info(&format!("deriving {num_keys} Stellar keys..."));
    let main_seed = main_wallet.signing_key.to_bytes();
    let wallets = stellar_sender::derive_wallets(&main_seed, num_keys)?;
    let mainnet_starting_balance = stellar_sender::mainnet_per_key_balance_stroops(gas_stroops, 1);
    stellar_sender::ensure_funded(
        &stellar_client,
        &wallets,
        use_friendbot,
        &main_wallet,
        mainnet_starting_balance,
    )
    .await?;

    let test_start = Instant::now();
    let mut report = stellar_sender::run_burst(stellar_sender::BurstRequest {
        client: stellar_client.clone(),
        wallets: wallets.clone(),
        example_contract: stellar_example.clone(),
        gateway_contract: stellar_gateway,
        destination_chain: args.destination_axelar_id.clone(),
        destination_address: destination_address.clone(),
        payload_override,
        run: RunIdentity::from_sizing(&args, sizing),
        gas_token_contract: stellar_xlm,
        gas_amount: gas_stroops,
    })
    .await?;
    report.destination_address = destination_address.clone();

    gmp_verification::attach_batch(
        &args,
        gmp_verification::SolanaGmpTarget {
            address: destination_address.to_string(),
            rpc_url: solana_rpc.to_string(),
            source_type: verify::SourceChainType::Stellar,
        },
        &mut report,
    )
    .await?;

    finish_report(&mut report, test_start)
}

pub(super) async fn run_sol_to_stellar(
    args: LoadTestArgs,
    _run_start: Instant,
    sizing: RunSizing,
) -> Result<()> {
    sizing.require_burst("GMP sol-to-stellar")?;
    let src = &args.source_chain;
    let dest = &args.destination_chain;

    validate_solana_rpc(&args.source_rpc).await?;

    ui::kv("source", src);
    ui::kv("destination", dest);
    ui::kv("protocol", "GMP (Solana → Stellar AxelarExample)");

    let stellar_rpc = &args.destination_rpc;
    let stellar_network_type = read_stellar_network_type(&args.config, dest)?;
    let stellar_gateway_addr = read_stellar_contract_address(&args.config, dest, "AxelarGateway")?;
    let stellar_example_addr = read_stellar_contract_address(&args.config, dest, "AxelarExample")?;
    ui::address("Stellar AxelarGateway", &stellar_gateway_addr);
    ui::address("Stellar AxelarExample", &stellar_example_addr);

    let test_start = Instant::now();
    let mut report = sol_sender::run_load_test_with_metrics(
        &args,
        sizing.require_burst("GMP sol-to-stellar")?,
        &stellar_example_addr,
        true, // evm_destination=true means use EVM-style payload encoding;
              // Stellar AxelarExample.execute also takes raw bytes so this
              // works for our purposes.
    )
    .await?;

    // Build a deterministic dummy ed25519 pk from the Solana keypair as the
    // Stellar simulate-only source account.
    let signer_pk: [u8; 32] = {
        let kp = crate::solana::load_keypair(args.keypair.as_deref())?;
        let pk = solana_sdk::signer::Signer::pubkey(&kp);
        let mut out = [0u8; 32];
        out.copy_from_slice(pk.as_ref());
        out
    };

    gmp_verification::attach_batch(
        &args,
        gmp_verification::StellarGmpTarget {
            contract: stellar_example_addr.to_string(),
            rpc_url: stellar_rpc.to_string(),
            network_type: stellar_network_type.to_string(),
            gateway_contract: stellar_gateway_addr.to_string(),
            signer_pk,
            source_type: verify::SourceChainType::Svm,
        },
        &mut report,
    )
    .await?;

    finish_report(&mut report, test_start)
}

// ===========================================================================
// Sui (Move) sources and destinations — GMP
// ===========================================================================

async fn prepare_sui_gmp_source(
    args: &LoadTestArgs,
) -> Result<(
    crate::sui::SuiClient,
    crate::sui::SuiWallet,
    crate::sui::SuiContractsConfig,
)> {
    let (configured_rpc, contracts) =
        crate::sui::read_sui_chain_config(&args.config, &args.source_chain)?;
    let rpc_url = if args.source_rpc.is_empty() {
        configured_rpc
    } else {
        args.source_rpc.clone()
    };
    let client = crate::sui::SuiClient::new(&rpc_url);
    let chain_id = client
        .get_chain_identifier()
        .await
        .unwrap_or_else(|_| "?".to_string());
    ui::kv("Sui chain id", &chain_id);

    let wallet = load_sui_main_wallet()?;
    ui::kv("Sui wallet", &wallet.address_hex());
    let balance = client.get_balance(&wallet.address).await?;
    ui::kv(
        "Sui balance",
        &format!("{balance} mist ({:.4} SUI)", balance as f64 / 1e9),
    );
    let minimum_balance = super::gmp_sui_source::DEFAULT_GAS_VALUE
        .get()
        .saturating_add(super::gmp_sui_source::DEFAULT_GAS_BUDGET.get());
    if balance < minimum_balance {
        eyre::bail!(
            "Sui wallet {} has insufficient SUI: {balance} mist. Need ≥ {} mist. \
             Get testnet SUI from `https://faucet.sui.io/?address={}`",
            wallet.address_hex(),
            minimum_balance,
            wallet.address_hex(),
        );
    }
    Ok((client, wallet, contracts))
}

/// Sui source -> any EVM destination, GMP only (sequential burst).
pub(super) async fn run_sui_to_evm(
    args: LoadTestArgs,
    _run_start: Instant,
    sizing: RunSizing,
) -> Result<()> {
    let num_txs = sizing.require_burst("GMP sui-to-evm")?;
    let src = &args.source_chain;
    let dest = &args.destination_chain;
    let cfg = ChainsConfig::load(&args.config)?;

    let evm_rpc_url = args.destination_rpc.clone();
    validate_evm_rpc(&evm_rpc_url).await?;

    // A legacy (consensus) EVM destination has no Cosmos Gateway and is verified
    // on its on-chain gateway instead, so it's exempt from this check.
    let (_, dst_legacy) =
        verify::classify_route(&cfg, &args.source_axelar_id, &args.destination_axelar_id);
    if !dst_legacy && cfg.axelar.contract_address("Gateway", dest).is_err() {
        eyre::bail!(
            "destination chain '{dest}' has no Cosmos Gateway in the config — verification would fail."
        );
    }

    ui::kv("source", src);
    ui::kv("destination", dest);
    ui::kv("protocol", "GMP (Example.gmp.send_call)");

    let (sui_client, main_wallet, sui_contracts) = prepare_sui_gmp_source(&args).await?;
    let missing_key_error =
        format!("no SenderReceiver cached for '{dest}' and no EVM private key available.");
    let (sender_receiver_addr, provider, gateway_addr) =
        prepare_cached_evm_destination(&args, &cfg, &missing_key_error).await?;
    let destination_address = format!("{sender_receiver_addr}");

    let gas_value = super::gmp_sui_source::parse_gas_value(args.gas_value.as_deref())?;

    let payload_bytes: Vec<u8> =
        match super::gmp_sui_source::parse_payload(args.payload.as_deref())? {
            Some(payload) => payload,
            None => {
                let s: String = format!(
                    "Hello from Sui axe load-test {}",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0)
                );
                s.abi_encode()
            }
        };

    let num_txs = usize::try_from(num_txs)?;

    let test_start = Instant::now();
    // Sui's `Example::gmp::send_call` takes the destination address as a
    // String (e.g. `"0xd7f2…"`), not raw bytes. Match what `sui/gmp.js` does.
    let dest_addr_str = format!("{sender_receiver_addr}");
    let burst = super::gmp_sui_source::run_sequential(
        super::gmp_sui_source::GmpSuiSubmitter {
            client: sui_client,
            wallet: main_wallet,
            contracts: sui_contracts,
            destination_chain: args.destination_axelar_id.clone(),
            destination_address: dest_addr_str,
            gas_value,
            gas_budget: super::gmp_sui_source::DEFAULT_GAS_BUDGET,
        },
        (0..num_txs)
            .map(|_| super::gmp_sui_source::GmpSuiSubmitJob {
                payload: payload_bytes.clone(),
            })
            .collect(),
    )
    .await?;

    let mut report = LoadTestReport::from_transactions(
        ReportInput {
            run: RunIdentity::from_sizing(&args, sizing),
            destination_address: destination_address.clone(),
            num_txs: burst.total_submitted,
            num_keys: 1,
            total_submitted: burst.total_submitted,
            test_duration_secs: burst.test_duration_secs,
            compute_unit_summary: ComputeUnitSummary::Omit,
        },
        burst.metrics,
    );

    gmp_verification::attach_batch(
        &args,
        gmp_verification::EvmGmpTarget {
            address: destination_address.to_string(),
            gateway_addr,
            provider: &provider,
            source_type: verify::SourceChainType::Sui,
            legacy: false,
        },
        &mut report,
    )
    .await?;

    finish_report(&mut report, test_start)
}

// ===========================================================================
// Sui -> Solana GMP
// ===========================================================================

/// Sui source -> Solana destination, GMP only (sequential burst).
///
/// Mirrors `run_sui_to_evm`: same Sui-side PTB (`example::gmp::send_call`),
/// but targets the Solana memo program and uses `verify_onchain_solana` for
/// the destination-side polling. Payload format matches what `run_evm_to_sol`
/// sends — the memo program accepts the same ABI-string framing.
pub(super) async fn run_sui_to_sol(
    args: LoadTestArgs,
    _run_start: Instant,
    sizing: RunSizing,
) -> Result<()> {
    let num_txs = sizing.require_burst("GMP sui-to-sol")?;
    let src = &args.source_chain;
    let dest = &args.destination_chain;
    let cfg = ChainsConfig::load(&args.config)?;

    let solana_rpc = args.destination_rpc.clone();
    validate_solana_rpc(&solana_rpc).await?;

    // A legacy (consensus) EVM destination has no Cosmos Gateway and is verified
    // on its on-chain gateway instead, so it's exempt from this check.
    let (_, dst_legacy) =
        verify::classify_route(&cfg, &args.source_axelar_id, &args.destination_axelar_id);
    if !dst_legacy && cfg.axelar.contract_address("Gateway", dest).is_err() {
        eyre::bail!(
            "destination chain '{dest}' has no Cosmos Gateway in the config — verification would fail."
        );
    }

    ui::kv("source", src);
    ui::kv("destination", dest);
    ui::kv("protocol", "GMP (Sui → Solana via memo)");

    let (sui_rpc, sui_contracts) = crate::sui::read_sui_chain_config(&args.config, src)?;
    let sui_rpc = if args.source_rpc.is_empty() {
        sui_rpc
    } else {
        args.source_rpc.clone()
    };
    let sui_client = crate::sui::SuiClient::new(&sui_rpc);
    let chain_id = sui_client
        .get_chain_identifier()
        .await
        .unwrap_or_else(|_| "?".to_string());
    ui::kv("Sui chain id", &chain_id);

    let main_wallet = load_sui_main_wallet()?;
    ui::kv("Sui wallet", &main_wallet.address_hex());
    let bal = sui_client.get_balance(&main_wallet.address).await?;
    let sui_amount = bal as f64 / 1e9;
    ui::kv("Sui balance", &format!("{bal} mist ({sui_amount:.4} SUI)"));
    let minimum_balance = super::gmp_sui_source::DEFAULT_GAS_VALUE
        .get()
        .saturating_add(super::gmp_sui_source::DEFAULT_GAS_BUDGET.get());
    if bal < minimum_balance {
        eyre::bail!(
            "Sui wallet {} has insufficient SUI: {bal} mist. Need ≥ {} mist.",
            main_wallet.address_hex(),
            minimum_balance,
        );
    }

    // Destination on Solana: the memo program (resolved from the target network).
    // Same target `run_evm_to_sol` uses, so the receive-side flow on Solana
    // is identical (memo program acks via gateway).
    let destination_address = evm_sender::memo_program_id(args.network).to_string();
    ui::kv("destination program", &destination_address);

    // The Solana axelar gateway needs the executable-payload framing
    // (1-byte scheme prefix + ABI-encoded SolanaGatewayPayload with the
    // counter PDA as a writable account). Building the bytes here matches
    // what `run_evm_to_sol` sends so the Solana relayer can execute it.
    let memo_program_id = evm_sender::memo_program_id(args.network);
    let (counter_pda, _) =
        solana_sdk::pubkey::Pubkey::find_program_address(&[b"counter"], &memo_program_id);

    let gas_value = super::gmp_sui_source::parse_gas_value(args.gas_value.as_deref())?;

    // `--payload`, if provided, overrides the memo body inside the
    // executable-payload wrapper. Otherwise `make_executable_payload`
    // picks a random "hello from axe load test ..." string.
    let inner_payload = super::gmp_sui_source::parse_payload(args.payload.as_deref())?;

    let num_txs = usize::try_from(num_txs)?;

    let test_start = Instant::now();
    let burst = super::gmp_sui_source::run_sequential(
        super::gmp_sui_source::GmpSuiSubmitter {
            client: sui_client,
            wallet: main_wallet,
            contracts: sui_contracts,
            destination_chain: args.destination_axelar_id.clone(),
            destination_address: destination_address.clone(),
            gas_value,
            gas_budget: super::gmp_sui_source::DEFAULT_GAS_BUDGET,
        },
        (0..num_txs)
            .map(|_| super::gmp_sui_source::GmpSuiSubmitJob {
                payload: evm_sender::make_executable_payload(&inner_payload, &counter_pda),
            })
            .collect(),
    )
    .await?;

    let mut report = LoadTestReport::from_transactions(
        ReportInput {
            run: RunIdentity::from_sizing(&args, sizing),
            destination_address: destination_address.clone(),
            num_txs: burst.total_submitted,
            num_keys: 1,
            total_submitted: burst.total_submitted,
            test_duration_secs: burst.test_duration_secs,
            compute_unit_summary: ComputeUnitSummary::Omit,
        },
        burst.metrics,
    );

    gmp_verification::attach_batch(
        &args,
        gmp_verification::SolanaGmpTarget {
            address: destination_address.to_string(),
            rpc_url: solana_rpc.to_string(),
            source_type: verify::SourceChainType::Sui,
        },
        &mut report,
    )
    .await?;

    finish_report(&mut report, test_start)
}
