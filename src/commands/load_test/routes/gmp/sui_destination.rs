//! GMP routes with Sui destinations.

use std::time::Instant;

use alloy::{providers::ProviderBuilder, signers::local::PrivateKeySigner};
use eyre::Result;

use super::super::gmp_payload::GmpPayloadEncoding;
use super::super::metrics::RunIdentity;
use super::super::run_sizing::RunSizing;
use super::super::{
    LoadTestArgs, check_evm_balance, ensure_sender_receiver, finalize_sui_dest_run,
    load_stellar_main_wallet, read_cache, read_stellar_contract_address, read_stellar_network_type,
    read_stellar_token_address, sui_dest_lookup, validate_evm_rpc, validate_solana_rpc,
};
use super::super::{evm_sender, sol_sender, stellar_sender, verify};
use crate::config::ChainContract;
use crate::config::ChainsConfig;
use crate::stellar::StellarClient;
use crate::ui;

/// EVM source -> Sui destination, GMP only.
pub(in crate::commands::load_test) async fn run_evm_to_sui(
    args: LoadTestArgs,
    sizing: RunSizing,
) -> Result<()> {
    sizing.require_burst("GMP evm-to-sui")?;
    let src = &args.source_chain;
    let dest = &args.destination_chain;
    let cfg = ChainsConfig::load(&args.config).await?;

    ui::kv("source", src);
    ui::kv("destination", dest);
    ui::kv("protocol", "GMP (EVM SenderReceiver → Sui memo)");

    let evm_rpc_url = args.source_rpc.to_string();
    validate_evm_rpc(&evm_rpc_url).await?;

    let signer = args
        .private_key
        .as_ref()
        .ok_or_else(|| {
            eyre::eyre!("EVM private key required. Set EVM_PRIVATE_KEY or use --private-key")
        })?
        .parse::<PrivateKeySigner>()?;
    let read_provider = ProviderBuilder::new().connect_http(evm_rpc_url.parse()?);
    check_evm_balance(&read_provider, signer.address()).await?;

    // Destination = Sui's Example.objects.GmpChannelId. The EVM
    // ContractCall payload is delivered to that channel; on Sui, the
    // executor calls the channel's execute path, gateway emits
    // MessageExecuted, and we observe via events.
    let (sui_channel, sui_rpc) =
        sui_dest_lookup(&args.config, dest, Some(&args.destination_rpc)).await?;
    ui::address("Sui GmpChannel (destination)", &sui_channel);

    let evm_gateway_addr = cfg
        .chains
        .get(src.as_ref())
        .ok_or_else(|| eyre::eyre!("chain '{}' not found in config", src))?
        .contract_address(ChainContract::AxelarGateway, src)?
        .parse()?;
    let evm_gas_service_addr = cfg
        .chains
        .get(src.as_ref())
        .ok_or_else(|| eyre::eyre!("chain '{}' not found in config", src))?
        .contract_address(ChainContract::AxelarGasService, src)?
        .parse()?;
    ui::address("EVM gateway", &format!("{evm_gateway_addr}"));

    let cache = read_cache(src).await;
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

    let main_key: [u8; 32] = signer.to_bytes().into();
    let test_start = Instant::now();
    let mut report = evm_sender::run_load_test_with_metrics(
        &args,
        sizing.require_burst("GMP evm-to-sui")?,
        sender_receiver_addr,
        &main_key,
        &evm_rpc_url,
        &sui_channel,
        GmpPayloadEncoding::AbiString,
    )
    .await?;

    finalize_sui_dest_run(
        &args,
        &mut report,
        &sui_channel,
        &sui_rpc,
        verify::SourceChainType::Evm,
        test_start,
    )
    .await
}

/// Solana source -> Sui destination, GMP only.
pub(in crate::commands::load_test) async fn run_sol_to_sui(
    args: LoadTestArgs,
    sizing: RunSizing,
) -> Result<()> {
    sizing.require_burst("GMP sol-to-sui")?;
    let src = &args.source_chain;
    let dest = &args.destination_chain;

    let solana_rpc = args.source_rpc.to_string();
    validate_solana_rpc(&solana_rpc).await?;

    ui::kv("source", src);
    ui::kv("destination", dest);
    ui::kv("protocol", "GMP (Solana → Sui via memo example)");

    let (sui_channel, sui_rpc) =
        sui_dest_lookup(&args.config, dest, Some(&args.destination_rpc)).await?;
    ui::address("Sui GmpChannel (destination)", &sui_channel);

    let test_start = Instant::now();
    let mut report = sol_sender::run_load_test_with_metrics(
        &args,
        sizing.require_burst("GMP sol-to-sui")?,
        &sui_channel,
        GmpPayloadEncoding::AbiString,
    )
    .await?;
    report.destination_address = sui_channel.clone();

    finalize_sui_dest_run(
        &args,
        &mut report,
        &sui_channel,
        &sui_rpc,
        verify::SourceChainType::Svm,
        test_start,
    )
    .await
}

/// Stellar source -> Sui destination, GMP only.
pub(in crate::commands::load_test) async fn run_stellar_to_sui(
    args: LoadTestArgs,
    sizing: RunSizing,
) -> Result<()> {
    let num_txs = sizing.require_burst("GMP stellar-to-sui")?;
    let src = &args.source_chain;
    let dest = &args.destination_chain;

    ui::kv("source", src);
    ui::kv("destination", dest);
    ui::kv("protocol", "GMP (Stellar AxelarExample.send → Sui memo)");

    let (sui_channel, sui_rpc) =
        sui_dest_lookup(&args.config, dest, Some(&args.destination_rpc)).await?;
    ui::address("Sui GmpChannel (destination)", &sui_channel);

    let stellar_rpc = &args.source_rpc;
    let network_type = read_stellar_network_type(&args.config, src).await?;
    let stellar_client = StellarClient::new(stellar_rpc, &network_type)?;
    let stellar_example =
        read_stellar_contract_address(&args.config, src, ChainContract::AxelarExample).await?;
    let stellar_gateway =
        read_stellar_contract_address(&args.config, src, ChainContract::AxelarGateway).await?;
    let stellar_xlm = read_stellar_token_address(&args.config, src).await?;

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

    let payload_override: Option<Vec<u8>> = match &args.payload {
        Some(hex_str) => Some(hex::decode(hex_str.strip_prefix("0x").unwrap_or(hex_str))?),
        None => None,
    };

    let num_keys = usize::try_from(num_txs)?;
    let gas_stroops = stellar_sender::parse_gas_stroops(args.gas_value.as_deref())?;
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
        destination_chain: args.destination_axelar_id.to_string(),
        destination_address: sui_channel.clone(),
        payload_override,
        run: RunIdentity::from_sizing(&args, sizing),
        gas_token_contract: stellar_xlm,
        gas_amount: gas_stroops,
    })
    .await?;
    report.destination_address = sui_channel.clone();

    finalize_sui_dest_run(
        &args,
        &mut report,
        &sui_channel,
        &sui_rpc,
        verify::SourceChainType::Stellar,
        test_start,
    )
    .await
}
