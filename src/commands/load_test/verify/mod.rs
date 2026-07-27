use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use alloy::primitives::{Address, keccak256};
use alloy::providers::Provider;
use eyre::{Result, WrapErr};
use tokio::sync::mpsc;

use super::metrics::{AmplifierTiming, PeakThroughput, TxMetrics, VerificationReport};
use crate::config::ChainsConfig;
use crate::cosmos::read_axelar_rpc;
use crate::evm::AxelarAmplifierGateway;
use crate::solana::solana_call_contract_index;
use crate::types::Network;
use crate::ui;

/// If no transaction completes a phase for this long, we stop waiting.
/// Resets every time a tx makes progress, so large batches naturally get more time.
// Legacy/consensus relayers batch the destination approval and can lag far
// behind the source send: on-chain triage of timed-out routes measured real
// approve→execute latencies up to 3226s (mantle → kava GMP), with several
// other legacy GMP routes in the 1200–2950s band. The previous 1000s buffer
// fired before those genuinely-executing routes landed and wrongly marked them
// `timed out`. 7200s gives ~2.2x headroom over the worst observed latency
// while still surfacing a truly stuck route within a couple of hours. Resets on
// any per-phase progress, so large batches naturally get more time.
const INACTIVITY_TIMEOUT: Duration = Duration::from_secs(7200);
/// Delay between poll attempts.
const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Interval for recalculating rolling throughput.
const THROUGHPUT_WINDOW: Duration = Duration::from_secs(10);

/// How many destination blocks before verification-start to include in the
/// legacy `ContractCallApproved` log scan. Verification begins right after the
/// source send and a cross-chain approval takes far longer than this to land,
/// so a small lookback is purely a safety margin against timing/reorg edges
/// while keeping the `eth_getLogs` range small.
const LEGACY_LOG_LOOKBACK_BLOCKS: u64 = 200;

mod checks;
mod its_deploy;
mod legacy;
mod pipeline;
mod report;
mod state;

use self::pipeline::{
    DestinationChecker, ItsEvmDest, ItsHubDest, PollItsHubArgs, PollItsHubEvmArgs,
    PollPipelineArgs, parse_payload_hash, poll_pipeline, poll_pipeline_its_hub,
    poll_pipeline_its_hub_evm,
};
use self::report::compute_verification_report;
use self::state::Phase;

// Re-export `PendingTx` to the parent `load_test` module so the per-pair
// runners can receive it back from the `tx_to_pending_*` constructors and
// forward it through their verifier mpsc channels.
pub(in crate::commands::load_test) use self::state::PendingTx;

// Re-export the ITS remote-deploy waiters so callers can keep using them as
// `super::verify::wait_for_its_remote_deploy*` after the move.
pub use self::its_deploy::{
    StellarRemoteDeployWait, wait_for_its_remote_deploy, wait_for_its_remote_deploy_to_solana,
    wait_for_its_remote_deploy_to_stellar,
};

// ---------------------------------------------------------------------------
// Shared inner helpers (private)
// ---------------------------------------------------------------------------

/// Indices of confirmed transactions in a metrics slice.
fn confirmed_indices(metrics: &[TxMetrics]) -> Vec<usize> {
    metrics
        .iter()
        .enumerate()
        .filter(|(_, m)| m.success && !m.signature.is_empty())
        .map(|(i, _)| i)
        .collect()
}

/// Streaming or batch dispatch for the polling pipelines.
enum VerifyMode<'a> {
    Batch,
    Stream {
        rx: &'a mut mpsc::UnboundedReceiver<PendingTx>,
        send_done: &'a AtomicBool,
        spinner: indicatif::ProgressBar,
    },
}

impl<'a> VerifyMode<'a> {
    fn parts(
        self,
    ) -> (
        Option<&'a mut mpsc::UnboundedReceiver<PendingTx>>,
        Option<&'a AtomicBool>,
        Option<indicatif::ProgressBar>,
    ) {
        match self {
            VerifyMode::Batch => (None, None, None),
            VerifyMode::Stream {
                rx,
                send_done,
                spinner,
            } => (Some(rx), Some(send_done), Some(spinner)),
        }
    }
}

/// Axelar config loaded for GMP verification.
struct GmpAxelarConfig {
    lcd: String,
    voting_verifier: Option<String>,
    cosm_gateway: String,
}

fn load_gmp_axelar_config(
    config: &Path,
    source_chain: &str,
    destination_chain: &str,
) -> Result<GmpAxelarConfig> {
    let cfg = ChainsConfig::load(config)?;
    let (lcd, _, _, _) = cfg.axelar.cosmos_tx_params()?;
    let voting_verifier = cfg
        .axelar
        .contract_address("VotingVerifier", source_chain)
        .ok()
        .map(String::from);
    let cosm_gateway = cfg
        .axelar
        .contract_address("Gateway", destination_chain)?
        .to_string();
    Ok(GmpAxelarConfig {
        lcd,
        voting_verifier,
        cosm_gateway,
    })
}

/// Axelar config loaded for ITS-via-hub verification. Does not include the
/// Tendermint `rpc` field — callers fetch `read_axelar_rpc` separately at
/// the original call site so the ordering relative to tx construction is
/// preserved verbatim. The `cfg` field is returned so the same config object
/// can be reused for the destination `Gateway` lookup.
struct ItsAxelarConfig {
    cfg: ChainsConfig,
    lcd: String,
    voting_verifier: Option<String>,
    axelarnet_gateway: String,
}

fn load_its_axelar_config(config: &Path, source_chain: &str) -> Result<ItsAxelarConfig> {
    let cfg = ChainsConfig::load(config)?;
    let (lcd, _, _, _) = cfg.axelar.cosmos_tx_params()?;
    let voting_verifier = cfg
        .axelar
        .contract_address("VotingVerifier", source_chain)
        .ok()
        .map(String::from);
    let axelarnet_gateway = cfg
        .axelar
        .global_contract_address("AxelarnetGateway")?
        .to_string();
    Ok(ItsAxelarConfig {
        cfg,
        lcd,
        voting_verifier,
        axelarnet_gateway,
    })
}

/// Look up the destination cosmos `Gateway` for a chain.
fn lookup_cosm_gateway_dest(cfg: &ChainsConfig, destination_chain: &str) -> Result<String> {
    Ok(cfg
        .axelar
        .contract_address("Gateway", destination_chain)?
        .to_string())
}

/// Look up the destination cosmos `Gateway` for an XRPL chain, falling back
/// to `XrplGateway` for deployments that use that contract name.
fn lookup_xrpl_cosm_gateway_dest(cfg: &ChainsConfig, destination_chain: &str) -> Result<String> {
    Ok(cfg
        .axelar
        .contract_address("Gateway", destination_chain)
        .or_else(|_| {
            cfg.axelar
                .contract_address("XrplGateway", destination_chain)
        })?
        .to_string())
}

/// Args bundle for [`run_gmp_pipeline`].
struct RunGmpArgs {
    lcd: String,
    voting_verifier: Option<String>,
    cosm_gateway: String,
    source_chain: String,
    destination_chain: String,
    destination_address: String,
    network: Network,
}

/// Drive the GMP polling pipeline (both batch and streaming modes).
async fn run_gmp_pipeline<P: Provider>(
    txs: &mut Vec<PendingTx>,
    checker: &DestinationChecker<'_, P>,
    mode: VerifyMode<'_>,
    args: RunGmpArgs,
) -> Result<PeakThroughput> {
    let RunGmpArgs {
        lcd,
        voting_verifier,
        cosm_gateway,
        source_chain,
        destination_chain,
        destination_address,
        network,
    } = args;
    let (rx, send_done, spinner) = mode.parts();
    poll_pipeline(
        txs,
        rx,
        send_done,
        checker,
        spinner,
        PollPipelineArgs {
            lcd,
            voting_verifier,
            cosm_gateway: Some(cosm_gateway),
            source_chain,
            destination_chain,
            destination_address,
            axelarnet_gateway: None,
            display_chain: None,
            network,
        },
    )
    .await
}

/// Args bundle for [`run_its_hub_pipeline`].
struct RunItsHubArgs {
    lcd: String,
    voting_verifier: Option<String>,
    source_chain: String,
    axelarnet_gateway: String,
    rpc: String,
    cosm_gateway_dest: String,
    dest: ItsHubDest,
    network: Network,
}

