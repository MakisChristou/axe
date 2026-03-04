mod cli;
mod commands;
mod cosmos;
mod evm;
mod preflight;
mod solana;
mod state;
mod steps;
pub mod ui;
mod utils;

use clap::Parser;
use eyre::Result;

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv_override().ok();

    let cli = cli::Cli::parse();

    match cli.command {
        cli::Commands::Init => commands::init::run().await,
        cli::Commands::Status { axelar_id } => commands::status::run(axelar_id),
        cli::Commands::Deploy {
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
        cli::Commands::Reset { axelar_id } => commands::reset::run(axelar_id),
        cli::Commands::Test { subcommand } => match subcommand {
            cli::TestCommands::Gmp { axelar_id } => commands::test_gmp::run(axelar_id).await,
            cli::TestCommands::Its { axelar_id } => commands::test_its::run(axelar_id).await,
            cli::TestCommands::LoadTest {
                config,
                test_type,
                time,
                delay,
                destination_chain,
                source_chain,
                private_key,
                keypair,
                contention_mode,
                payload,
                output_dir,
            } => {
                let resolved = commands::load_test::resolve_from_config(
                    &config,
                    test_type,
                    source_chain,
                    destination_chain,
                    private_key,
                )?;

                commands::load_test::run(commands::load_test::LoadTestArgs {
                    config,
                    test_type: resolved.test_type,
                    destination_chain: resolved.destination_chain,
                    source_chain: resolved.source_chain,
                    solana_rpc: resolved.solana_rpc,
                    private_key: resolved.private_key,
                    time,
                    delay,
                    keypair,
                    contention_mode,
                    payload,
                    output_dir,
                })
                .await
            }
        },
    }
}
