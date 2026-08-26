#![deny(clippy::absolute_paths)]

#[path = "load_test/support/chain_names.rs"]
mod chain_names;
#[path = "load_test/submission/evm_sender.rs"]
mod evm_sender;
#[path = "load_test/support/gas_estimate.rs"]
mod gas_estimate;
#[path = "load_test/support/gas_mode.rs"]
mod gas_mode;
#[path = "load_test/routes/gmp.rs"]
mod gmp;
#[path = "load_test/support/gmp_payload.rs"]
mod gmp_payload;
#[path = "load_test/submission/gmp_sui_source.rs"]
mod gmp_sui_source;
#[path = "load_test/verification/gmp_verification.rs"]
mod gmp_verification;
#[path = "load_test/support/helpers.rs"]
mod helpers;
#[path = "load_test/support/identifiers.rs"]
mod identifiers;
#[path = "load_test/submission/its_evm_source.rs"]
mod its_evm_source;
#[path = "load_test/routes/its_evm_to_evm.rs"]
mod its_evm_to_evm;
#[path = "load_test/routes/its_evm_to_sol.rs"]
mod its_evm_to_sol;
#[path = "load_test/routes/its_evm_to_sol_with_data.rs"]
mod its_evm_to_sol_with_data;
#[path = "load_test/routes/its_evm_to_stellar.rs"]
mod its_evm_to_stellar;
#[path = "load_test/routes/its_evm_to_sui.rs"]
mod its_evm_to_sui;
#[path = "load_test/routes/its_evm_to_xrpl.rs"]
mod its_evm_to_xrpl;
#[path = "load_test/support/its_prerequisites.rs"]
mod its_prerequisites;
#[path = "load_test/submission/its_sol_source.rs"]
mod its_sol_source;
#[path = "load_test/routes/its_sol_to_evm.rs"]
mod its_sol_to_evm;
#[path = "load_test/routes/its_sol_to_sui.rs"]
mod its_sol_to_sui;
#[path = "load_test/submission/its_stellar_source.rs"]
mod its_stellar_source;
#[path = "load_test/routes/its_stellar_to_evm.rs"]
mod its_stellar_to_evm;
#[path = "load_test/routes/its_stellar_to_sol.rs"]
mod its_stellar_to_sol;
#[path = "load_test/routes/its_stellar_to_sui.rs"]
mod its_stellar_to_sui;
#[path = "load_test/submission/its_sui_source.rs"]
mod its_sui_source;
#[path = "load_test/routes/its_sui_to_evm.rs"]
mod its_sui_to_evm;
#[path = "load_test/routes/its_sui_to_sol.rs"]
mod its_sui_to_sol;
#[path = "load_test/verification/its_verification.rs"]
mod its_verification;
#[path = "load_test/routes/its_xrpl_to_evm.rs"]
mod its_xrpl_to_evm;
#[path = "load_test/submission/keypairs.rs"]
mod keypairs;
#[path = "load_test/support/metrics.rs"]
pub mod metrics;
#[path = "load_test/support/resolve.rs"]
mod resolve;
#[path = "load_test/scheduling/retry.rs"]
mod retry;
#[path = "load_test/support/route.rs"]
pub(crate) mod route;
#[path = "load_test/scheduling/run_sizing.rs"]
mod run_sizing;
#[path = "load_test/submission/sol_sender.rs"]
mod sol_sender;
#[path = "load_test/submission/stellar_sender.rs"]
mod stellar_sender;
#[path = "load_test/scheduling/submitter.rs"]
mod submitter;
#[path = "load_test/scheduling/sustained.rs"]
mod sustained;
#[path = "load_test/scheduling/task_group.rs"]
mod task_group;
#[path = "load_test/support/units.rs"]
mod units;
#[path = "load_test/verification/verification_session.rs"]
mod verification_session;
#[path = "load_test/verification/engine.rs"]
mod verify;
#[path = "load_test/submission/xrpl_sender.rs"]
mod xrpl_sender;

