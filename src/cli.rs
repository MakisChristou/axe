use std::path::PathBuf;

use clap::{Parser, Subcommand};
use eyre::Result;

use crate::commands::load_test::{Protocol, TestType};
use crate::commands::propose::ProposeArgs;
use crate::types::Network;

#[derive(Parser)]
#[command(name = "axe")]
pub struct Cli {
    /// Axelar network to target (defaults to the config filename's network,
    /// else testnet)
    #[arg(long, global = true, env = "AXE_NETWORK", value_enum)]
    pub network: Option<Network>,

    #[command(subcommand)]
    pub command: Commands,
}

/// Pick the network for this invocation: explicit `--network`/`AXE_NETWORK`
/// wins, else the network named by the config filename, else testnet. A flag
/// that contradicts the config filename is a hard error — that's the runtime
/// replacement for the old compiled-network-vs-config guard.
pub fn resolve_network(flag: Option<Network>, config: Option<&std::path::Path>) -> Result<Network> {
    let from_config = config.and_then(crate::commands::load_test::detect_network_from_config);
    match (flag, from_config) {
        (Some(f), Some(c)) if f != c => eyre::bail!(
            "--network {f} contradicts the config file ({c}); pass a matching --config or drop one"
        ),
        (Some(f), _) => Ok(f),
        (None, Some(c)) => Ok(c),
        (None, None) => Ok(Network::Testnet),
    }
}

/// Resolve a command's own (optional) network arg against the global flag:
/// the command's arg wins, then `--network`/`AXE_NETWORK`, then testnet.
/// Contradicting values are a hard error.
pub fn network_or_default(arg: Option<Network>, global: Option<Network>) -> Result<Network> {
    match (arg, global) {
        (Some(a), Some(g)) if a != g => {
            eyre::bail!("network argument {a} contradicts --network {g}; drop one")
        }
        (Some(a), _) => Ok(a),
        (None, Some(g)) => Ok(g),
        (None, None) => Ok(Network::Testnet),
    }
}

#[derive(Subcommand)]
pub enum Commands {
    /// Deploy and manage chain deployments
    Deploy {
        #[command(subcommand)]
        subcommand: DeployCommands,
    },

    /// Test GMP or ITS functionality
    Test {
        #[command(subcommand)]
        subcommand: TestCommands,
    },

    /// Decode EVM calldata or full transactions
    Decode {
        #[command(subcommand)]
        subcommand: DecodeCommands,
    },

    /// Show active verifiers for a chain
    Verifiers {
        /// Axelar network (devnet-amplifier, stagenet, testnet, mainnet)
        network: Network,
        /// Chain axelar ID (e.g. solana, ethereum, avalanche-fuji)
        chain: String,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },

    /// Show ITS owner/operator addresses across a network
    ItsOwnership {
        /// Axelar network (defaults to --network / AXE_NETWORK, else testnet)
        network: Option<Network>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },

    /// Pre-flight check: verify each chain's wallet has the minimum native
    /// balance needed by the cron amplifier-routes load tests. Fails the
    /// process if any wallet is underfunded.
    CheckBalances {
        /// Axelar network (defaults to --network / AXE_NETWORK, else testnet)
        network: Option<Network>,
    },

    /// Show network info (e.g. block height + timestamp)
    Info {
        #[command(subcommand)]
        subcommand: InfoCommands,
    },

    /// Show recent votes cast by a single verifier on a given chain
    VerifierVotes {
        /// Axelar network (testnet, mainnet)
        network: Network,
        /// Chain axelar ID (e.g. solana, xrpl, hedera)
        chain: String,
        /// Verifier axelar1... address
        verifier: String,
        /// Maximum number of recent votes to show (default: 20)
        #[arg(long, default_value = "20")]
        limit: usize,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },

    /// Submit an AxelarServiceGovernance proposal to an edge chain's ASG
    Propose(ProposeArgs),
}

#[derive(Subcommand)]
pub enum DeployCommands {
    /// Initialize a new chain deployment (reads all config from .env / environment)
    Init,

    /// Show deployment progress
    Status {
        #[arg(long)]
        axelar_id: Option<String>,
    },