/// Drive the ITS-via-hub polling pipeline (both batch and streaming modes).
async fn run_its_hub_pipeline(
    txs: &mut Vec<PendingTx>,
    mode: VerifyMode<'_>,
    args: RunItsHubArgs,
) -> Result<PeakThroughput> {
    let RunItsHubArgs {
        lcd,
        voting_verifier,
        source_chain,
        axelarnet_gateway,
        rpc,
        cosm_gateway_dest,
        dest,
        network,
    } = args;
    let (rx, send_done, spinner) = mode.parts();
    poll_pipeline_its_hub(
        txs,
        rx,
        send_done,
        spinner,
        PollItsHubArgs {
            lcd,
            voting_verifier,
            source_chain,
            axelarnet_gateway,
            rpc,
            cosm_gateway_dest,
            dest,
            network,
        },
    )
    .await
}

/// Args bundle for [`run_its_hub_evm_pipeline`].
struct RunItsHubEvmArgs {
    lcd: String,
    voting_verifier: Option<String>,
    source_chain: String,
    axelarnet_gateway: String,
    rpc: String,
    cosm_gateway_dest: String,
    destination_chain: String,
    dest: ItsEvmDest,
    network: Network,
}

/// Drive the ITS-via-hub polling pipeline with an EVM destination
/// (both batch and streaming modes).
async fn run_its_hub_evm_pipeline<P: Provider>(
    txs: &mut Vec<PendingTx>,
    gw_contract: &AxelarAmplifierGateway::AxelarAmplifierGatewayInstance<&P>,
    mode: VerifyMode<'_>,
    args: RunItsHubEvmArgs,
) -> Result<PeakThroughput> {
    let RunItsHubEvmArgs {
        lcd,
        voting_verifier,
        source_chain,
        axelarnet_gateway,
        rpc,
        cosm_gateway_dest,
        destination_chain,
        dest,
        network,
    } = args;
    let (rx, send_done, spinner) = mode.parts();
    poll_pipeline_its_hub_evm(
        txs,
        rx,
        send_done,
        gw_contract,
        spinner,
        PollItsHubEvmArgs {
            lcd,
            voting_verifier,
            source_chain,
            axelarnet_gateway,
            rpc,
            cosm_gateway_dest,
            _destination_chain: destination_chain,
            dest,
            network,
        },
    )
    .await
}

/// Build the `(report, timings)` tuple returned by every streaming entry.
fn streaming_report_and_timings(
    txs: &[PendingTx],
    peaks: PeakThroughput,
) -> (VerificationReport, Vec<(String, AmplifierTiming)>) {
    let report = compute_verification_report(txs, &mut [], peaks);
    let timings: Vec<(String, AmplifierTiming)> = txs
        .iter()
        .map(|tx| (tx.message_id.clone(), tx.timing.clone()))
        .collect();
    (report, timings)
}

fn parse_first_leg_payload_hash(
    tx: &TxMetrics,
    required: bool,
) -> Result<Option<alloy::primitives::FixedBytes<32>>> {
    if tx.payload_hash.is_empty() {
        if required {
            return Err(eyre::eyre!(
                "missing payload_hash for confirmed tx {}",
                tx.signature
            ));
        }
        return Ok(None);
    }
    parse_payload_hash(&tx.payload_hash)
        .map(Some)
        .wrap_err_with(|| format!("invalid payload_hash for confirmed tx {}", tx.signature))
}

/// Build a `PendingTx` for an ITS-via-hub batch entry. The four ITS batch
/// orchestrators (`verify_onchain_{solana,stellar,xrpl,evm}_its`) share the
/// same struct literal — only the starting `phase` differs, which the caller
/// computes from `voting_verifier.is_some()` (or hardcodes for EVM-ITS).
fn pending_tx_for_its_batch(tx: &TxMetrics, idx: usize, initial_phase: Phase) -> Result<PendingTx> {
    let payload_hash = parse_first_leg_payload_hash(tx, initial_phase == Phase::Voted)?;
    Ok(PendingTx {
        idx,
        message_id: tx.signature.clone(),
        send_instant: tx.send_instant.unwrap_or_else(Instant::now),
        source_address: tx.source_address.clone(),
        contract_addr: Address::ZERO,
        payload_hash,
        payload_hash_hex: tx.payload_hash.clone(),
        command_id: None,
        gmp_destination_chain: tx.gmp_destination_chain.clone(),
        gmp_destination_address: tx.gmp_destination_address.clone(),
        timing: AmplifierTiming::default(),
        failed: false,
        fail_reason: None,
        phase: initial_phase,
        second_leg_message_id: None,
        second_leg_payload_hash: None,
        second_leg_source_address: None,
        second_leg_destination_address: None,
        recovered_via_api: false,
    })
}

/// Compute the source-side `message_id` from a confirmed `TxMetrics` based on
/// the source chain family. EVM/Stellar/Sui pre-format the id in
/// `tx.signature`; SVM appends the `call_contract` log index.
///
/// For SVM, ITS-source paths (its_sol_to_evm, its_sol_to_sui) pre-format the
/// signature via `solana::extract_its_message_id` because the gateway CPI's
/// inner-instruction index varies per tx (observed `-1.7` in production vs
/// the static `-2.1` shape). Detect that by the `-` separator and pass
/// through; otherwise fall back to the synthetic format for raw GMP paths.
fn message_id_for_source(tx: &TxMetrics, source_type: SourceChainType, network: Network) -> String {
    match source_type {
        SourceChainType::Evm | SourceChainType::Stellar | SourceChainType::Sui => {
            tx.signature.clone()
        }
        SourceChainType::Svm => {
            if tx.signature.contains('-') {
                tx.signature.clone()
            } else {
                format!("{}-{}.1", tx.signature, solana_call_contract_index(network))
            }
        }
    }
}

/// Build a `PendingTx` for a GMP batch entry. The four GMP batch orchestrators
/// vary by destination chain — `contract_addr` (parsed for EVM, zero
/// elsewhere), `command_id` (`Some` for Solana, `None` elsewhere), and the
/// `gmp_destination_*` fields — so the caller passes those explicitly.
struct PendingGmpBatchArgs<'a> {
    tx: &'a TxMetrics,
    idx: usize,
    message_id: String,
    contract_addr: Address,
    command_id: Option<[u8; 32]>,
    gmp_destination_chain: String,
    gmp_destination_address: String,
    initial_phase: Phase,
}

fn pending_tx_for_gmp_batch(args: PendingGmpBatchArgs<'_>) -> Result<PendingTx> {
    let PendingGmpBatchArgs {
        tx,
        idx,
        message_id,
        contract_addr,
        command_id,
        gmp_destination_chain,
        gmp_destination_address,
        initial_phase,
    } = args;
    let payload_hash = parse_first_leg_payload_hash(tx, true)?;
    Ok(PendingTx {
        idx,
        message_id,
        send_instant: tx.send_instant.unwrap_or_else(Instant::now),
        source_address: tx.source_address.clone(),
        contract_addr,
        payload_hash,
        payload_hash_hex: tx.payload_hash.clone(),
        command_id,
        gmp_destination_chain,
        gmp_destination_address,
        timing: AmplifierTiming::default(),
        failed: false,
        fail_reason: None,
        phase: initial_phase,
        second_leg_message_id: None,
        second_leg_payload_hash: None,
        second_leg_source_address: None,
        second_leg_destination_address: None,
        recovered_via_api: false,
    })
}

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// Source chain type — determines how message IDs are constructed.
#[derive(Clone, Copy)]
pub enum SourceChainType {
    /// Solana source: message ID = `{signature}-{group}.{index}`
    Svm,
    /// EVM source: message ID = `{tx_hash}-{event_index}` (already in tx.signature)
    Evm,
    /// Stellar source: message ID = `0x{lowercase_tx_hash}-{event_index}`
    Stellar,
    /// Sui source: message ID = `{base58_tx_digest}-{event_index}`
    Sui,
}

