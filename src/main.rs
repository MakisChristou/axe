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
                destination_chain,
                source_chain,
                private_key,
                time,
                delay,
                keypair,
                mnemonic,
                addresses_to_derive,
                contention_mode,
                payload,
                output_dir,
                skip_gmp_verify,
            } => {
                commands::load_test::run(commands::load_test::LoadTestArgs {
                    config,
                    destination_chain,
                    source_chain,
                    private_key,
                    time,
                    delay,
                    keypair,
                    mnemonic,
                    addresses_to_derive,
                    contention_mode,
                    payload,
                    output_dir,
                    skip_gmp_verify,
                })
                .await
            }
        },
    }
}