    /// Run all pending deployment steps
    Run {
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

    /// Deploy (or verify + reuse) the GMP SenderReceiver helper on an EVM
    /// chain. Checks the axe-tokens overlay and local cache first; a fresh
    /// deploy prints the overlay line to record so no run ever redeploys.
    SenderReceiver {
        /// Path to the chains-config JSON for the target network
        #[arg(long, env = "CHAINS_CONFIG")]
        config: PathBuf,
        /// Chain key in the config (e.g. base-sepolia)
        #[arg(long)]
        chain: String,
        /// RPC override (falls back to the chain's config rpc)
        #[arg(long)]
        rpc: Option<String>,
        /// EVM private key (hex)
        #[arg(long, env = "EVM_PRIVATE_KEY", hide_env_values = true)]
        private_key: String,
    },
}

#[derive(Subcommand)]
pub enum TestCommands {
    /// Test GMP: send a cross-chain message and relay through the full Amplifier pipeline
    Gmp {
        /// Chain axelar ID (legacy EVM-only mode, uses state file)
        #[arg(long)]
        axelar_id: Option<String>,

        /// Path to chains config JSON (config-based mode, supports EVM + Solana)
        #[arg(long, env = "CHAINS_CONFIG")]
        config: Option<PathBuf>,

        /// Source chain axelar ID
        #[arg(long)]
        source_chain: Option<String>,

        /// Destination chain axelar ID
        #[arg(long)]
        destination_chain: Option<String>,

        /// Destination contract address (required for sol→evm; defaults to the
        /// SVM memo program for sol→sol). For EVM destinations this should
        /// point at a deployed `SenderReceiver` so the test can call
        /// `execute(...)` and read back the stored message.
        #[arg(long)]
        destination_address: Option<String>,

        /// Cosmos mnemonic for relay transactions
        #[arg(long, env = "MNEMONIC")]
        mnemonic: Option<String>,
    },

    /// Test ITS: deploy interchain token on source, deploy remotely to a destination chain via hub
    Its {
        /// Chain axelar ID (legacy EVM-only mode, uses state file)
        #[arg(long)]
        axelar_id: Option<String>,

        /// Path to chains config JSON (config-based mode, supports Solana → EVM)
        #[arg(long, env = "CHAINS_CONFIG")]
        config: Option<PathBuf>,

        /// Source chain axelar ID (e.g. solana-devnet)
        #[arg(long)]
        source_chain: Option<String>,

        /// Destination chain axelar ID (e.g. avalanche-fuji)
        #[arg(long)]
        destination_chain: Option<String>,

        /// Cosmos mnemonic for relay transactions
        #[arg(long, env = "MNEMONIC")]
        mnemonic: Option<String>,

        /// EVM private key (used to derive the destination receiver address)
        #[arg(long, env = "EVM_PRIVATE_KEY")]
        evm_private_key: Option<String>,

        /// Amount of base units to transfer (default 1_000_000_000 = 1 token at 9 decimals)
        #[arg(long)]
        amount: Option<u64>,

        /// Gas value (lamports) attached to the cross-chain ITS deploy/transfer (default: 0.01 SOL)
        #[arg(long)]
        gas_value: Option<u64>,

        /// Force a fresh token deploy even if a cached token already exists
        /// for this network/src/dst/deployer combination.
        #[arg(long)]
        fresh_token: bool,
    },

    /// Monitor express-execution reimbursement (observe-only): for each chain
    /// report recent express transfers' two phases (express-executed →
    /// executor reimbursed), or watch a single source tx through both phases.
    ExpressExecution {
        /// Express-supported chains to monitor (axelar IDs / config keys).
        /// Ignored when `--source-tx` is given.
        chains: Vec<String>,

        /// Monitor exactly this source tx through both phases (overrides the
        /// chains scan).
        #[arg(long)]
        source_tx: Option<String>,

        /// Path to chains config JSON (reserved; chain ids are passed directly).
        #[arg(long, env = "CHAINS_CONFIG")]
        config: Option<PathBuf>,

        /// How many recent express transfers per chain to report in scan mode.
        #[arg(long, default_value = "5")]
        recent: usize,

        /// Seconds to wait for the canonical execute (reimbursement) in
        /// single-tx mode before reporting PENDING/timeout.
        #[arg(long, default_value = "1800")]
        timeout_secs: u64,
    },