/// Verify transactions on-chain through 4 Amplifier pipeline checkpoints:
///
/// 1. **Voted** — VotingVerifier verification (source chain)
/// 2. **Routed** — Destination Gateway outgoing_messages
/// 3. **Approved** — EVM gateway isMessageApproved
/// 4. **Executed** — EVM approval consumed
#[expect(
    clippy::too_many_arguments,
    reason = "route adapter stays explicit until verification phase state is characterized and typed"
)]
pub async fn verify_onchain<P: Provider>(
    config: &Path,
    source_chain: &str,
    destination_chain: &str,
    destination_address: &str,
    gateway_addr: Address,
    provider: &P,
    metrics: &mut [TxMetrics],
    source_type: SourceChainType,
    network: Network,
) -> Result<VerificationReport> {
    let confirmed = confirmed_indices(metrics);
    let total = confirmed.len();
    if total == 0 {
        ui::warn("no confirmed transactions to verify");
        return Ok(VerificationReport::default());
    }

    // A legacy (consensus) EVM destination has no Cosmos Gateway and is verified
    // on its on-chain gateway; delegate to the legacy verifier. This covers every
    // GMP-with-EVM-dest source (EVM, Solana, Sui, Stellar).
    let cfg = ChainsConfig::load(config)?;
    if cfg
        .axelar
        .contract_address("VotingVerifier", destination_chain)
        .is_err()
    {
        return verify_onchain_evm_legacy(
            config,
            source_chain,
            destination_chain,
            destination_address,
            gateway_addr,
            provider,
            metrics,
            network,
            source_type,
        )
        .await;
    }

    let GmpAxelarConfig {
        lcd,
        voting_verifier,
        cosm_gateway,
    } = load_gmp_axelar_config(config, source_chain, destination_chain)?;

    let gw_contract = AxelarAmplifierGateway::new(gateway_addr, provider);
    let contract_addr: Address = destination_address.parse()?;

    let initial_phase = if voting_verifier.is_some() {
        Phase::Voted
    } else {
        Phase::Routed
    };

    let mut txs: Vec<PendingTx> = confirmed
        .iter()
        .map(|&idx| {
            let tx = &metrics[idx];
            pending_tx_for_gmp_batch(PendingGmpBatchArgs {
                tx,
                idx,
                message_id: message_id_for_source(tx, source_type, network),
                contract_addr,
                command_id: None, // EVM destination, not needed
                gmp_destination_chain: String::new(),
                gmp_destination_address: String::new(),
                initial_phase,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let checker = DestinationChecker::Evm {
        gw_contract: &gw_contract,
        // Amplifier→amplifier: the tx only reaches Approved after `routed`, so
        // the fast-path (unapproved ⇒ executed-between-polls) is sound.
        require_observed_approval: false,
    };

    let peaks = run_gmp_pipeline(
        &mut txs,
        &checker,
        VerifyMode::Batch,
        RunGmpArgs {
            lcd,
            voting_verifier,
            cosm_gateway,
            source_chain: source_chain.to_string(),
            destination_chain: destination_chain.to_string(),
            destination_address: destination_address.to_string(),
            network,
        },
    )
    .await?;

    Ok(compute_verification_report(&txs, metrics, peaks))
}

/// Classify each end of an EVM route as legacy (consensus) or Amplifier, by the
/// presence of a per-chain `VotingVerifier` in the Amplifier config. Returns
/// `(source_legacy, destination_legacy)`.
pub(in crate::commands::load_test) fn classify_route(
    cfg: &ChainsConfig,
    source_axelar_id: &str,
    destination_axelar_id: &str,
) -> (bool, bool) {
    let src_legacy = cfg
        .axelar
        .contract_address("VotingVerifier", source_axelar_id)
        .is_err();
    let dst_legacy = cfg
        .axelar
        .contract_address("VotingVerifier", destination_axelar_id)
        .is_err();
    (src_legacy, dst_legacy)
}

/// Verify GMP messages for a route where at least one end is a legacy
/// (consensus) chain. Burst mode only.
///
/// Phase model (orthogonal to the Amplifier path):
/// - `voted` runs only when the **source** is Amplifier (has a `VotingVerifier`).
/// - `routed` is skipped — a legacy-touching route has no observable Amplifier
///   router leg, so `cosm_gateway` is `None` and the voted→next transition goes
///   straight to `Approved`.
/// - the destination checker is chosen by the **destination** type: a legacy
///   gateway (`EvmLegacy`: read the on-chain `ContractCallApproved` commandId,
///   then `isCommandExecuted`) or the Amplifier gateway (`isMessageApproved`).
#[expect(
    clippy::too_many_arguments,
    reason = "route adapter stays explicit until verification phase state is characterized and typed"
)]
pub async fn verify_onchain_evm_legacy<P: Provider>(
    config: &Path,
    source_chain: &str,
    destination_chain: &str,
    destination_address: &str,
    gateway_addr: Address,
    provider: &P,
    metrics: &mut [TxMetrics],
    network: Network,
    source_type: SourceChainType,
) -> Result<VerificationReport> {
    let confirmed = confirmed_indices(metrics);
    if confirmed.is_empty() {
        ui::warn("no confirmed transactions to verify");
        return Ok(VerificationReport::default());
    }
    // EVM source ⇒ match the dest approval by the exact sourceTxHash; a non-EVM
    // source has no EVM tx hash, so match by the unique payloadHash.
    let match_by_payload = !matches!(source_type, SourceChainType::Evm);

    let cfg = ChainsConfig::load(config)?;
    // Amplifier source ⇒ observe the VotingVerifier `voted` phase; otherwise
    // skip straight to the destination check.
    let voting_verifier = cfg
        .axelar
        .contract_address("VotingVerifier", source_chain)
        .ok()
        .map(String::from);
    let dest_legacy = cfg
        .axelar
        .contract_address("VotingVerifier", destination_chain)
        .is_err();
    // `lcd` is only used by the `voted` phase; a pure-consensus route needs none.
    let lcd = if voting_verifier.is_some() {
        cfg.axelar.cosmos_tx_params()?.0
    } else {
        String::new()
    };

    let gw_contract = AxelarAmplifierGateway::new(gateway_addr, provider);
    let contract_addr: Address = destination_address.parse()?;
    let from_block = provider
        .get_block_number()
        .await?
        .saturating_sub(LEGACY_LOG_LOOKBACK_BLOCKS);

    let initial_phase = if voting_verifier.is_some() {
        Phase::Voted
    } else {
        Phase::Approved
    };

    let mut txs: Vec<PendingTx> = confirmed
        .iter()
        .map(|&idx| {
            let tx = &metrics[idx];
            pending_tx_for_gmp_batch(PendingGmpBatchArgs {
                tx,
                idx,
                message_id: message_id_for_source(tx, source_type, network),
                contract_addr,
                command_id: None,
                gmp_destination_chain: String::new(),
                gmp_destination_address: String::new(),
                initial_phase,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let checker = if dest_legacy {
        DestinationChecker::EvmLegacy {
            gw_contract: &gw_contract,
            from_block,
            match_by_payload,
        }
    } else {
        // Amplifier destination reached from a consensus source: there is no
        // `routed` gate, so the message enters the Approved phase immediately.
        // Require an observed approval before concluding execution, else the
        // first (unapproved) poll would false-positive.
        DestinationChecker::Evm {
            gw_contract: &gw_contract,
            require_observed_approval: true,
        }
    };

    let peaks = poll_pipeline(
        &mut txs,
        None,
        None,
        &checker,
        None,
        PollPipelineArgs {
            lcd,
            voting_verifier,
            cosm_gateway: None,
            source_chain: source_chain.to_string(),
            destination_chain: destination_chain.to_string(),
            destination_address: destination_address.to_string(),
            axelarnet_gateway: None,
            display_chain: None,
            network,
        },
    )
    .await?;

    Ok(compute_verification_report(&txs, metrics, peaks))
}

/// Streaming version of `verify_onchain` for EVM destinations — runs
/// concurrently with the send phase, receiving confirmed txs via the channel.
#[expect(
    clippy::too_many_arguments,
    reason = "route adapter stays explicit until verification phase state is characterized and typed"
)]
pub async fn verify_onchain_evm_streaming(
    config: &Path,
    source_chain: &str,
    destination_chain: &str,
    network: Network,
    destination_address: &str,
    gateway_addr: Address,
    evm_rpc_url: &str,
    rx: mpsc::UnboundedReceiver<PendingTx>,
    send_done: Arc<AtomicBool>,
    spinner: indicatif::ProgressBar,
) -> Result<(VerificationReport, Vec<(String, AmplifierTiming)>)> {
    let GmpAxelarConfig {
        lcd,
        voting_verifier,
        cosm_gateway,
    } = load_gmp_axelar_config(config, source_chain, destination_chain)?;

    let provider = alloy::providers::ProviderBuilder::new().connect_http(evm_rpc_url.parse()?);
    let gw_contract = AxelarAmplifierGateway::new(gateway_addr, &provider);

    let checker = DestinationChecker::Evm {
        gw_contract: &gw_contract,
        // Amplifier→amplifier streaming: same as the burst path — the fast-path
        // is sound because Approved is only reached after `routed`.
        require_observed_approval: false,
    };

    let mut txs: Vec<PendingTx> = Vec::new();
    let mut rx = rx;

    let peaks = run_gmp_pipeline(
        &mut txs,
        &checker,
        VerifyMode::Stream {
            rx: &mut rx,
            send_done: &send_done,
            spinner,
        },
        RunGmpArgs {
            lcd,
            voting_verifier,
            cosm_gateway,
            source_chain: source_chain.to_string(),
            destination_chain: destination_chain.to_string(),
            destination_address: destination_address.to_string(),
            network,
        },
    )
    .await?;

    Ok(streaming_report_and_timings(&txs, peaks))
}

/// Streaming variant of `verify_onchain_evm_legacy` (sustained mode). Same
/// legacy phase model and dest checker, but receives confirmed txs over the
/// channel. `from_block` is captured here, before the first tx arrives, so the
/// `ContractCallApproved` scan has a sound lower bound.
#[expect(
    clippy::too_many_arguments,
    reason = "route adapter stays explicit until verification phase state is characterized and typed"
)]
pub async fn verify_onchain_evm_legacy_streaming(
    config: &Path,
    source_chain: &str,
    destination_chain: &str,
    network: Network,
    destination_address: &str,
    gateway_addr: Address,
    evm_rpc_url: &str,
    rx: mpsc::UnboundedReceiver<PendingTx>,
    send_done: Arc<AtomicBool>,
    spinner: indicatif::ProgressBar,
) -> Result<(VerificationReport, Vec<(String, AmplifierTiming)>)> {
    let cfg = ChainsConfig::load(config)?;
    let voting_verifier = cfg
        .axelar
        .contract_address("VotingVerifier", source_chain)
        .ok()
        .map(String::from);
    let dest_legacy = cfg
        .axelar
        .contract_address("VotingVerifier", destination_chain)
        .is_err();
    let lcd = if voting_verifier.is_some() {
        cfg.axelar.cosmos_tx_params()?.0
    } else {
        String::new()
    };

    let provider = alloy::providers::ProviderBuilder::new().connect_http(evm_rpc_url.parse()?);
    let gw_contract = AxelarAmplifierGateway::new(gateway_addr, &provider);
    let from_block = provider
        .get_block_number()
        .await?
        .saturating_sub(LEGACY_LOG_LOOKBACK_BLOCKS);

    let checker = if dest_legacy {
        DestinationChecker::EvmLegacy {
            gw_contract: &gw_contract,
            from_block,
            // Streaming legacy verification is only wired for an EVM source today.
            match_by_payload: false,
        }
    } else {
        DestinationChecker::Evm {
            gw_contract: &gw_contract,
            require_observed_approval: true,
        }
    };

    let mut txs: Vec<PendingTx> = Vec::new();
    let mut rx = rx;

    let peaks = poll_pipeline(
        &mut txs,
        Some(&mut rx),
        Some(&send_done),
        &checker,
        Some(spinner),
        PollPipelineArgs {
            lcd,
            voting_verifier,
            cosm_gateway: None,
            source_chain: source_chain.to_string(),
            destination_chain: destination_chain.to_string(),
            destination_address: destination_address.to_string(),
            axelarnet_gateway: None,
            display_chain: None,
            network,
        },
    )
    .await?;

    Ok(streaming_report_and_timings(&txs, peaks))
}

/// GMP verification with a Stellar destination — uses Stellar's
/// `is_message_approved` / `is_message_executed` Soroban view calls
/// instead of an EVM gateway or Solana PDA.
#[expect(
    clippy::too_many_arguments,
    reason = "route adapter stays explicit until verification phase state is characterized and typed"
)]
pub async fn verify_onchain_stellar_gmp(
    config: &Path,
    source_chain: &str,
    destination_chain: &str,
    destination_contract: &str,
    stellar_rpc: &str,
    stellar_network_type: &str,
    stellar_gateway: &str,
    signer_pk: [u8; 32],
    metrics: &mut [TxMetrics],
    source_type: SourceChainType,
    network: Network,
) -> Result<VerificationReport> {
    let confirmed = confirmed_indices(metrics);
    if confirmed.is_empty() {
        ui::warn("no confirmed transactions to verify");
        return Ok(VerificationReport::default());
    }

    let GmpAxelarConfig {
        lcd,
        voting_verifier,
        cosm_gateway,
    } = load_gmp_axelar_config(config, source_chain, destination_chain)?;
    let initial_phase = if voting_verifier.is_some() {
        Phase::Voted
    } else {
        Phase::Routed
    };

    let mut txs: Vec<PendingTx> = confirmed
        .iter()
        .map(|&idx| {
            let tx = &metrics[idx];
            pending_tx_for_gmp_batch(PendingGmpBatchArgs {
                tx,
                idx,
                message_id: message_id_for_source(tx, source_type, network),
                contract_addr: Address::ZERO,
                command_id: None,
                gmp_destination_chain: tx.gmp_destination_chain.clone(),
                gmp_destination_address: destination_contract.to_string(),
                initial_phase,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let stellar_client = crate::stellar::StellarClient::new(stellar_rpc, stellar_network_type)?;
    let checker: DestinationChecker<alloy::providers::RootProvider> = DestinationChecker::Stellar {
        client: stellar_client,
        gateway_contract: stellar_gateway.to_string(),
        signer_pk,
        _phantom: std::marker::PhantomData,
    };

    let peaks = run_gmp_pipeline(
        &mut txs,
        &checker,
        VerifyMode::Batch,
        RunGmpArgs {
            lcd,
            voting_verifier,
            cosm_gateway,
            source_chain: source_chain.to_string(),
            destination_chain: destination_chain.to_string(),
            destination_address: destination_contract.to_string(),
            network,
        },
    )
    .await?;

    Ok(compute_verification_report(&txs, metrics, peaks))
}

/// Streaming variant of `verify_onchain_stellar_gmp`.
/// Reserved for future sustained-mode flows (the burst-mode runners use
/// `verify_onchain_stellar_gmp` above today).
#[expect(
    clippy::too_many_arguments,
    dead_code,
    reason = "reserved streaming adapter mirrors the batch path until sustained Stellar GMP is enabled"
)]
pub async fn verify_onchain_stellar_gmp_streaming(
    config: &Path,
    source_chain: &str,
    destination_chain: &str,
    network: Network,
    destination_contract: &str,
    stellar_rpc: &str,
    stellar_network_type: &str,
    stellar_gateway: &str,
    signer_pk: [u8; 32],
    rx: mpsc::UnboundedReceiver<PendingTx>,
    send_done: Arc<AtomicBool>,
    spinner: indicatif::ProgressBar,
) -> Result<(VerificationReport, Vec<(String, AmplifierTiming)>)> {
    let GmpAxelarConfig {
        lcd,
        voting_verifier,
        cosm_gateway,
    } = load_gmp_axelar_config(config, source_chain, destination_chain)?;

    let stellar_client = crate::stellar::StellarClient::new(stellar_rpc, stellar_network_type)?;
    let checker: DestinationChecker<alloy::providers::RootProvider> = DestinationChecker::Stellar {
        client: stellar_client,
        gateway_contract: stellar_gateway.to_string(),
        signer_pk,
        _phantom: std::marker::PhantomData,
    };

    let mut txs: Vec<PendingTx> = Vec::new();
    let mut rx = rx;

    let peaks = run_gmp_pipeline(
        &mut txs,
        &checker,
        VerifyMode::Stream {
            rx: &mut rx,
            send_done: &send_done,
            spinner,
        },
        RunGmpArgs {
            lcd,
            voting_verifier,
            cosm_gateway,
            source_chain: source_chain.to_string(),
            destination_chain: destination_chain.to_string(),
            destination_address: destination_contract.to_string(),
            network,
        },
    )
    .await?;

    Ok(streaming_report_and_timings(&txs, peaks))
}

/// Convert a confirmed TxMetrics into a PendingTx for Solana verification.
pub(super) fn tx_to_pending_solana(
    tx: &TxMetrics,
    idx: usize,
    source_chain: &str,
    has_voting_verifier: bool,
    source_type: SourceChainType,
    network: Network,
    legacy_route: bool,
) -> Result<PendingTx> {
    let payload_hash = parse_first_leg_payload_hash(tx, true)?;
    let message_id = message_id_for_source(tx, source_type, network);
    let cmd_input = [source_chain.as_bytes(), b"-", message_id.as_bytes()].concat();
    Ok(PendingTx {
        idx,
        message_id,
        send_instant: tx.send_instant.unwrap_or_else(Instant::now),
        source_address: tx.source_address.clone(),
        contract_addr: Address::ZERO,
        payload_hash,
        payload_hash_hex: tx.payload_hash.clone(),
        command_id: Some(keccak256(&cmd_input).into()),
        gmp_destination_chain: String::new(),
        gmp_destination_address: String::new(),
        timing: AmplifierTiming::default(),
        failed: false,
        fail_reason: None,
        // Amplifier source ⇒ Voted. A legacy (consensus) source has no
        // VotingVerifier/router, so it skips straight to Approved (the dest
        // checker drives it); a no-VV amplifier path keeps the Routed start.
        phase: match (has_voting_verifier, legacy_route) {
            (true, _) => Phase::Voted,
            (false, true) => Phase::Approved,
            (false, false) => Phase::Routed,
        },
        second_leg_message_id: None,
        second_leg_payload_hash: None,
        second_leg_source_address: None,
        second_leg_destination_address: None,
        recovered_via_api: false,
    })
}

/// Convert a confirmed TxMetrics into a PendingTx for Stellar-sourced GMP.
/// The `signature` field on the input is the pre-formatted message id
/// (`0x{lowercase_hex_tx_hash}-{event_index}`) per the `hex_tx_hash_and_event_index`
/// format of the Stellar `VotingVerifier`.
pub(super) fn tx_to_pending_stellar(
    tx: &TxMetrics,
    has_voting_verifier: bool,
    contract_addr: Address,
) -> Result<PendingTx> {
    let payload_hash = parse_first_leg_payload_hash(tx, true)?;
    Ok(PendingTx {
        idx: 0,
        message_id: tx.signature.clone(),
        send_instant: tx.send_instant.unwrap_or_else(Instant::now),
        source_address: tx.source_address.clone(),
        contract_addr,
        payload_hash,
        payload_hash_hex: tx.payload_hash.clone(),
        command_id: None,
        gmp_destination_chain: String::new(),
        gmp_destination_address: String::new(),
        timing: AmplifierTiming::default(),
        failed: false,
        fail_reason: None,
        phase: if has_voting_verifier {
            Phase::Voted
        } else {
            Phase::Routed
        },
        second_leg_message_id: None,
        second_leg_payload_hash: None,
        second_leg_source_address: None,
        second_leg_destination_address: None,
        recovered_via_api: false,
    })
}

/// Convert a confirmed TxMetrics into a PendingTx for XRPL-sourced ITS
/// verification. The `signature` field on the input is the already-formatted
/// XRPL message id (`0x{lowercase_hex_tx_hash}`), which is what the
/// `XrplVotingVerifier` / `XrplGateway` expect.
pub(super) fn tx_to_pending_xrpl(tx: &TxMetrics, has_voting_verifier: bool) -> Result<PendingTx> {
    let payload_hash = parse_first_leg_payload_hash(tx, has_voting_verifier)?;
    Ok(PendingTx {
        idx: 0,
        message_id: tx.signature.clone(),
        send_instant: tx.send_instant.unwrap_or_else(Instant::now),
        source_address: tx.source_address.clone(),
        contract_addr: Address::ZERO,
        payload_hash,
        payload_hash_hex: tx.payload_hash.clone(),
        command_id: None,
        gmp_destination_chain: tx.gmp_destination_chain.clone(),
        gmp_destination_address: tx.gmp_destination_address.clone(),
        timing: AmplifierTiming::default(),
        failed: false,
        fail_reason: None,
        phase: if has_voting_verifier {
            Phase::Voted
        } else {
            Phase::HubApproved
        },
        second_leg_message_id: None,
        second_leg_payload_hash: None,
        second_leg_source_address: None,
        second_leg_destination_address: None,
        recovered_via_api: false,
    })
}

/// Convert a confirmed TxMetrics into a PendingTx for ITS hub verification.
/// ITS messages route through the hub, so gmp_destination_chain/address are
/// set from the TxMetrics (typically "axelar" / AxelarnetGateway).
pub(super) fn tx_to_pending_its(tx: &TxMetrics, has_voting_verifier: bool) -> Result<PendingTx> {
    let payload_hash = parse_first_leg_payload_hash(tx, has_voting_verifier)?;
    Ok(PendingTx {
        idx: 0,
        message_id: tx.signature.clone(),
        send_instant: tx.send_instant.unwrap_or_else(Instant::now),
        source_address: tx.source_address.clone(),
        contract_addr: Address::ZERO,
        payload_hash,
        payload_hash_hex: tx.payload_hash.clone(),
        command_id: None,
        gmp_destination_chain: tx.gmp_destination_chain.clone(),
        gmp_destination_address: tx.gmp_destination_address.clone(),
        timing: AmplifierTiming::default(),
        failed: false,
        fail_reason: None,
        phase: if has_voting_verifier {
            Phase::Voted
        } else {
            Phase::HubApproved
        },
        second_leg_message_id: None,
        second_leg_payload_hash: None,
        second_leg_source_address: None,
        second_leg_destination_address: None,
        recovered_via_api: false,
    })
}

// ---------------------------------------------------------------------------
// Sui destination verifier (GMP)
// ---------------------------------------------------------------------------

/// Burst-mode Sui destination verifier — block on confirmed metrics array.
/// Uses Sui events polling (`MessageApproved` / `MessageExecuted` on the
/// AxelarGateway events module) for the destination-side phases.
#[expect(
    clippy::too_many_arguments,
    reason = "route adapter stays explicit until verification phase state is characterized and typed"
)]
pub async fn verify_onchain_sui_gmp(
    config: &Path,
    source_chain: &str,
    destination_chain: &str,
    destination_address: &str,
    sui_rpc: &str,
    metrics: &mut [TxMetrics],
    source_type: SourceChainType,
    network: Network,
) -> Result<VerificationReport> {
    let confirmed = confirmed_indices(metrics);
    let total = confirmed.len();
    if total == 0 {
        ui::warn("no confirmed transactions to verify");
        return Ok(VerificationReport::default());
    }

    let GmpAxelarConfig {
        lcd,
        voting_verifier,
        cosm_gateway,
    } = load_gmp_axelar_config(config, source_chain, destination_chain)?;
    let gateway_pkg = crate::sui::read_sui_gateway_pkg(config, destination_chain)?;
    let sui_client = crate::sui::SuiClient::new(sui_rpc);

    let initial_phase = if voting_verifier.is_some() {
        Phase::Voted
    } else {
        Phase::Routed
    };

    let mut txs: Vec<PendingTx> = confirmed
        .iter()
        .map(|&idx| {
            let tx = &metrics[idx];
            // VotingVerifier first-leg destination = the source CallContract
            // event's destination, captured per-tx as gmp_destination_*. For
            // ITS-hub-routed Sui transfers (e.g. sol→sui) that's
            // (axelar, AxelarnetGateway) — NOT the final Sui channel passed
            // as `destination_address`. Using the channel here made the VV
            // `messages_status` query never match (status "unknown") and the
            // verify phase timed out at "voted". Plumb the per-tx hub
            // destination through; for raw Sui-dest GMP it equals the channel.
            pending_tx_for_gmp_batch(PendingGmpBatchArgs {
                tx,
                idx,
                message_id: message_id_for_source(tx, source_type, network),
                contract_addr: Address::ZERO,
                command_id: None,
                gmp_destination_chain: tx.gmp_destination_chain.clone(),
                gmp_destination_address: tx.gmp_destination_address.clone(),
                initial_phase,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let checker: DestinationChecker<'_, alloy::providers::RootProvider> = DestinationChecker::Sui {
        client: sui_client,
        gateway_pkg,
        _phantom: std::marker::PhantomData,
    };

    let peaks = run_gmp_pipeline(
        &mut txs,
        &checker,
        VerifyMode::Batch,
        RunGmpArgs {
            lcd,
            voting_verifier,
            cosm_gateway,
            source_chain: source_chain.to_string(),
            destination_chain: destination_chain.to_string(),
            destination_address: destination_address.to_string(),
            network,
        },
    )
    .await?;

    Ok(compute_verification_report(&txs, metrics, peaks))
}

/// Streaming verification for EVM→Solana in sustained mode.
///
/// Runs verification concurrently with the send phase. Receives confirmed
/// transactions via the channel and starts polling them immediately.
#[expect(
    clippy::too_many_arguments,
    reason = "route adapter stays explicit until verification phase state is characterized and typed"
)]
pub async fn verify_onchain_solana_streaming(
    config: &Path,
    source_chain: &str,
    destination_chain: &str,
    destination_address: &str,
    solana_rpc: &str,
    network: Network,
    mut rx: mpsc::UnboundedReceiver<PendingTx>,
    send_done: Arc<AtomicBool>,
    spinner: indicatif::ProgressBar,
) -> Result<(VerificationReport, Vec<(String, AmplifierTiming)>)> {
    let GmpAxelarConfig {
        lcd,
        voting_verifier,
        cosm_gateway,
    } = load_gmp_axelar_config(config, source_chain, destination_chain)?;

    let rpc_client = Arc::new(solana_client::rpc_client::RpcClient::new_with_commitment(
        solana_rpc,
        solana_commitment_config::CommitmentConfig::finalized(),
    ));

    let checker: DestinationChecker<'_, alloy::providers::RootProvider> =
        DestinationChecker::Solana {
            rpc_client,
            network,
            _phantom: std::marker::PhantomData,
        };

    let mut txs: Vec<PendingTx> = Vec::new();

    let peaks = run_gmp_pipeline(
        &mut txs,
        &checker,
        VerifyMode::Stream {
            rx: &mut rx,
            send_done: &send_done,
            spinner,
        },
        RunGmpArgs {
            lcd,
            voting_verifier,
            cosm_gateway,
            source_chain: source_chain.to_string(),
            destination_chain: destination_chain.to_string(),
            destination_address: destination_address.to_string(),
            network,
        },
    )
    .await?;

    // Key by message_id (signature) since streaming PendingTx idx is always 0.
    Ok(streaming_report_and_timings(&txs, peaks))
}

/// Verify EVM->Solana transactions through the Amplifier pipeline:
///
/// 1. **Voted** — VotingVerifier verification (source EVM chain)
/// 2. **Routed** — Cosmos Gateway outgoing_messages (dest Solana chain)
/// 3. **Approved** — Solana IncomingMessage PDA exists
/// 4. **Executed** — Solana IncomingMessage PDA status = executed
#[expect(
    clippy::too_many_arguments,
    reason = "route adapter stays explicit until verification phase state is characterized and typed"
)]
pub async fn verify_onchain_solana(
    config: &Path,
    source_chain: &str,
    destination_chain: &str,
    destination_address: &str,
    solana_rpc: &str,
    metrics: &mut [TxMetrics],
    source_type: SourceChainType,
    network: Network,
) -> Result<VerificationReport> {
    let confirmed = confirmed_indices(metrics);
    let total = confirmed.len();
    if total == 0 {
        ui::warn("no confirmed transactions to verify");
        return Ok(VerificationReport::default());
    }

    let GmpAxelarConfig {
        lcd,
        voting_verifier,
        cosm_gateway,
    } = load_gmp_axelar_config(config, source_chain, destination_chain)?;

    let initial_phase = if voting_verifier.is_some() {
        Phase::Voted
    } else {
        Phase::Routed
    };

    let mut txs: Vec<PendingTx> = confirmed
        .iter()
        .map(|&idx| {
            let tx = &metrics[idx];
            let message_id = message_id_for_source(tx, source_type, network);
            let cmd_input = [source_chain.as_bytes(), b"-", message_id.as_bytes()].concat();
            // The voting verifier indexes the message by the *outer* GMP
            // destination (which is the Axelar Hub for ITS-routed transfers,
            // a Sui channel for raw GMP). Passing empty strings here made
            // `messages_status` queries silently no-match and the verify
            // phase timed out at "voted". The TxMetrics already captured
            // these from the source-side CallContract event, so plumb them
            // through.
            pending_tx_for_gmp_batch(PendingGmpBatchArgs {
                tx,
                idx,
                message_id,
                contract_addr: Address::ZERO,
                command_id: Some(keccak256(&cmd_input).into()),
                gmp_destination_chain: tx.gmp_destination_chain.clone(),
                gmp_destination_address: tx.gmp_destination_address.clone(),
                initial_phase,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let rpc_client = Arc::new(solana_client::rpc_client::RpcClient::new_with_commitment(
        solana_rpc,
        solana_commitment_config::CommitmentConfig::finalized(),
    ));

    let checker: DestinationChecker<'_, alloy::providers::RootProvider> =
        DestinationChecker::Solana {
            rpc_client,
            network,
            _phantom: std::marker::PhantomData,
        };

    let peaks = run_gmp_pipeline(
        &mut txs,
        &checker,
        VerifyMode::Batch,
        RunGmpArgs {
            lcd,
            voting_verifier,
            cosm_gateway,
            source_chain: source_chain.to_string(),
            destination_chain: destination_chain.to_string(),
            destination_address: destination_address.to_string(),
            network,
        },
    )
    .await?;

    Ok(compute_verification_report(&txs, metrics, peaks))
}

/// Verify EVM->Solana ITS transactions through the Amplifier pipeline.
///
/// ITS messages route via the Axelar hub: the ContractCall event has
/// `destination_chain = "axelar"` and `destination_address = AxelarnetGateway`.
/// The VotingVerifier query must match these event values, not the final
/// destination (solana-18).
///
/// Phases tracked:
/// 1. **Voted** — VotingVerifier (dest = "axelar" / AxelarnetGateway)
/// 2. **Hub Approved** — AxelarnetGateway executable_messages
/// 3. **Discover Second Leg** — find second-leg message_id from hub execution tx
/// 4. **Routed** — Cosmos Gateway outgoing_messages (second-leg)
/// 5. **Approved** — Solana IncomingMessage PDA exists
/// 6. **Executed** — Solana IncomingMessage PDA status = executed
pub async fn verify_onchain_solana_its(
    config: &Path,
    source_chain: &str,
    destination_chain: &str,
    _destination_address: &str,
    solana_rpc: &str,
    network: Network,
    metrics: &mut [TxMetrics],
) -> Result<VerificationReport> {
    let confirmed = confirmed_indices(metrics);
    if confirmed.is_empty() {
        ui::warn("no confirmed transactions to verify");
        return Ok(VerificationReport::default());
    }

    let ItsAxelarConfig {
        cfg,
        lcd,
        voting_verifier,
        axelarnet_gateway,
    } = load_its_axelar_config(config, source_chain)?;

    let initial_phase = if voting_verifier.is_some() {
        Phase::Voted
    } else {
        Phase::HubApproved
    };

    let mut txs: Vec<PendingTx> = confirmed
        .iter()
        .map(|&idx| pending_tx_for_its_batch(&metrics[idx], idx, initial_phase))
        .collect::<Result<Vec<_>>>()?;

    let rpc = read_axelar_rpc(config)?;
    let cosm_gateway_dest = lookup_cosm_gateway_dest(&cfg, destination_chain)?;

    let peaks = run_its_hub_pipeline(
        &mut txs,
        VerifyMode::Batch,
        RunItsHubArgs {
            lcd,
            voting_verifier,
            source_chain: source_chain.to_string(),
            axelarnet_gateway,
            rpc,
            cosm_gateway_dest,
            network,
            dest: ItsHubDest::Solana {
                rpc_url: solana_rpc.to_string(),
                network,
            },
        },
    )
    .await?;

    Ok(compute_verification_report(&txs, metrics, peaks))
}

/// Verify an ITS-via-hub transfer whose **destination is Sui**, batch mode.
///
/// Mirrors [`verify_onchain_solana_its`] but drives the Sui destination
/// through the two-leg hub pipeline ([`ItsHubDest::Sui`]): Voted → HubApproved
/// → DiscoverSecondLeg → Routed → Approved → Executed. The first leg
/// (source→hub) message id comes from `tx.signature` (already formatted by
/// each source sender), and the Sui-side approval/execution events key off the
/// discovered second-leg id with `source_chain = "axelar"`.
///
/// This is the ITS counterpart to [`verify_onchain_sui_gmp`], which handles
/// only single-leg raw GMP to Sui and so can't see the hub→Sui second leg.
pub async fn verify_onchain_sui_its(
    config: &Path,
    source_chain: &str,
    destination_chain: &str,
    network: Network,
    sui_rpc: &str,
    metrics: &mut [TxMetrics],
) -> Result<VerificationReport> {
    let confirmed = confirmed_indices(metrics);
    if confirmed.is_empty() {
        ui::warn("no confirmed transactions to verify");
        return Ok(VerificationReport::default());
    }

    let ItsAxelarConfig {
        cfg,
        lcd,
        voting_verifier,
        axelarnet_gateway,
    } = load_its_axelar_config(config, source_chain)?;
    let gateway_pkg = crate::sui::read_sui_gateway_pkg(config, destination_chain)?;

    let initial_phase = if voting_verifier.is_some() {
        Phase::Voted
    } else {
        Phase::HubApproved
    };

    let mut txs: Vec<PendingTx> = confirmed
        .iter()
        .map(|&idx| pending_tx_for_its_batch(&metrics[idx], idx, initial_phase))
        .collect::<Result<Vec<_>>>()?;

    let rpc = read_axelar_rpc(config)?;
    let cosm_gateway_dest = lookup_cosm_gateway_dest(&cfg, destination_chain)?;

    let peaks = run_its_hub_pipeline(
        &mut txs,
        VerifyMode::Batch,
        RunItsHubArgs {
            lcd,
            voting_verifier,
            source_chain: source_chain.to_string(),
            axelarnet_gateway,
            rpc,
            cosm_gateway_dest,
            network,
            dest: ItsHubDest::Sui {
                rpc_url: sui_rpc.to_string(),
                gateway_pkg,
            },
        },
    )
    .await?;

    Ok(compute_verification_report(&txs, metrics, peaks))
}

/// Streaming version of `verify_onchain_solana_its` — runs concurrently with
/// the send phase, receiving confirmed txs via the channel.
#[expect(
    clippy::too_many_arguments,
    reason = "route adapter stays explicit until verification phase state is characterized and typed"
)]
pub async fn verify_onchain_solana_its_streaming(
    config: &Path,
    source_chain: &str,
    destination_chain: &str,
    solana_rpc: &str,
    network: Network,
    rx: mpsc::UnboundedReceiver<PendingTx>,
    send_done: Arc<AtomicBool>,
    spinner: indicatif::ProgressBar,
) -> Result<(VerificationReport, Vec<(String, AmplifierTiming)>)> {
    let ItsAxelarConfig {
        cfg,
        lcd,
        voting_verifier,
        axelarnet_gateway,
    } = load_its_axelar_config(config, source_chain)?;
    let rpc = read_axelar_rpc(config)?;
    let cosm_gateway_dest = lookup_cosm_gateway_dest(&cfg, destination_chain)?;

    let mut txs: Vec<PendingTx> = Vec::new();
    let mut rx = rx;

    let peaks = run_its_hub_pipeline(
        &mut txs,
        VerifyMode::Stream {
            rx: &mut rx,
            send_done: &send_done,
            spinner,
        },
        RunItsHubArgs {
            lcd,
            voting_verifier,
            source_chain: source_chain.to_string(),
            axelarnet_gateway,
            rpc,
            cosm_gateway_dest,
            network,
            dest: ItsHubDest::Solana {
                rpc_url: solana_rpc.to_string(),
                network,
            },
        },
    )
    .await?;

    Ok(streaming_report_and_timings(&txs, peaks))
}

/// Verify EVM/Solana → Stellar ITS transactions. Mirrors
/// `verify_onchain_solana_its` but uses Stellar's `is_message_approved` /
/// `is_message_executed` view calls to detect destination-side approval and
/// execution. The `signer_pk` is just the source account for simulate
/// envelopes — read-only, no real authorization needed.
#[expect(
    clippy::too_many_arguments,
    reason = "route adapter stays explicit until verification phase state is characterized and typed"
)]
pub async fn verify_onchain_stellar_its(
    config: &Path,
    source_chain: &str,
    destination_chain: &str,
    network: Network,
    _destination_address: &str,
    stellar_rpc: &str,
    stellar_network_type: &str,
    stellar_gateway_contract: &str,
    signer_pk: [u8; 32],
    metrics: &mut [TxMetrics],
) -> Result<VerificationReport> {
    let confirmed = confirmed_indices(metrics);
    if confirmed.is_empty() {
        ui::warn("no confirmed transactions to verify");
        return Ok(VerificationReport::default());
    }

    let ItsAxelarConfig {
        cfg,
        lcd,
        voting_verifier,
        axelarnet_gateway,
    } = load_its_axelar_config(config, source_chain)?;

    let initial_phase = if voting_verifier.is_some() {
        Phase::Voted
    } else {
        Phase::HubApproved
    };

    let mut txs: Vec<PendingTx> = confirmed
        .iter()
        .map(|&idx| pending_tx_for_its_batch(&metrics[idx], idx, initial_phase))
        .collect::<Result<Vec<_>>>()?;

    let rpc = read_axelar_rpc(config)?;
    let cosm_gateway_dest = lookup_cosm_gateway_dest(&cfg, destination_chain)?;

    let peaks = run_its_hub_pipeline(
        &mut txs,
        VerifyMode::Batch,
        RunItsHubArgs {
            lcd,
            voting_verifier,
            source_chain: source_chain.to_string(),
            axelarnet_gateway,
            rpc,
            cosm_gateway_dest,
            network,
            dest: ItsHubDest::Stellar {
                rpc_url: stellar_rpc.to_string(),
                network_type: stellar_network_type.to_string(),
                gateway_contract: stellar_gateway_contract.to_string(),
                signer_pk,
            },
        },
    )
    .await?;

    Ok(compute_verification_report(&txs, metrics, peaks))
}