// Re-exports for callers outside the load_test module:
// - `ensure_sender_receiver_on_evm_chain` is used by `commands::test_gmp`
//   (the `test_gmp --config` flow deploys / reuses a SenderReceiver on the
//   destination EVM chain).
// - `make_executable_payload` / `memo_program_id` are used by
//   `commands::test_gmp::source` to build the Solana-side memo payload.
// - `resolve_from_config` is used by `main.rs` to resolve a chains-config
//   JSON into a `ResolvedConfig` before dispatching to `run`.
pub(crate) use gmp_payload::{make_executable_payload, memo_program_id};
pub(crate) use helpers::ensure_sender_receiver_on_evm_chain;
pub(crate) use resolve::resolve_from_config;

// Re-export helpers/resolve names through `load_test` so the per-pair
// modules (its_*.rs, gmp.rs, the *_sender modules) can keep calling them
// as `super::name`. Each entry is `pub(super)`, restricting reach to
// `load_test` itself — exactly the scope its siblings sit in.
pub(super) use helpers::{
    axelar_id_for_chain, check_evm_balance, deploy_or_reuse_sender_receiver,
    deploy_sender_receiver, ensure_evm_contract_deployed, ensure_sender_receiver,
    finalize_sui_dest_run, finalize_sui_dest_run_its, finish_report, load_stellar_main_wallet,
    load_sui_main_wallet, read_stellar_contract_address, read_stellar_network_type,
    read_stellar_token_address, read_sui_axe_token_id, resolve_sui_axe_token, sui_dest_lookup,
    sui_its_dest_lookup, validate_evm_rpc, validate_solana_rpc,
};
pub(super) use resolve::{
    GmpCache, ItsCache, find_cached_salt, read_cache, read_its_cache, save_cache, save_its_cache,
};
// `pub(crate)` (not `pub(super)`): cli::resolve_network also detects the
// network from `--config` filenames.
pub(crate) use resolve::detect_network_from_config;
pub(crate) use resolve::set_cache_network;
pub(crate) use resolve::set_report_dir;

use std::env;
use std::fmt::{Display, Formatter, Result as FmtResult};
use std::path::PathBuf;

use eyre::Result;

use crate::config::AxelarChainContract;
use crate::config::ChainsConfig;
use crate::hyperliquid::{enable_big_blocks_from_key, env_for};
use crate::types::Network;
use crate::ui;
use chain_names::{AxelarChainId, ConfigChainId, RpcUrl};
use route::{GmpRoute, ItsRoute, SupportedRoute};

/// Load test type (extensible for future directions).
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum TestType {
    /// Solana -> EVM (GMP, ITS)
    SolToEvm,
    /// EVM -> Solana (GMP, ITS, ITS-with-data)
    EvmToSol,
    /// EVM -> EVM (GMP)
    EvmToEvm,
    /// Solana -> Solana (GMP)
    SolToSol,
    /// XRPL -> EVM (ITS, canonical XRP)
    XrplToEvm,
    /// EVM -> XRPL (ITS, canonical XRP)
    EvmToXrpl,
    /// Stellar -> EVM (GMP, ITS)
    StellarToEvm,
    /// EVM -> Stellar (GMP, ITS)
    EvmToStellar,
    /// Stellar -> Solana (GMP only — Stellar ITS testnet does not yet trust "solana"
    /// as a destination chain; ITS will fail with Contract Error #7
    /// (UntrustedChain) until the ITS owner runs `add-trusted-chains solana`.)
    StellarToSol,
    /// Solana -> Stellar (GMP)
    SolToStellar,
    /// Sui -> EVM (GMP). ITS variant forthcoming.
    SuiToEvm,
    /// EVM -> Sui (GMP + ITS scaffolded; runs are stubbed pending Sui
    /// destination verifier wiring in poll_pipeline).
    EvmToSui,
    /// Sui -> Solana (GMP + ITS scaffolded).
    SuiToSol,
    /// Solana -> Sui (GMP + ITS scaffolded).
    SolToSui,
    /// Sui -> Stellar (GMP + ITS scaffolded).
    SuiToStellar,
    /// Stellar -> Sui (GMP + ITS scaffolded).
    StellarToSui,
    /// Sui -> XRPL (ITS only — XRPL has no GMP).
    SuiToXrpl,
    /// XRPL -> Sui (ITS only).
    XrplToSui,
}

