use std::path::PathBuf;

use alloy::primitives::Address;
use clap::{Args, Parser, Subcommand};
use eyre::Result;

use crate::commands::intents::{AssetSpec, AssetType, HumanAmount, OrderType, QuoteBenchmarkMode};
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
    /// Test Axelar intent routes through the public RFQ API
    Intents {
        #[command(subcommand)]
        subcommand: IntentsCommands,
    },

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

    /// Run the fee-api gas / compute-unit benchmarks (EVM + Solana harnesses)
    Bench {
        #[command(subcommand)]
        subcommand: BenchCommands,
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

    /// Serve axe over the Model Context Protocol on stdio, so an LLM can
    /// drive it. The network is fixed for the life of the server: pass
    /// `--network` or set `AXE_NETWORK`.
    Mcp {
        /// Allow the server to be pinned to mainnet. Without this, starting
        /// against mainnet is refused: the flows spend real funds, so that
        /// decision is made once by a human outside the conversation.
        #[arg(long)]
        allow_mainnet: bool,
    },
}

#[derive(Subcommand)]
pub enum BenchCommands {
    /// EVM source-gas benchmark: `forge test` GasHarness on a mainnet fork.
    /// Prints per-operation gasUsed for the cost.source_gas_units config.
    EvmGas {
        /// Ethereum mainnet RPC URL (else uses MAINNET_RPC_URL, else a public node)
        #[arg(long)]
        rpc: Option<String>,
    },
    /// Solana compute-unit benchmark: runs the LiteSVM harness against the real
    /// mainnet program binaries (fetched automatically when missing). Prints the
    /// compute units for the cost.solana config.
    SolanaCu,
    /// Run both benchmarks.
    All,
}

#[derive(Subcommand)]
pub enum IntentsCommands {
    /// Show supported chains with their tokens
    Catalog(IntentCatalogOptions),

    /// Show the solver's catalog-backed token inventory and USD value
    Inventory(IntentInventoryOptions),

    /// Request and inspect one quote without executing it
    Quote(IntentQuoteOptions),

    /// Show or watch the status of a quote
    Status(IntentStatusOptions),

    /// Benchmark intent API operations
    Bench {
        #[command(subcommand)]
        subcommand: IntentBenchCommands,
    },

    /// Send one intent over a random or explicit route
    Send(IntentSendOptions),

    /// Send one intent in each direction over the same asset pair
    Roundtrip(IntentRoundtripOptions),

    /// Run round trips across every currently executable wallet route
    Sweep(IntentSweepOptions),

    /// Continuously simulate users across all executable intent routes
    Traffic(IntentTrafficOptions),
}

#[derive(Args)]
pub struct IntentApiOptions {
    /// RFQ API base URL. Defaults to the selected network's public endpoint.
    #[arg(long, env = "INTENTS_API_URL")]
    pub api_url: Option<String>,
}

#[derive(Args)]
pub struct IntentReadOptions {
    #[command(flatten)]
    pub api: IntentApiOptions,

    /// Print machine-readable JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub struct IntentCatalogOptions {
    #[command(flatten)]
    pub read: IntentReadOptions,

    /// Show only this CAIP-2 chain ID.
    #[arg(long)]
    pub chain: Option<String>,
}

#[derive(Args)]
pub struct IntentInventoryOptions {
    #[command(flatten)]
    pub read: IntentReadOptions,

    /// Path to chains config JSON. Omit to resolve from --network.
    #[arg(long, env = "CHAINS_CONFIG")]
    pub config: Option<PathBuf>,
}

#[derive(Args)]
pub struct IntentAssetOptions {
    /// Use token-to-token or native-to-native routes.
    #[arg(long, value_enum, default_value_t)]
    pub asset_type: AssetType,
}

#[derive(Args)]
pub struct IntentQuoteRequestOptions {
    /// Source asset as <CAIP-2 chain>/<token address>.
    #[arg(long)]
    pub from: AssetSpec,

    /// Destination asset as <CAIP-2 chain>/<token address>.
    #[arg(long)]
    pub to: AssetSpec,

    /// Human-readable source amount for exact-input or output amount for exact-output.
    #[arg(long)]
    pub amount: HumanAmount,