/// Streaming variant of `verify_onchain_stellar_its`.
#[expect(
    clippy::too_many_arguments,
    reason = "route adapter stays explicit until verification phase state is characterized and typed"
)]
pub async fn verify_onchain_stellar_its_streaming(
    config: &Path,
    source_chain: &str,
    destination_chain: &str,
    network: Network,
    stellar_rpc: &str,
    stellar_network_type: &str,
    stellar_gateway_contract: &str,
    signer_pk: [u8; 32],
    rx: mpsc::UnboundedReceiver<PendingTx>,
    send_done: Arc<AtomicBool>,
    spinner: indicatif::ProgressBar,
) -> Result<(VerificationReport, Vec<(String, AmplifierTiming)>)> {
    let ItsAxelarConfig {
        cfg,
        lcd,
        voting_verifier,
        axelarnet_gateway,
    } = load_its_axelar_config(config, source_chain)?;
    let rpc = read_axelar_rpc(config)?;
    let cosm_gateway_dest = lookup_cosm_gateway_dest(&cfg, destination_chain)?;

    let mut txs: Vec<PendingTx> = Vec::new();
    let mut rx = rx;

    let peaks = run_its_hub_pipeline(
        &mut txs,
        VerifyMode::Stream {
            rx: &mut rx,
            send_done: &send_done,
            spinner,
        },
        RunItsHubArgs {
            lcd,
            voting_verifier,
            source_chain: source_chain.to_string(),
            axelarnet_gateway,
            rpc,
            cosm_gateway_dest,
            network,
            dest: ItsHubDest::Stellar {
                rpc_url: stellar_rpc.to_string(),
                network_type: stellar_network_type.to_string(),
                gateway_contract: stellar_gateway_contract.to_string(),
                signer_pk,
            },
        },
    )
    .await?;

    Ok(streaming_report_and_timings(&txs, peaks))
}