impl Display for TestType {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        match self {
            TestType::SolToEvm => write!(f, "sol-to-evm"),
            TestType::EvmToSol => write!(f, "evm-to-sol"),
            TestType::EvmToEvm => write!(f, "evm-to-evm"),
            TestType::SolToSol => write!(f, "sol-to-sol"),
            TestType::XrplToEvm => write!(f, "xrpl-to-evm"),
            TestType::EvmToXrpl => write!(f, "evm-to-xrpl"),
            TestType::StellarToEvm => write!(f, "stellar-to-evm"),
            TestType::EvmToStellar => write!(f, "evm-to-stellar"),
            TestType::StellarToSol => write!(f, "stellar-to-sol"),
            TestType::SolToStellar => write!(f, "sol-to-stellar"),
            TestType::SuiToEvm => write!(f, "sui-to-evm"),
            TestType::EvmToSui => write!(f, "evm-to-sui"),
            TestType::SuiToSol => write!(f, "sui-to-sol"),
            TestType::SolToSui => write!(f, "sol-to-sui"),
            TestType::SuiToStellar => write!(f, "sui-to-stellar"),
            TestType::StellarToSui => write!(f, "stellar-to-sui"),
            TestType::SuiToXrpl => write!(f, "sui-to-xrpl"),
            TestType::XrplToSui => write!(f, "xrpl-to-sui"),
        }
    }
}

/// Protocol: GMP (callContract), ITS (interchainTransfer), or ITS with data
/// (interchainTransfer that triggers a contract call on the destination).
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    clap::ValueEnum,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum Protocol {
    #[default]
    Gmp,
    Its,
    /// ITS interchainTransfer with data — sends tokens AND calls the memo
    /// program on the Solana destination chain.
    ItsWithData,
}

impl Display for Protocol {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        match self {
            Protocol::Gmp => write!(f, "gmp"),
            Protocol::Its => write!(f, "its"),
            Protocol::ItsWithData => write!(f, "its-with-data"),
        }
    }
}

/// CLI arguments for the load test command.
#[derive(Clone)]
pub(crate) struct LoadTestArgs {
    pub config: PathBuf,
    /// The Axelar network this run targets (resolved in `main.rs` from
    /// `--network` / the config filename).
    pub network: Network,
    pub test_type: TestType,
    pub protocol: Protocol,
    pub destination_chain: ConfigChainId,
    pub source_chain: ConfigChainId,
    /// The `axelarId` for the source chain (used for Cosmos-side verification).
    pub source_axelar_id: AxelarChainId,
    /// The `axelarId` for the destination chain (used for Cosmos-side verification).
    pub destination_axelar_id: AxelarChainId,
    pub source_rpc: RpcUrl,
    pub destination_rpc: RpcUrl,
    pub private_key: Option<String>,
    pub num_txs: u64,
    pub keypair: Option<String>,
    pub payload: Option<String>,
    pub gas_value: Option<String>,
    pub token_id: Option<String>,
    /// Sui Move type tag for ITS coin (e.g. `0x...::token::TOKEN`). Used by
    /// Sui-source ITS runs; resolved via dev-inspect when omitted.
    pub coin_type: Option<String>,
    pub tps: Option<u64>,
    pub duration_secs: Option<u64>,
    pub key_cycle: u64,
    /// Number of extra accounts to add to ITS-with-data payloads.
    /// The first extra account is a valid ATA for the ITS token mint;
    /// remaining accounts are random pubkeys. Useful for testing ALT paths.
    pub extra_accounts: u32,
    /// Names the report artifact when set, so a caller that started this run
    /// in the background can find its report afterwards. `None` keeps the
    /// CLI's historical timestamp-derived filename.
    pub run_id: Option<String>,
}

