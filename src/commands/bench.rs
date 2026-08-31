//! `axe bench` — run the fee-api gas / compute-unit benchmarks and print which
//! `global.toml [cost]` keys the measurements calibrate.
//!
//! This wraps the relocated harnesses; it does not re-implement the measurement:
//!   - EVM  (`benchmarks/evm-gas`): a Foundry test that forks Ethereum mainnet
//!     and meters real `gasUsed` per bridge operation (must run `--isolate`).
//!   - Solana (`benchmarks/solana-cu`): a LiteSVM harness that runs the real
//!     mainnet program binaries and meters `compute_units_consumed`.
//!
//! Each subcommand streams the harness's own output (its result table), then
//! prints a footer mapping the numbers to the fee-api cost config. The harnesses
//! live in the axe repo, so `axe bench` must run from a checkout (via `cargo run
//! -p axe -- bench ...` or the installed binary run from the repo root).

use std::path::{Path, PathBuf};
use std::process::Command;

use eyre::{Result, WrapErr, bail};

use crate::cli::BenchCommands;
use crate::ui;

/// The `benchmarks/` directory in the axe checkout this binary was built from.
fn benchmarks_dir() -> Result<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("benchmarks");
    if !dir.is_dir() {
        bail!(
            "benchmarks/ not found at {} — `axe bench` runs the harnesses from an axe checkout",
            dir.display()
        );
    }
    Ok(dir)
}

pub async fn run(subcommand: BenchCommands) -> Result<()> {
    match subcommand {
        BenchCommands::EvmGas { rpc } => evm_gas(rpc),
        BenchCommands::SolanaCu => solana_cu(),
        BenchCommands::All => {
            evm_gas(None)?;
            solana_cu()
        }
    }
}

/// EVM source-gas benchmark: `forge test --mc GasHarness --isolate` on a mainnet
/// fork. `--isolate` meters each top-level call as a real transaction (intrinsic
/// plus calldata plus execution against cold state), so the delta equals a
/// receipt's `gasUsed`.
fn evm_gas(rpc: Option<String>) -> Result<()> {
    let dir = benchmarks_dir()?.join("evm-gas");
    ui::section("EVM source-gas benchmark (GasHarness, mainnet fork, --isolate)");

    let mut cmd = Command::new("forge");
    cmd.current_dir(&dir)
        .args(["test", "--match-contract", "GasHarness", "--isolate", "-vv"]);
    if let Some(url) = rpc.or_else(|| std::env::var("MAINNET_RPC_URL").ok()) {
        cmd.env("MAINNET_RPC_URL", url);
    } else {
        ui::warn("no --rpc / MAINNET_RPC_URL set; the harness will use its public-node default");
    }

    let status = cmd
        .status()
        .wrap_err("running `forge test` — is Foundry installed and on PATH?")?;
    if !status.success() {
        bail!("forge test failed (see output above)");
    }

    ui::section("Maps to global.toml");
    for line in [
        "[cost.source_gas_units]  gmp / gmpWithToken / itsTransfer /",
        "                         itsTransferWithCall / itsDeployment",
        "  = the per-operation `gasUsed` rows above (the source-chain tx the",
        "    caller's wallet pays; per-chain deviations go in chains/<id>.toml",
        "    source_gas_units overrides).",
    ] {
        println!("  {line}");
    }
    Ok(())
}

/// Solana compute-unit benchmark: `cargo test -p its-cu-harness`. Fetches the
/// mainnet program binaries first when they are absent (they are not committed).
fn solana_cu() -> Result<()> {
    let dir = benchmarks_dir()?.join("solana-cu");
    let testdata = dir.join("tests/testdata");

    // Every program the harness loads, so adding one re-fetches for a checkout
    // that already holds the older set.
    const PROGRAMS: [&str; 5] = [
        "its.so",
        "gateway.so",
        "gas_service.so",
        "native_unwrapper.so",
        "mpl_token_metadata.so",
    ];
    if !PROGRAMS.iter().all(|name| testdata.join(name).is_file()) {
        ui::info("Solana program binaries missing — fetching from mainnet...");
        let status = Command::new("bash")
            .arg(dir.join("scripts/fetch-testdata.sh"))
            .status()
            .wrap_err("running fetch-testdata.sh — is the `solana` CLI installed?")?;
        if !status.success() {
            bail!("fetch-testdata.sh failed (see output above)");
        }
    }

    ui::section("Solana compute-unit benchmark (its-cu-harness, LiteSVM, mainnet .so)");
    let status = Command::new("cargo")
        .current_dir(&dir)
        .args(["test", "-p", "its-cu-harness", "--", "--nocapture"])
        .status()
        .wrap_err("running `cargo test` for the CU harness")?;
    if !status.success() {
        bail!("cargo test failed (see output above)");
    }

    ui::section("Maps to global.toml");
    for line in [
        "These numbers are NOT the config values. This harness runs on minimal",
        "injected state and lands ~25% under real mainnet; use it for the per-CPI",
        "breakdown and as a redeploy regression check.",
        "",
        "For the fee-api's [cost.solana] budgets, read the CHARGED max column of:",
        "  python3 benchmarks/solana-cu/scripts/mainnet_cu_limits.py",
        "",
        "which reports the compute-unit limits real mainnet transactions carry —",
        "what Solana actually charges, since unused units are never refunded.",
    ] {
        println!("  {line}");
    }
    Ok(())
}