/// Verify EVM/Solana → XRPL ITS transactions. Polls the recipient XRPL
/// account's `account_tx` for an inbound `Payment` whose `message_id` memo
/// matches the second-leg message id (the XRPL relayer attaches that memo).
pub async fn verify_onchain_xrpl_its(
    config: &Path,
    source_chain: &str,
    destination_chain: &str,
    network: Network,
    xrpl_rpc: &str,
    xrpl_recipient: &str,
    metrics: &mut [TxMetrics],
) -> Result<VerificationReport> {
    let confirmed = confirmed_indices(metrics);
    if confirmed.is_empty() {
        ui::warn("no confirmed transactions to verify");
        return Ok(VerificationReport::default());
    }

    let ItsAxelarConfig {
        cfg,
        lcd,
        voting_verifier,
        axelarnet_gateway,
    } = load_its_axelar_config(config, source_chain)?;

    let initial_phase = if voting_verifier.is_some() {
        Phase::Voted
    } else {
        Phase::HubApproved
    };

    let mut txs: Vec<PendingTx> = confirmed
        .iter()
        .map(|&idx| pending_tx_for_its_batch(&metrics[idx], idx, initial_phase))
        .collect::<Result<Vec<_>>>()?;

    let rpc = read_axelar_rpc(config)?;
    // XRPL's destination cosmos gateway is `XrplGateway/{chain}`, not the
    // standard `Gateway/{chain}`. Try both so the same verifier works
    // regardless of which contract name the deployment uses.
    let cosm_gateway_dest = lookup_xrpl_cosm_gateway_dest(&cfg, destination_chain)?;

    let peaks = run_its_hub_pipeline(
        &mut txs,
        VerifyMode::Batch,
        RunItsHubArgs {
            lcd,
            voting_verifier,
            source_chain: source_chain.to_string(),
            axelarnet_gateway,
            rpc,
            cosm_gateway_dest,
            network,
            dest: ItsHubDest::Xrpl {
                rpc_url: xrpl_rpc.to_string(),
                recipient_address: xrpl_recipient.to_string(),
            },
        },
    )
    .await?;

    Ok(compute_verification_report(&txs, metrics, peaks))
}