pub async fn run(args: LoadTestArgs) -> Result<()> {
    // Scope the ITS/GMP cache files and the axe-tokens overlay lookups to the
    // network this invocation runs on (chain ids collide across networks).
    resolve::set_cache_network(args.network);
    let run_sizing = run_sizing::RunSizing::new(&args)?;

    ui::section(&format!(
        "Load Test ({}/{}): {} -> {}",
        args.protocol, args.test_type, args.source_chain, args.destination_chain
    ));

    enable_hyperliquid_big_blocks(&args).await;
    validate_source_support(&args).await?;
    dispatch(args, run_sizing).await
}

async fn enable_hyperliquid_big_blocks(args: &LoadTestArgs) {
    // Hyperliquid requires the deploying wallet to be opted into "big blocks"
    // before any contract-deploy tx is accepted. axe deploys SenderReceiver
    // (GMP) and a fresh ITS token (ITS source-side) on first run, so we
    // pre-emptively enable big blocks whenever Hyperliquid is on either side.
    // Idempotent on the API side: calling enable twice in a row is a no-op.
    if !args.source_axelar_id.starts_with("hyperliquid")
        && !args.destination_axelar_id.starts_with("hyperliquid")
    {
        return;
    }

    let key = args
        .private_key
        .clone()
        .or_else(|| env::var("EVM_PRIVATE_KEY").ok());
    let Some(key) = key else {
        ui::warn(
            "Hyperliquid is in this route but EVM_PRIVATE_KEY is not set; big-blocks opt-in skipped",
        );
        return;
    };

    let env = env_for(args.network);
    match enable_big_blocks_from_key(&key, env).await {
        Ok(addr) => ui::info(&format!("Hyperliquid big-blocks enabled for {addr}")),
        Err(error) => ui::warn(&format!(
            "Hyperliquid big-blocks opt-in failed: {error} — contract deploys on Hyperliquid may be rejected"
        )),
    }
}

async fn validate_source_support(args: &LoadTestArgs) -> Result<()> {
    // A consensus (legacy) source has no VotingVerifier. XRPL uses
    // `XrplVotingVerifier` (not `VotingVerifier`), so we also accept that as
    // evidence of a verifiable Amplifier source. Stellar shares the
    // `VotingVerifier` contract name, so the standard check covers it too.
    let src = &args.source_chain;
    let cfg = ChainsConfig::load(&args.config).await?;
    let has_standard_vv = cfg
        .axelar
        .contract_address(AxelarChainContract::VotingVerifier, &args.source_axelar_id)
        .is_ok();
    let has_xrpl_vv = cfg
        .axelar
        .contract_address(
            AxelarChainContract::XrplVotingVerifier,
            &args.source_axelar_id,
        )
        .is_ok();
    // A legacy (consensus) chain is always EVM, so a legacy *source* only
    // appears on an `Evm -> X` route. Allow those (destination-side verification
    // handles legacy or Amplifier dests on-chain); bail on anything else, which
    // would mean a non-EVM source with no voting verifier (unsupported).
    let legacy_evm_source = matches!(
        args.test_type,
        TestType::EvmToEvm
            | TestType::EvmToSol
            | TestType::EvmToSui
            | TestType::EvmToStellar
            | TestType::EvmToXrpl
    );
    if !has_standard_vv && !has_xrpl_vv && !legacy_evm_source {
        eyre::bail!(
            "source chain '{src}' is a legacy (consensus) chain with no VotingVerifier. \
             Legacy support requires an EVM source — the {}/{} route is not supported.",
            args.protocol,
            args.test_type
        );
    }
    Ok(())
}

async fn dispatch(args: LoadTestArgs, run_sizing: run_sizing::RunSizing) -> Result<()> {
    let route = SupportedRoute::resolve(
        args.protocol,
        args.test_type,
        &args.source_chain,
        &args.destination_chain,
    )?;

    match route {
        SupportedRoute::Gmp(route) => dispatch_gmp(route, args, run_sizing).await,
        SupportedRoute::Its(route) => dispatch_its(route, args, run_sizing).await,
        SupportedRoute::ItsWithData(route) => dispatch_its_with_data(route, args, run_sizing).await,
    }
}

