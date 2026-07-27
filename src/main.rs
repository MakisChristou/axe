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

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv_override().ok();

    let cli = cli::Cli::parse();

    match cli.command {
        cli::Commands::Deploy { subcommand } => match subcommand {
            cli::DeployCommands::Init => commands::init::run().await,
            cli::DeployCommands::Status { axelar_id } => commands::status::run(axelar_id),
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
            cli::DeployCommands::Reset { axelar_id } => commands::reset::run(axelar_id),
        },
        cli::Commands::Decode { subcommand } => match subcommand {
            cli::DecodeCommands::Calldata { hex } => {
                let joined = hex.join("");
                commands::decode::run(&joined)
            }
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
                let network = cli::network_or_default(network, cli.network)?;
                commands::decode_evm_activity::run(contract, network, chain, limit, json).await
            }
        },
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
        cli::Commands::Test { subcommand } => match subcommand {
            cli::TestCommands::Gmp {
                axelar_id,
                config,
                source_chain,
                destination_chain,
                destination_address,
                mnemonic,
            } => {
                // Config mode when --config or the chain pair is given;
                // legacy state-file mode otherwise.
                if config.is_some() || source_chain.is_some() || destination_chain.is_some() {
                    let network = cli::resolve_network(cli.network, config.as_deref())?;
                    let config = match config {
                        Some(path) => path,
                        None => config_source::resolve(network, None).await?.into_path(),
                    };
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
            cli::TestCommands::Its {
                axelar_id,
                config,
                source_chain,
                destination_chain,
                mnemonic,
                evm_private_key,
                amount,
                gas_value,
                fresh_token,
            } => {
                // Config mode when --config or the chain pair is given;
                // legacy state-file mode otherwise.
                if config.is_some() || source_chain.is_some() || destination_chain.is_some() {
                    let network = cli::resolve_network(cli.network, config.as_deref())?;
                    let config = match config {
                        Some(path) => path,
                        None => config_source::resolve(network, None).await?.into_path(),
                    };
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
            cli::TestCommands::ExpressExecution {
                chains,
                source_tx,
                config,
                recent,
                timeout_secs,
            } => {
                let network = cli::resolve_network(cli.network, config.as_deref())?;
                commands::test_express::run_config(
                    config,
                    network,
                    chains,
                    source_tx,
                    recent,
                    timeout_secs,
                )
                .await
            }
            cli::TestCommands::LoadTest {
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
            } => {
                let network = cli::resolve_network(cli.network, config.as_deref())?;
                let config = match config {
                    Some(path) => path,
                    None => config_source::resolve(network, None).await?.into_path(),
                };
                let resolved = commands::load_test::resolve_from_config(
                    &config,
                    test_type,
                    source_chain,
                    destination_chain,
                    private_key,
                    source_rpc,
                    destination_rpc,
                )?;

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
        },
    }
}