/// Streaming variant of `verify_onchain_xrpl_its`.
#[expect(
    clippy::too_many_arguments,
    reason = "route adapter stays explicit until verification phase state is characterized and typed"
)]
pub async fn verify_onchain_xrpl_its_streaming(
    config: &Path,
    source_chain: &str,
    destination_chain: &str,
    network: Network,
    xrpl_rpc: &str,
    xrpl_recipient: &str,
    rx: mpsc::UnboundedReceiver<PendingTx>,
    send_done: Arc<AtomicBool>,
    spinner: indicatif::ProgressBar,
) -> Result<(VerificationReport, Vec<(String, AmplifierTiming)>)> {
    let ItsAxelarConfig {
        cfg,
        lcd,
        voting_verifier,
        axelarnet_gateway,
    } = load_its_axelar_config(config, source_chain)?;
    let rpc = read_axelar_rpc(config)?;
    let cosm_gateway_dest = lookup_xrpl_cosm_gateway_dest(&cfg, destination_chain)?;

    let mut txs: Vec<PendingTx> = Vec::new();
    let mut rx = rx;

    let peaks = run_its_hub_pipeline(
        &mut txs,
        VerifyMode::Stream {
            rx: &mut rx,
            send_done: &send_done,
            spinner,
        },
        RunItsHubArgs {
            lcd,
            voting_verifier,
            source_chain: source_chain.to_string(),
            axelarnet_gateway,
            rpc,
            cosm_gateway_dest,
            network,
            dest: ItsHubDest::Xrpl {
                rpc_url: xrpl_rpc.to_string(),
                recipient_address: xrpl_recipient.to_string(),
            },
        },
    )
    .await?;

    Ok(streaming_report_and_timings(&txs, peaks))
}

