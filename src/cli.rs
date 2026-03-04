use std::path::PathBuf;

use clap::{Parser, Subcommand};
use eyre::Result;

use crate::commands::load_test::sol_to_evm::ContentionMode;
use crate::commands::load_test::TestType;

#[derive(Parser)]
#[command(name = "axelar-evm-deployer")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Initialize a new chain deployment (reads all config from .env / environment)
    Init,

    /// Show deployment progress
    Status {
        #[arg(long)]
        axelar_id: Option<String>,
    },

    /// Run all pending deployment steps
    Deploy {
        #[arg(long)]
        axelar_id: Option<String>,
        /// Private key override (auto-resolved per step by default)
        #[arg(long)]
        private_key: Option<String>,
        /// Path to implementation artifact JSON (auto-resolved by default)
        #[arg(long)]
        artifact_path: Option<String>,
        /// Salt for create2 deployments (read from state by default)
        #[arg(long)]
        salt: Option<String>,
        /// Path to proxy artifact JSON (auto-resolved by default)
        #[arg(long)]
        proxy_artifact_path: Option<String>,
    },

    /// Reset all steps to pending and remove all changes from target JSON
    Reset {
        #[arg(long)]
        axelar_id: Option<String>,
    },

    /// Test GMP or ITS functionality
    Test {
        #[command(subcommand)]
        subcommand: TestCommands,
    },
}

#[derive(Subcommand)]
pub enum TestCommands {
    /// Test GMP source flow: deploy SenderReceiver, send a loopback callContract
    Gmp {
        #[arg(long)]
        axelar_id: Option<String>,
    },

    /// Test ITS: deploy interchain token on source, deploy remotely to flow via hub
    Its {
        #[arg(long)]
        axelar_id: Option<String>,
    },

    /// Cross-chain load test (auto-detects chains, RPCs, and test type from config)
    LoadTest {
        /// Path to chains config JSON (e.g. devnet-amplifier.json, testnet.json, mainnet.json)
        #[arg(long, env = "CHAINS_CONFIG")]
        config: PathBuf,

        /// Test duration in seconds
        #[arg(long, default_value = "10")]
        time: u64,

        /// Delay between transactions in milliseconds
        #[arg(long, default_value = "1000")]
        delay: u64,

        /// Load test type (auto-detected from source/destination chain types if omitted)
        #[arg(long, value_enum)]
        test_type: Option<TestType>,

        /// Override destination chain axelar ID (auto-detected from config)
        #[arg(long)]
        destination_chain: Option<String>,

        /// Override source chain axelar ID (auto-detected from config)
        #[arg(long)]
        source_chain: Option<String>,

        /// EVM private key for deploying SenderReceiver on destination chain
        #[arg(long, env = "EVM_PRIVATE_KEY")]
        private_key: Option<String>,

        /// Path to Solana keypair JSON file
        #[arg(long, env = "SOLANA_PRIVATE_KEY")]
        keypair: Option<String>,

        /// Contention testing mode
        #[arg(long, value_enum, default_value = "none")]
        contention_mode: ContentionMode,

        /// Hex-encoded payload to send (default: random test message)
        #[arg(long)]
        payload: Option<String>,

        /// Output directory for all results
        #[arg(long, default_value = "output")]
        output_dir: PathBuf,
    },
}

pub fn resolve_axelar_id(opt: Option<String>) -> Result<String> {
    opt.or_else(|| std::env::var("CHAIN").ok())
        .ok_or_else(|| eyre::eyre!("--axelar-id not provided and CHAIN env var not set"))
}