    /// Cross-chain load test (auto-detects chains, RPCs, and test type from config)
    LoadTest {
        /// Path to chains config JSON (e.g. devnet-amplifier.json,
        /// testnet.json, mainnet.json). Omit to resolve it from `--network`
        /// (sibling checkout, then cache, then GitHub fetch).
        #[arg(long, env = "CHAINS_CONFIG")]
        config: Option<PathBuf>,

        /// Number of transactions to send (default: a single end-to-end test)
        #[arg(long, default_value = "1")]
        num_txs: u64,

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

        /// Override source chain RPC URL (default: from config)
        #[arg(long, env = "SOURCE_RPC")]
        source_rpc: Option<String>,

        /// Override destination chain RPC URL (default: from config)
        #[arg(long, env = "DESTINATION_RPC")]
        destination_rpc: Option<String>,

        /// Hex-encoded payload to send (default: random test message)
        #[arg(long)]
        payload: Option<String>,

        /// Protocol: gmp (callContract) or its (interchainTransfer)
        #[arg(long, value_enum, default_value = "gmp")]
        protocol: Protocol,

        /// Gas value to attach for cross-chain gas (in wei, e.g. "10000000000000000")
        #[arg(long)]
        gas_value: Option<String>,

        /// ITS token ID to use (hex, skips token deployment)
        #[arg(long)]
        token_id: Option<String>,

        /// Sui Move type tag for the ITS coin, e.g.
        /// `0x96b4…::token::TOKEN`. Required for Sui-source ITS runs because
        /// `interchain_transfer<T>` PTBs need the type at compile time. If
        /// omitted, the runner will resolve it via dev-inspect on
        /// `interchain_token_service::registered_coin_type(token_id)`.
        #[arg(long)]
        coin_type: Option<String>,

        /// Transactions per second for sustained load test
        #[arg(long)]
        tps: Option<u64>,

        /// Duration in seconds for sustained load test (use with --tps)
        #[arg(long)]
        duration_secs: Option<u64>,

        /// Key cycle interval in seconds (default: 3). Each signing key waits this
        /// many seconds before reuse. pool_size = tps × key_cycle. Higher values
        /// use more wallets, reducing per-address mempool pressure.
        #[arg(long, default_value = "3")]
        key_cycle: u64,

        /// Number of extra accounts to add to ITS-with-data payloads (default: 0).
        /// The first extra account is a valid ATA for the ITS token mint;
        /// remaining accounts are random pubkeys. Useful for testing ALT paths.
        #[arg(long, default_value = "0")]
        extra_accounts: u32,
    },
}

#[derive(Subcommand)]
pub enum InfoCommands {
    /// Show info about a block. With no arguments, shows the current head.
    /// With a height, shows that block's timestamp (predicted if the height
    /// is in the future). With `--at-time`, predicts the block at that time.
    Block {
        /// Block height. Omit to show the current head. Mutually exclusive
        /// with `--at-time`.
        number: Option<u64>,

        /// Axelar network (mainnet, testnet, stagenet, devnet-amplifier)
        #[arg(long, default_value = "testnet")]
        network: Network,

        /// Predict the block at this time (RFC3339, e.g.
        /// `2026-05-18T14:00:00Z`, or unix seconds). Mutually exclusive
        /// with the positional height.
        #[arg(long, conflicts_with = "number")]
        at_time: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum DecodeCommands {
    /// Decode raw hex calldata (ITS, Gateway, Factory)
    Calldata {
        /// Hex-encoded calldata (with or without 0x prefix, whitespace is stripped)
        #[arg(trailing_var_arg = true, num_args = 1..)]
        hex: Vec<String>,
    },

    /// Fetch and decode a full EVM transaction (calldata + logs)
    Tx {
        /// Transaction hash (0x...)
        txid: String,

        /// Path to chains config JSON (auto-discovered from sibling axelar-contract-deployments repo)
        #[arg(long, env = "CHAINS_CONFIG")]
        config: Option<PathBuf>,

        /// Chain axelar ID (skip RPC brute-forcing)
        #[arg(long)]
        chain: Option<String>,
    },

    /// Show recent Solana program activity (Gateway, ITS, GasService, Memo)
    SolActivity {
        /// Filter to a specific program type
        #[arg(long, value_enum)]
        program: Option<SolProgram>,

        /// Axelar network (devnet-amplifier, stagenet, testnet, mainnet)
        #[arg(long)]
        network: Option<Network>,

        /// Number of recent transactions to show per program (default: 20)
        #[arg(long, default_value = "20")]
        limit: usize,

        /// Output as JSON for machine consumption
        #[arg(long)]
        json: bool,
    },

    /// Show recent EVM contract events (Gateway, ITS, GasService)
    EvmActivity {
        /// Filter to a specific contract type
        #[arg(long, value_enum)]
        contract: Option<EvmContract>,

        /// Axelar network (defaults to AXE_NETWORK, else testnet)
        #[arg(long)]
        network: Option<Network>,

        /// EVM chain name (e.g. avalanche-fuji, eth-sepolia)
        #[arg(long)]
        chain: String,

        /// Max number of events to show per contract (default: 20)
        #[arg(long, default_value = "20")]
        limit: usize,

        /// Output as JSON for machine consumption
        #[arg(long)]
        json: bool,
    },
}

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub enum SolProgram {
    Gateway,
    Its,
    GasService,
    Memo,
}

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub enum EvmContract {
    Gateway,
    Its,
    GasService,
}

pub fn resolve_axelar_id(opt: Option<String>) -> Result<String> {
    opt.or_else(|| std::env::var("CHAIN").ok())
        .ok_or_else(|| eyre::eyre!("--axelar-id not provided and CHAIN env var not set"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every subcommand must parse alongside the global `--network` flag.
    /// Guards against the clap id-collision panic ("Mismatch between
    /// definition and access of `network`") that occurs when a subcommand
    /// declares its own `network` arg with a type other than
    /// `Option<Network>`'s inner type.
    #[test]
    fn all_subcommands_parse_with_global_network_flag() {
        let cases: &[&[&str]] = &[
            &["axe", "--network", "testnet", "deploy", "status"],
            &["axe", "--network", "testnet", "test", "gmp"],
            &["axe", "--network", "testnet", "decode", "calldata", "0x00"],
            &["axe", "--network", "testnet", "decode", "tx", "0xabc"],
            &["axe", "--network", "testnet", "decode", "sol-activity"],
            &[
                "axe",
                "decode",
                "evm-activity",
                "--network",
                "testnet",
                "--chain",
                "avalanche-fuji",
            ],
            &["axe", "verifiers", "testnet", "xrpl"],
            &["axe", "its-ownership", "testnet"],
            &["axe", "its-ownership"],
            &["axe", "check-balances", "testnet"],
            &["axe", "check-balances"],
            &["axe", "--network", "mainnet", "check-balances"],
            &["axe", "info", "block", "--network", "testnet"],
            &["axe", "verifier-votes", "testnet", "xrpl", "axelar1abc"],
            &["axe", "propose", "testnet", "hedera", "--op", "pause"],
        ];
        for args in cases {
            if let Err(e) = Cli::try_parse_from(*args) {
                panic!("failed to parse {args:?}: {e}");
            }
        }
    }

    /// The global `--network` is propagated by clap into subcommand-local
    /// args that share its id — `main.rs` reading the local field therefore
    /// reads the global value. This is invisible at the call sites, so pin
    /// it here against regressions (and against reviewers reasoning it away).
    #[test]
    fn global_network_propagates_into_subcommand_args() {
        let cli = Cli::try_parse_from(["axe", "--network", "mainnet", "info", "block"]).unwrap();
        let Commands::Info {
            subcommand: InfoCommands::Block { network, .. },
        } = cli.command
        else {
            panic!("expected info block");
        };
        assert_eq!(
            network,
            Network::Mainnet,
            "global flag must beat the local default"
        );

        let cli =
            Cli::try_parse_from(["axe", "--network", "mainnet", "decode", "sol-activity"]).unwrap();
        let Commands::Decode {
            subcommand: DecodeCommands::SolActivity { network, .. },
        } = cli.command
        else {
            panic!("expected decode sol-activity");
        };
        assert_eq!(network, Some(Network::Mainnet));

        // Without the flag, the local default applies.
        let cli = Cli::try_parse_from(["axe", "info", "block"]).unwrap();
        let Commands::Info {
            subcommand: InfoCommands::Block { network, .. },
        } = cli.command
        else {
            panic!("expected info block");
        };
        assert_eq!(network, Network::Testnet);
    }
}