/// Verify Solana->EVM ITS transactions through the Amplifier pipeline.
///
/// ITS messages route via the Axelar hub: the Solana ITS program CPI's
/// `call_contract` with `destination_chain = "axelar"`.
///
/// Phases tracked:
/// 1. **Voted** — VotingVerifier (dest = "axelar" / AxelarnetGateway)
/// 2. **Hub Approved** — AxelarnetGateway executable_messages
/// 3. **Discover Second Leg** — find second-leg message_id from hub execution tx
/// 4. **Routed** — Cosmos Gateway outgoing_messages (second-leg, dest EVM chain)
/// 5. **Approved** — EVM gateway isMessageApproved (second-leg)
/// 6. **Executed** — EVM approval consumed
#[expect(
    clippy::too_many_arguments,
    reason = "route adapter stays explicit until verification phase state is characterized and typed"
)]
pub async fn verify_onchain_evm_its(
    config: &Path,
    source_chain: &str,
    destination_chain: &str,
    network: Network,
    _destination_address: &str,
    evm_gateway_addr: Address,
    evm_rpc_url: &str,
    metrics: &mut [TxMetrics],
) -> Result<VerificationReport> {
    let confirmed = confirmed_indices(metrics);
    if confirmed.is_empty() {
        ui::warn("no confirmed transactions to verify");
        return Ok(VerificationReport::default());
    }

    let cfg = ChainsConfig::load(config)?;
    let (lcd, _, _, _) = cfg.axelar.cosmos_tx_params()?;

    let axelarnet_gateway = cfg
        .axelar
        .global_contract_address("AxelarnetGateway")?
        .to_string();

    // For Solana ITS, we don't have the payload_hash (the ITS program constructs
    // the payload internally via CPI). Skip VotingVerifier and start at HubApproved,
    // which only needs source_chain + message_id. HubApproved implies voted.
    let initial_phase = Phase::HubApproved;
    let mut txs: Vec<PendingTx> = confirmed
        .iter()
        .map(|&idx| pending_tx_for_its_batch(&metrics[idx], idx, initial_phase))
        .collect::<Result<Vec<_>>>()?;

    let rpc = read_axelar_rpc(config)?;

    let provider = alloy::providers::ProviderBuilder::new().connect_http(evm_rpc_url.parse()?);
    let gw_contract = AxelarAmplifierGateway::new(evm_gateway_addr, &provider);

    // A legacy (consensus) destination has no Cosmos Gateway: skip the `routed`
    // phase and verify the second leg on the legacy gateway via events.
    let (dest, cosm_gateway_dest) = its_evm_dest(&cfg, destination_chain, &provider).await?;

    let peaks = run_its_hub_evm_pipeline(
        &mut txs,
        &gw_contract,
        VerifyMode::Batch,
        RunItsHubEvmArgs {
            lcd,
            voting_verifier: None, // skip VotingVerifier — no payload_hash for Solana ITS
            source_chain: source_chain.to_string(),
            axelarnet_gateway,
            rpc,
            cosm_gateway_dest,
            destination_chain: destination_chain.to_string(),
            dest,
            network,
        },
    )
    .await?;

    Ok(compute_verification_report(&txs, metrics, peaks))
}