async fn dispatch_gmp(
    route: GmpRoute,
    args: LoadTestArgs,
    run_sizing: run_sizing::RunSizing,
) -> Result<()> {
    match route {
        GmpRoute::SolToEvm => gmp::run_sol_to_evm(args, run_sizing).await,
        GmpRoute::EvmToSol => gmp::run_evm_to_sol(args, run_sizing).await,
        GmpRoute::EvmToEvm => gmp::run_evm_to_evm(args, run_sizing).await,
        GmpRoute::SolToSol => gmp::run_sol_to_sol(args, run_sizing).await,
        GmpRoute::StellarToEvm => gmp::run_stellar_to_evm(args, run_sizing).await,
        GmpRoute::EvmToStellar => gmp::run_evm_to_stellar(args, run_sizing).await,
        GmpRoute::StellarToSol => gmp::run_stellar_to_sol(args, run_sizing).await,
        GmpRoute::SolToStellar => gmp::run_sol_to_stellar(args, run_sizing).await,
        GmpRoute::SuiToEvm => gmp::run_sui_to_evm(args, run_sizing).await,
        GmpRoute::EvmToSui => gmp::run_evm_to_sui(args, run_sizing).await,
        GmpRoute::SolToSui => gmp::run_sol_to_sui(args, run_sizing).await,
        GmpRoute::StellarToSui => gmp::run_stellar_to_sui(args, run_sizing).await,
        GmpRoute::SuiToSol => gmp::run_sui_to_sol(args, run_sizing).await,
    }
}

async fn dispatch_its(
    route: ItsRoute,
    args: LoadTestArgs,
    run_sizing: run_sizing::RunSizing,
) -> Result<()> {
    match route {
        ItsRoute::StellarToEvm => its_stellar_to_evm::run(args, run_sizing).await,
        ItsRoute::EvmToStellar => its_evm_to_stellar::run(args, run_sizing).await,
        // Stellar -> Solana ITS: code is in place, but the destination chain
        // must be in the Stellar ITS contract's trusted-chains list. On
        // testnet today "solana" is not registered, so the source-side
        // simulation reverts with Contract Error #7. The runner will surface
        // that clearly. We leave it dispatched so the run becomes possible
        // automatically once the trusted-chain config is updated upstream.
        ItsRoute::StellarToSol => its_stellar_to_sol::run(args, run_sizing).await,
        ItsRoute::EvmToSol => its_evm_to_sol::run(args, run_sizing).await,
        ItsRoute::SolToEvm => its_sol_to_evm::run(args, run_sizing).await,
        ItsRoute::XrplToEvm => its_xrpl_to_evm::run(args, run_sizing).await,
        ItsRoute::EvmToXrpl => its_evm_to_xrpl::run(args, run_sizing).await,
        ItsRoute::EvmToEvm => its_evm_to_evm::run(args, run_sizing).await,
        // Sui as destination — Sui events-based verifier is now wired in
        // verify.rs. EVM -> Sui GMP runs end-to-end. ITS to Sui still
        // needs the receive-side coin type plumbing.
        ItsRoute::EvmToSui => its_evm_to_sui::run(args, run_sizing).await,
        ItsRoute::SolToSui => its_sol_to_sui::run(args, run_sizing).await,
        ItsRoute::StellarToSui => its_stellar_to_sui::run(args, run_sizing).await,
        // Sui-source ITS. We don't auto-deploy a fresh AXE token on Sui
        // (Move package publish from Rust is impractical), so the user must
        // pre-register a token via axelar-contract-deployments/sui/its.js
        // and pass `--token-id`. `--coin-type` resolves automatically via
        // dev-inspect when omitted.
        ItsRoute::SuiToEvm => its_sui_to_evm::run(args, run_sizing).await,
        ItsRoute::SuiToSol => its_sui_to_sol::run(args, run_sizing).await,
    }
}

async fn dispatch_its_with_data(
    route: route::ItsWithDataRoute,
    args: LoadTestArgs,
    run_sizing: run_sizing::RunSizing,
) -> Result<()> {
    match route {
        route::ItsWithDataRoute::EvmToSol => its_evm_to_sol_with_data::run(args, run_sizing).await,
    }
}
