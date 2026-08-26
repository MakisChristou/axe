use std::path::Path;

use chrono::{DateTime, Local, Utc};
use eyre::Result;
use serde_json::Value;

use crate::config_source;
use crate::cosmos::rpc_block_info;
use crate::types::Network;
use crate::ui;

/// Number of blocks back from the head we sample to estimate the per-block
/// time when predicting. Axelar produces ~5–6s blocks, so a 1000-block window
/// is ~90 minutes of history — long enough to smooth out one-off slow blocks
/// without going so far back that consensus parameter changes skew the rate.
const RATE_SAMPLE_WINDOW: u64 = 1000;

async fn read_axelar_rpc_from(config_path: &Path) -> Result<String> {
    let content = tokio::fs::read_to_string(config_path).await?;
    let root: Value = serde_json::from_str(&content)?;
    root.pointer("/axelar/rpc")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| eyre::eyre!("no axelar.rpc in config"))
}

/// Parse `--at-time`: RFC3339 first, then unix seconds.
fn parse_at_time(s: &str) -> Result<DateTime<Utc>> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Ok(dt.with_timezone(&Utc));
    }
    if let Ok(secs) = s.parse::<i64>()
        && let Some(dt) = DateTime::from_timestamp(secs, 0)
    {
        return Ok(dt);
    }
    Err(eyre::eyre!(
        "could not parse '{s}' as RFC3339 (e.g. 2026-05-18T14:00:00Z) or unix seconds"
    ))
}

fn parse_block_time(s: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| eyre::eyre!("invalid block timestamp '{s}': {e}"))
}

fn print_times(time: DateTime<Utc>) {
    ui::kv("UTC", &time.format("%Y-%m-%d %H:%M:%S UTC").to_string());
    ui::kv(
        "Local",
        &time
            .with_timezone(&Local)
            .format("%Y-%m-%d %H:%M:%S %:z")
            .to_string(),
    );
}

/// What a block lookup resolved to, independent of how it is rendered.
///
/// `run` prints this for the CLI and the MCP server serializes it, so the
/// query runs once and both front ends agree on the answer.
#[derive(Debug, serde::Serialize)]
pub struct BlockInfo {
    pub network: Network,
    pub height: u64,
    pub time: DateTime<Utc>,
    /// True when the height or the time was extrapolated from the current
    /// block rate rather than read from a block that already exists.
    pub predicted: bool,
    /// Seconds per block used to extrapolate, absent when not predicting.
    pub rate_secs_per_block: Option<f64>,
}

/// Resolve a block lookup without printing anything.
///
/// With no arguments this reports the head. A past height reports that
/// block's real timestamp. A future height, or `at_time`, extrapolates from
/// the rate measured over [`RATE_SAMPLE_WINDOW`] blocks.
pub async fn resolve(
    network: Network,
    number: Option<u64>,
    at_time: Option<String>,
) -> Result<BlockInfo> {
    let config_path = config_source::resolve(network, None).await?.into_path();
    let rpc = read_axelar_rpc_from(&config_path).await?;

    let (head_height, head_time_raw) = rpc_block_info(&rpc, None).await?;
    let head_time = parse_block_time(&head_time_raw)?;

    // Past block: actual on-chain time, no prediction.
    if let Some(n) = number
        && n <= head_height
    {
        let (h, t_raw) = rpc_block_info(&rpc, Some(n)).await?;
        return Ok(BlockInfo {
            network,
            height: h,
            time: parse_block_time(&t_raw)?,
            predicted: false,
            rate_secs_per_block: None,
        });
    }

    // Either future block or an explicit time: both need a measured rate.
    if number.is_some() || at_time.is_some() {
        let rate = measure_block_rate(&rpc, head_height, head_time).await?;

        if let Some(n) = number {
            let delta_secs = (n - head_height) as f64 * rate;
            return Ok(BlockInfo {
                network,
                height: n,
                time: head_time + chrono::Duration::milliseconds((delta_secs * 1000.0) as i64),
                predicted: true,
                rate_secs_per_block: Some(rate),
            });
        }

        let target = parse_at_time(
            at_time
                .as_deref()
                .ok_or_else(|| eyre::eyre!("missing --at-time value"))?,
        )?;
        let delta_blocks = (target - head_time).num_milliseconds() as f64 / 1000.0 / rate;
        let predicted_height = head_height as f64 + delta_blocks;
        if predicted_height < 1.0 {
            return Err(eyre::eyre!(
                "target time predates the chain genesis at the current rate"
            ));
        }
        return Ok(BlockInfo {
            network,
            height: predicted_height.round() as u64,
            time: target,
            predicted: true,
            rate_secs_per_block: Some(rate),
        });
    }

    Ok(BlockInfo {
        network,
        height: head_height,
        time: head_time,
        predicted: false,
        rate_secs_per_block: None,
    })
}

/// Seconds per block, averaged over the sample window behind the head.
async fn measure_block_rate(rpc: &str, head_height: u64, head_time: DateTime<Utc>) -> Result<f64> {
    let sample_window = RATE_SAMPLE_WINDOW.min(head_height.saturating_sub(1));
    if sample_window == 0 {
        return Err(eyre::eyre!(
            "chain has too few blocks ({head_height}) to sample a block rate"
        ));
    }

    let (_, sample_time_raw) = rpc_block_info(rpc, Some(head_height - sample_window)).await?;
    let sample_time = parse_block_time(&sample_time_raw)?;

    let elapsed_secs = (head_time - sample_time).num_milliseconds() as f64 / 1000.0;
    let rate = elapsed_secs / sample_window as f64;
    if rate <= 0.0 {
        return Err(eyre::eyre!(
            "computed non-positive block rate ({rate:.3}s); RPC may have returned out-of-order timestamps"
        ));
    }
    Ok(rate)
}

pub async fn run(network: Network, number: Option<u64>, at_time: Option<String>) -> Result<()> {
    ui::section(&format!("Info: block ({network})"));

    let spinner = ui::wait_spinner("querying Tendermint RPC...");
    let info = resolve(network, number, at_time).await;
    spinner.finish_and_clear();
    let info = info?;

    ui::kv("Block", &info.height.to_string());
    print_times(info.time);
    if let Some(rate) = info.rate_secs_per_block {
        ui::kv("Note", &format!("prediction using {rate:.2}s / block"));
    }
    Ok(())
}
