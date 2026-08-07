mod cli;
mod commands;
mod config;
mod config_source;
mod cosmos;
mod error;
mod evm;
mod gmp_api;
mod hyperliquid;
mod preflight;
mod retry;
mod solana;
mod state;
mod stellar;
mod steps;
mod sui;
mod timing;
mod types;
pub mod ui;
mod utils;
mod xrpl;

use clap::Parser;
use eyre::Result;

async fn run_deploy(command: cli::DeployCommands) -> Result<()> {
    match command {
        cli::DeployCommands::Init => commands::init::run().await,
        cli::DeployCommands::Status { axelar_id } => commands::status::run(axelar_id).await,
        cli::DeployCommands::Run {
            axelar_id,
            private_key,
            artifact_path,
            salt,
            proxy_artifact_path,
        } => {
            commands::deploy::run(
                axelar_id,
                private_key,
                artifact_path,
                salt,
                proxy_artifact_path,
            )
            .await
        }
        cli::DeployCommands::Reset { axelar_id } => commands::reset::run(axelar_id).await,
    }
}

async fn run_decode(
    command: cli::DecodeCommands,
    global_network: Option<types::Network>,
) -> Result<()> {
    match command {
        cli::DecodeCommands::Calldata { hex } => commands::decode::run(&hex.join("")),
        cli::DecodeCommands::Tx {
            txid,
            config,
            chain,
        } => commands::decode_tx::run(&txid, config.as_deref(), chain.as_deref()).await,
        cli::DecodeCommands::SolActivity {
            program,
            network,
            limit,
            json,
        } => commands::decode_sol_activity::run(program, network, limit, json).await,
        cli::DecodeCommands::EvmActivity {
            contract,
            network,
            chain,
            limit,
            json,
        } => {
            let network = cli::network_or_default(network, global_network)?;
            commands::decode_evm_activity::run(contract, network, chain, limit, json).await
        }
    }
}

async fn resolve_test_config(
    global_network: Option<types::Network>,
    config: Option<std::path::PathBuf>,
) -> Result<(types::Network, std::path::PathBuf)> {
    let network = cli::resolve_network(global_network, config.as_deref())?;
    let config = match config {
        Some(path) => path,
        None => config_source::resolve(network, None).await?.into_path(),
    };
    Ok((network, config))
}

async fn run_gmp_test(
    command: cli::TestCommands,
    global_network: Option<types::Network>,
) -> Result<()> {
    let cli::TestCommands::Gmp {
        axelar_id,
        config,
        source_chain,
        destination_chain,
        destination_address,
        mnemonic,
    } = command
    else {
        unreachable!("run_gmp_test called with another test command")
    };
    if config.is_some() || source_chain.is_some() || destination_chain.is_some() {
        let (network, config) = resolve_test_config(global_network, config).await?;
        commands::test_gmp::run_config(
            config,
            network,
            source_chain,
            destination_chain,
            destination_address,
            mnemonic,
        )
        .await
    } else {
        commands::test_gmp::run(axelar_id).await
    }
}

async fn run_its_test(
    command: cli::TestCommands,
    global_network: Option<types::Network>,
) -> Result<()> {
    let cli::TestCommands::Its {
        axelar_id,
        config,
        source_chain,
        destination_chain,
        mnemonic,
        evm_private_key,
        amount,
        gas_value,
        fresh_token,
    } = command
    else {
        unreachable!("run_its_test called with another test command")
    };
    if config.is_some() || source_chain.is_some() || destination_chain.is_some() {
        let (network, config) = resolve_test_config(global_network, config).await?;
        commands::test_its::run_config(commands::test_its::ConfigArgs {
            config,
            network,
            source_chain,
            destination_chain,
            mnemonic_override: mnemonic,
            evm_private_key_override: evm_private_key,
            amount,
            gas_value,
            fresh_token,
        })
        .await
    } else {
        commands::test_its::run(axelar_id).await
    }
}