    /// Address used to construct approval and deposit actions.
    #[arg(long)]
    pub sender: Address,

    /// Destination recipient. Defaults to --sender.
    #[arg(long)]
    pub recipient: Option<Address>,

    /// Fix the source input or destination output amount.
    #[arg(long, value_enum, default_value_t)]
    pub order_type: OrderType,
}

#[derive(Args)]
pub struct IntentQuoteOptions {
    #[command(flatten)]
    pub read: IntentReadOptions,

    #[command(flatten)]
    pub request: IntentQuoteRequestOptions,
}

#[derive(Args)]
pub struct IntentStatusOptions {
    #[command(flatten)]
    pub read: IntentReadOptions,

    /// Quote ID returned by the intent API.
    pub quote_id: String,

    /// Poll until the quote completes, refunds, fails, or times out.
    #[arg(long)]
    pub watch: bool,

    /// Seconds between status requests in watch mode.
    #[arg(long, default_value = "2", value_parser = clap::value_parser!(u64).range(1..))]
    pub poll_interval_secs: u64,

    /// Maximum seconds to watch before returning an error.
    #[arg(long, default_value = "1200", value_parser = clap::value_parser!(u64).range(1..))]
    pub timeout_secs: u64,
}

#[derive(Subcommand)]
pub enum IntentBenchCommands {
    /// Benchmark the solver quote path with parallel requests
    Quote(IntentQuoteBenchOptions),
}

#[derive(Args)]
pub struct IntentQuoteBenchOptions {
    #[command(flatten)]
    pub read: IntentReadOptions,

    #[command(flatten)]
    pub assets: IntentAssetOptions,

    /// Restrict automatic discovery to this source asset.
    #[arg(long)]
    pub from: Option<AssetSpec>,

    /// Restrict automatic discovery to this destination asset.
    #[arg(long)]
    pub to: Option<AssetSpec>,

    /// Human-readable amount. Defaults to 1 source or destination token.
    #[arg(long)]
    pub amount: Option<HumanAmount>,

    /// Quote sender. Defaults to EVM_PRIVATE_KEY's address, then the zero address.
    #[arg(long)]
    pub sender: Option<Address>,

    /// Key used only to derive the quote sender when --sender is omitted.
    #[arg(long, env = "EVM_PRIVATE_KEY", hide_env_values = true)]
    pub private_key: Option<String>,

    /// Destination recipient. Defaults to the resolved sender.
    #[arg(long)]
    pub recipient: Option<Address>,

    /// Fix the source input or destination output amount.
    #[arg(long, value_enum, default_value_t)]
    pub order_type: OrderType,

    /// Scheduling mode. Defaults to burst, or continuous when --duration-secs is set.
    #[arg(long, value_enum)]
    pub mode: Option<QuoteBenchmarkMode>,

    /// Requests to measure in burst mode. Defaults to 100.
    #[arg(long, conflicts_with = "duration_secs", value_parser = clap::value_parser!(u64).range(1..))]
    pub requests: Option<u64>,

    /// Runtime for continuous mode. Defaults to 60 seconds.
    #[arg(long, conflicts_with = "requests", value_parser = clap::value_parser!(u64).range(1..))]
    pub duration_secs: Option<u64>,

    /// Maximum number of in-flight quote requests.
    #[arg(long, default_value = "8", value_parser = clap::value_parser!(u16).range(1..))]
    pub concurrency: u16,

    /// Unmeasured requests to run before the benchmark.
    #[arg(long, default_value = "10")]
    pub warmup: u64,

    /// Maximum seconds to wait for each quote request.
    #[arg(long, default_value = "10", value_parser = clap::value_parser!(u64).range(1..))]
    pub request_timeout_secs: u64,

    /// Limit aggregate request starts per second.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    pub max_rps: Option<u64>,
}

#[derive(Args)]
pub struct IntentRuntimeConfigOptions {
    #[command(flatten)]
    pub api: IntentApiOptions,

    /// Path to chains config JSON. Omit to resolve from --network.
    #[arg(long, env = "CHAINS_CONFIG")]
    pub config: Option<PathBuf>,