/// Resolve the ITS EVM destination descriptor + the Cosmos Gateway address the
/// `routed` phase needs. Amplifier dest → `(Amplifier, <cosm gateway>)`; legacy
/// dest → `(Legacy { from_block }, "")` (no Cosmos Gateway, `routed` skipped).
async fn its_evm_dest<P: Provider>(
    cfg: &ChainsConfig,
    destination_chain: &str,
    provider: &P,
) -> Result<(ItsEvmDest, String)> {
    let dest_legacy = cfg
        .axelar
        .contract_address("VotingVerifier", destination_chain)
        .is_err();
    if dest_legacy {
        let from_block = provider
            .get_block_number()
            .await?
            .saturating_sub(LEGACY_LOG_LOOKBACK_BLOCKS);
        Ok((ItsEvmDest::Legacy { from_block }, String::new()))
    } else {
        Ok((
            ItsEvmDest::Amplifier,
            lookup_cosm_gateway_dest(cfg, destination_chain)?,
        ))
    }
}

/// Streaming version of `verify_onchain_evm_its` — runs concurrently with
/// the send phase, receiving confirmed txs via the channel.
#[expect(
    clippy::too_many_arguments,
    reason = "route adapter stays explicit until verification phase state is characterized and typed"
)]
pub async fn verify_onchain_evm_its_streaming(
    config: &Path,
    source_chain: &str,
    destination_chain: &str,
    network: Network,
    evm_gateway_addr: Address,
    evm_rpc_url: &str,
    rx: mpsc::UnboundedReceiver<PendingTx>,
    send_done: Arc<AtomicBool>,
    spinner: indicatif::ProgressBar,
) -> Result<(VerificationReport, Vec<(String, AmplifierTiming)>)> {
    let cfg = ChainsConfig::load(config)?;
    let (lcd, _, _, _) = cfg.axelar.cosmos_tx_params()?;

    let axelarnet_gateway = cfg
        .axelar
        .global_contract_address("AxelarnetGateway")?
        .to_string();

    let rpc = read_axelar_rpc(config)?;

    let provider = alloy::providers::ProviderBuilder::new().connect_http(evm_rpc_url.parse()?);
    let gw_contract = AxelarAmplifierGateway::new(evm_gateway_addr, &provider);

    let (dest, cosm_gateway_dest) = its_evm_dest(&cfg, destination_chain, &provider).await?;

    let mut txs: Vec<PendingTx> = Vec::new();
    let mut rx = rx;

    let peaks = run_its_hub_evm_pipeline(
        &mut txs,
        &gw_contract,
        VerifyMode::Stream {
            rx: &mut rx,
            send_done: &send_done,
            spinner,
        },
        RunItsHubEvmArgs {
            lcd,
            voting_verifier: None, // skip VotingVerifier — Solana ITS has no payload_hash
            source_chain: source_chain.to_string(),
            axelarnet_gateway,
            rpc,
            cosm_gateway_dest,
            destination_chain: destination_chain.to_string(),
            dest,
            network,
        },
    )
    .await?;

    Ok(streaming_report_and_timings(&txs, peaks))
}