async fn run_load_test(
    command: cli::TestCommands,
    global_network: Option<types::Network>,
) -> Result<()> {
    let cli::TestCommands::LoadTest {
        config,
        test_type,
        num_txs,
        destination_chain,
        source_chain,
        private_key,
        keypair,
        source_rpc,
        destination_rpc,
        payload,
        protocol,
        gas_value,
        token_id,
        coin_type,
        tps,
        duration_secs,
        key_cycle,
        extra_accounts,
    } = command
    else {
        unreachable!("run_load_test called with another test command")
    };
    let (network, config) = resolve_test_config(global_network, config).await?;
    let resolved = commands::load_test::resolve_from_config(
        &config,
        test_type,
        source_chain,
        destination_chain,
        private_key,
        source_rpc,
        destination_rpc,
    )
    .await?;
    commands::load_test::run(commands::load_test::LoadTestArgs {
        config,
        network,
        test_type: resolved.test_type,
        protocol,
        destination_chain: resolved.destination_chain,
        source_chain: resolved.source_chain,
        source_axelar_id: resolved.source_axelar_id,
        destination_axelar_id: resolved.destination_axelar_id,
        source_rpc: resolved.source_rpc,
        destination_rpc: resolved.destination_rpc,
        private_key: resolved.private_key,
        num_txs,
        keypair,
        payload,
        gas_value,
        token_id,
        coin_type,
        tps,
        duration_secs,
        key_cycle,
        extra_accounts,
    })
    .await
}

async fn run_test(
    command: cli::TestCommands,
    global_network: Option<types::Network>,
) -> Result<()> {
    match command {
        command @ cli::TestCommands::Gmp { .. } => run_gmp_test(command, global_network).await,
        command @ cli::TestCommands::Its { .. } => run_its_test(command, global_network).await,
        command @ cli::TestCommands::LoadTest { .. } => {
            run_load_test(command, global_network).await
        }
        cli::TestCommands::ExpressExecution {
            chains,
            source_tx,
            config,
            recent,
            timeout_secs,
        } => {
            let network = cli::resolve_network(global_network, config.as_deref())?;
            commands::test_express::run_config(network, chains, source_tx, recent, timeout_secs)
                .await
        }
    }
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    dotenvy::dotenv_override().ok();

    // Errors are printed through `ui::scrub_urls` so RPC URLs (which can come
    // from private/keyed secrets) never reach stderr — upstream-crate errors
    // (reqwest, alloy transports) embed the full request URL in their Display.
    match run_cli().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Error: {}", ui::scrub_urls(&format!("{error:?}")));
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run_cli() -> Result<()> {
    let cli = cli::Cli::parse();

    match cli.command {
        cli::Commands::Deploy { subcommand } => run_deploy(subcommand).await,
        cli::Commands::Decode { subcommand } => run_decode(subcommand, cli.network).await,
        cli::Commands::Info { subcommand } => match subcommand {
            cli::InfoCommands::Block {
                number,
                network,
                at_time,
            } => commands::info_block::run(network, number, at_time).await,
        },
        cli::Commands::Verifiers {
            network,
            chain,
            json,
        } => commands::verifiers::run(network, chain, json).await,
        cli::Commands::ItsOwnership { network, json } => {
            let network = cli::network_or_default(network, cli.network)?;
            commands::its_ownership::run(network, json).await
        }
        cli::Commands::CheckBalances { network } => {
            let network = cli::network_or_default(network, cli.network)?;
            commands::check_balances::run(network).await
        }
        cli::Commands::VerifierVotes {
            network,
            chain,
            verifier,
            limit,
            json,
        } => commands::verifier_votes::run(network, chain, verifier, limit, json).await,
        cli::Commands::Propose(args) => commands::propose::run(args).await,
        cli::Commands::Test { subcommand } => run_test(subcommand, cli.network).await,
    }
}