    /// EVM key for the axe wallet (prefer EVM_PRIVATE_KEY).
    #[arg(long, env = "EVM_PRIVATE_KEY", hide_env_values = true)]
    pub private_key: String,

    /// Seconds between RFQ status requests.
    #[arg(long, default_value = "2", value_parser = clap::value_parser!(u64).range(1..))]
    pub poll_interval_secs: u64,

    /// Maximum seconds to wait for one intent fulfillment.
    #[arg(long, default_value = "1200", value_parser = clap::value_parser!(u64).range(1..))]
    pub fulfillment_timeout_secs: u64,
}

#[derive(Args)]
pub struct IntentRuntimeOptions {
    #[command(flatten)]
    pub config: IntentRuntimeConfigOptions,

    /// Execute without an interactive confirmation.
    #[arg(long)]
    pub yes: bool,
}

#[derive(Args)]
pub struct IntentRouteOptions {
    #[command(flatten)]
    pub assets: IntentAssetOptions,

    /// Source asset as <CAIP-2 chain>/<token address>. Requires --to.
    #[arg(long, requires = "to")]
    pub from: Option<AssetSpec>,

    /// Destination asset as <CAIP-2 chain>/<token address>. Requires --from.
    #[arg(long, requires = "from")]
    pub to: Option<AssetSpec>,

    /// Human-readable fixed amount: source for exact-input, destination for exact-output.
    #[arg(long, requires_all = ["from", "to"])]
    pub amount: Option<HumanAmount>,

    /// Fix the source input or destination output amount.
    #[arg(long, value_enum, default_value_t)]
    pub order_type: OrderType,

    /// Basis points of spendable source balance when --amount is omitted.
    #[arg(long, default_value = "100", value_parser = clap::value_parser!(u16).range(1..=10_000))]
    pub wallet_bps: u16,
}

#[derive(Args)]
pub struct IntentSendOptions {
    #[command(flatten)]
    pub runtime: IntentRuntimeOptions,

    #[command(flatten)]
    pub route: IntentRouteOptions,

    /// Destination recipient. Defaults to the axe wallet.
    #[arg(long)]
    pub recipient: Option<Address>,
}

#[derive(Args)]
pub struct IntentRoundtripOptions {
    #[command(flatten)]
    pub runtime: IntentRuntimeOptions,

    #[command(flatten)]
    pub route: IntentRouteOptions,
}

#[derive(Args)]
pub struct IntentSweepOptions {
    #[command(flatten)]
    pub runtime: IntentRuntimeOptions,

    #[command(flatten)]
    pub assets: IntentAssetOptions,

    /// Complete passes over every currently executable route.
    #[arg(long, conflicts_with = "continuous", value_parser = clap::value_parser!(u64).range(1..))]
    pub sweeps: Option<u64>,

    /// Rediscover and sweep routes until Ctrl-C.
    #[arg(long)]
    pub continuous: bool,

    /// Print every executable round trip without submitting transactions.
    #[arg(long, conflicts_with_all = ["continuous", "sweeps", "yes"])]
    pub dry_run: bool,

    /// Basis points of each source asset's spendable balance per route.
    #[arg(long, default_value = "100", value_parser = clap::value_parser!(u16).range(1..=10_000))]
    pub wallet_bps: u16,

    /// Fix the source input or destination output amount.
    #[arg(long, value_enum, default_value_t)]
    pub order_type: OrderType,
}

#[derive(Args)]
pub struct IntentTrafficOptions {
    #[command(flatten)]
    pub runtime: IntentRuntimeConfigOptions,

    /// Maximum basis points of a source balance used by one route.
    #[arg(long, default_value = "10", value_parser = clap::value_parser!(u16).range(1..=1_000))]
    pub wallet_bps: u16,
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

        /// Originate a transfer that Axelar's own express executor will front,
        /// then watch it through both phases. Sends aUSDC through the
        /// AxelarApp proxy registered in the express registry, which is the
        /// only shape the service picks up (gateway path, allowlisted
        /// contract, capped amount). Requires --source-chain and
        /// --destination-chain.
        #[arg(long)]
        originate: bool,

        /// Source chain axelar ID for --originate.
        #[arg(long)]
        source_chain: Option<String>,

        /// Destination chain axelar ID for --originate.
        #[arg(long)]
        destination_chain: Option<String>,

        /// Express-asset base units (6 decimals) to send with --originate.
        /// Must stay inside the express registry's per-chain cap.
        #[arg(long, default_value = "5000000")]
        amount: String,

        /// Native gas to attach to the --originate call, in wei.
        #[arg(long, default_value = "350000000000000000")]
        gas_value: String,

        /// Override the AxelarApp proxy address used by --originate.
        #[arg(long)]
        app_address: Option<String>,

        /// Override the gateway token symbol for --originate. Defaults to the
        /// network's registered express asset (testnet aUSDC, mainnet
        /// axlUSDC/USDC).
        #[arg(long)]
        symbol: Option<String>,

        /// EVM private key for --originate.
        #[arg(long, env = "EVM_PRIVATE_KEY")]
        private_key: Option<String>,

        /// Override the source chain RPC URL for --originate.
        #[arg(long, env = "SOURCE_RPC")]
        source_rpc: Option<String>,
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

#[derive(Clone, Copy, Debug, clap::ValueEnum, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum SolProgram {
    Gateway,
    Its,
    GasService,
    Memo,
}

#[derive(Clone, Copy, Debug, clap::ValueEnum, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case")]
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
            &[
                "axe",
                "--network",
                "testnet",
                "intents",
                "send",
                "--private-key",
                "00",
            ],
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

    #[test]
    fn intent_send_and_roundtrip_accept_automatic_routes() {
        let cli = Cli::try_parse_from(["axe", "intents", "send", "--private-key", "00"]).unwrap();
        let Commands::Intents {
            subcommand: IntentsCommands::Send(options),
        } = cli.command
        else {
            panic!("expected intents send");
        };
        assert!(options.route.from.is_none());
        assert!(options.route.to.is_none());
        assert_eq!(options.route.assets.asset_type, AssetType::Token);

        assert!(
            Cli::try_parse_from([
                "axe",
                "intents",
                "roundtrip",
                "--private-key",
                "00",
                "--asset-type",
                "native",
            ])
            .is_ok()
        );
    }

    #[test]
    fn intent_send_accepts_an_explicit_route_and_human_amount() {
        assert!(
            Cli::try_parse_from([
                "axe",
                "intents",
                "send",
                "--private-key",
                "00",
                "--asset-type",
                "native",
                "--from",
                "eip155:11155111/0x0000000000000000000000000000000000000000",
                "--to",
                "eip155:43113/0x0000000000000000000000000000000000000000",
                "--amount",
                "0.01",
            ])
            .is_ok()
        );
    }

    #[test]
    fn intent_execution_commands_accept_asset_types() {
        assert!(
            Cli::try_parse_from([
                "axe",
                "intents",
                "sweep",
                "--private-key",
                "00",
                "--asset-type",
                "native",
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from([
                "axe",
                "intents",
                "send",
                "--private-key",
                "00",
                "--asset-type",
                "anything",
            ])
            .is_err()
        );
    }

    #[test]
    fn intent_traffic_is_continuous_and_balance_capped() {
        let cli = Cli::try_parse_from([
            "axe",
            "intents",
            "traffic",
            "--private-key",
            "00",
            "--wallet-bps",
            "25",
        ])
        .unwrap();
        let Commands::Intents {
            subcommand: IntentsCommands::Traffic(options),
        } = cli.command
        else {
            panic!("expected intents traffic");
        };
        assert_eq!(options.wallet_bps, 25);
        assert!(
            Cli::try_parse_from([
                "axe",
                "intents",
                "traffic",
                "--private-key",
                "00",
                "--wallet-bps",
                "1001",
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from(["axe", "intents", "traffic", "--private-key", "00", "--yes",])
                .is_err()
        );
    }

    #[test]
    fn intent_commands_accept_exact_output() {
        let cli = Cli::try_parse_from([
            "axe",
            "intents",
            "send",
            "--private-key",
            "00",
            "--order-type",
            "exact-output",
        ])
        .unwrap();
        let Commands::Intents {
            subcommand: IntentsCommands::Send(options),
        } = cli.command
        else {
            panic!("expected intents send");
        };
        assert_eq!(options.route.order_type, OrderType::ExactOutput);

        assert!(
            Cli::try_parse_from([
                "axe",
                "intents",
                "sweep",
                "--private-key",
                "00",
                "--order-type",
                "exact-output",
            ])
            .is_ok()
        );
    }

    #[test]
    fn intent_read_commands_parse_without_a_private_key() {
        const ASSET: &str = "eip155:11155111/0x0000000000000000000000000000000000000000";
        const ADDRESS: &str = "0x0000000000000000000000000000000000000001";

        assert!(Cli::try_parse_from(["axe", "intents", "catalog"]).is_ok());
        assert!(
            Cli::try_parse_from(["axe", "intents", "catalog", "--chain", "eip155:11155111",])
                .is_ok()
        );
        assert!(
            Cli::try_parse_from([
                "axe", "intents", "quote", "--from", ASSET, "--to", ASSET, "--amount", "1",
                "--sender", ADDRESS,
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from([
                "axe",
                "intents",
                "status",
                "quote-id",
                "--watch",
                "--timeout-secs",
                "30",
            ])
            .is_ok()
        );
    }

    #[test]
    fn intent_catalog_no_longer_has_nested_commands() {
        assert!(Cli::try_parse_from(["axe", "intents", "catalog", "chains"]).is_err());
        assert!(Cli::try_parse_from(["axe", "intents", "catalog", "tokens"]).is_err());
    }

    #[test]
    fn intent_quotes_require_sender_context() {
        const ASSET: &str = "eip155:11155111/0x0000000000000000000000000000000000000000";

        assert!(
            Cli::try_parse_from([
                "axe", "intents", "quote", "--from", ASSET, "--to", ASSET, "--amount", "1",
            ])
            .is_err()
        );
    }

    #[test]
    fn intent_quote_benchmark_accepts_parallel_controls() {
        const ASSET: &str = "eip155:11155111/0x0000000000000000000000000000000000000000";
        const ADDRESS: &str = "0x0000000000000000000000000000000000000001";
        let base = [
            "axe", "intents", "bench", "quote", "--from", ASSET, "--to", ASSET, "--amount", "1",
            "--sender", ADDRESS,
        ];
        assert!(Cli::try_parse_from(["axe", "intents", "bench", "quote"]).is_ok());

        assert!(
            Cli::try_parse_from(base.into_iter().chain([
                "--requests",
                "20",
                "--concurrency",
                "4",
                "--warmup",
                "2"
            ]))
            .is_ok()
        );
        assert!(
            Cli::try_parse_from(base.into_iter().chain([
                "--requests",
                "20",
                "--duration-secs",
                "5"
            ]))
            .is_err()
        );
        assert!(Cli::try_parse_from(base.into_iter().chain(["--concurrency", "0"])).is_err());
    }

    #[test]
    fn intent_route_requires_both_assets() {
        assert!(
            Cli::try_parse_from([
                "axe",
                "intents",
                "send",
                "--private-key",
                "00",
                "--from",
                "eip155:11155111/0x0000000000000000000000000000000000000000",
            ])
            .is_err()
        );
    }

    #[test]
    fn intent_sweep_modes_conflict_only_when_both_are_explicit() {
        assert!(
            Cli::try_parse_from([
                "axe",
                "intents",
                "sweep",
                "--private-key",
                "00",
                "--continuous",
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from([
                "axe",
                "intents",
                "sweep",
                "--private-key",
                "00",
                "--continuous",
                "--sweeps",
                "2",
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "axe",
                "intents",
                "sweep",
                "--private-key",
                "00",
                "--dry-run",
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from([
                "axe",
                "intents",
                "sweep",
                "--private-key",
                "00",
                "--dry-run",
                "--continuous",
            ])
            .is_err()
        );
    }

    #[test]
    fn exercise_was_renamed_to_sweep() {
        assert!(
            Cli::try_parse_from(["axe", "intents", "exercise", "--private-key", "00"]).is_err()
        );
    }
}
