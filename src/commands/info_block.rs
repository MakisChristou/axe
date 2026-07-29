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

fn read_axelar_rpc_from(config_path: &Path) -> Result<String> {
    let content = std::fs::read_to_string(config_path)?;
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

pub async fn run(network: Network, number: Option<u64>, at_time: Option<String>) -> Result<()> {
    let config_path = config_source::resolve(network, None).await?.into_path();
    let rpc = read_axelar_rpc_from(&config_path)?;

    ui::section(&format!("Info: block ({network})"));

    let spinner = ui::wait_spinner("querying Tendermint RPC...");
    let (head_height, head_time_raw) = rpc_block_info(&rpc, None).await?;
    let head_time = parse_block_time(&head_time_raw)?;

    // Past block: actual on-chain time, no prediction.
    if let Some(n) = number
        && n <= head_height
    {
        let (h, t_raw) = rpc_block_info(&rpc, Some(n)).await?;
        let t = parse_block_time(&t_raw)?;
        spinner.finish_and_clear();
        ui::kv("Block", &h.to_string());
        print_times(t);
        return Ok(());
    }

    // Either future block or --at-time: need rate from a sample window.
    if number.is_some() || at_time.is_some() {
        let sample_window = RATE_SAMPLE_WINDOW.min(head_height.saturating_sub(1));
        if sample_window == 0 {
            spinner.finish_and_clear();
            return Err(eyre::eyre!(
                "chain has too few blocks ({head_height}) to sample a block rate"
            ));
        }
        let sample_height = head_height - sample_window;
        let (_, sample_time_raw) = rpc_block_info(&rpc, Some(sample_height)).await?;
        let sample_time = parse_block_time(&sample_time_raw)?;
        spinner.finish_and_clear();

        let elapsed_secs = (head_time - sample_time).num_milliseconds() as f64 / 1000.0;
        let rate_secs_per_block = elapsed_secs / sample_window as f64;
        if rate_secs_per_block <= 0.0 {
            return Err(eyre::eyre!(
                "computed non-positive block rate ({rate_secs_per_block:.3}s); RPC may have returned out-of-order timestamps"
            ));
        }

        if let Some(n) = number {
            let delta_blocks = n - head_height;
            let delta_secs = delta_blocks as f64 * rate_secs_per_block;
            let predicted =
                head_time + chrono::Duration::milliseconds((delta_secs * 1000.0) as i64);
            ui::kv("Block", &n.to_string());
            print_times(predicted);
            ui::kv(
                "Note",
                &format!("prediction using {rate_secs_per_block:.2}s / block"),
            );
            return Ok(());
        }

        let target = parse_at_time(
            at_time
                .as_deref()
                .ok_or_else(|| eyre::eyre!("missing --at-time value"))?,
        )?;
        let delta_secs = (target - head_time).num_milliseconds() as f64 / 1000.0;
        let delta_blocks = delta_secs / rate_secs_per_block;
        let predicted_height = head_height as f64 + delta_blocks;
        if predicted_height < 1.0 {
            return Err(eyre::eyre!(
                "target time predates the chain genesis at the current rate"
            ));
        }
        ui::kv("Block", &format!("{}", predicted_height.round() as u64));
        print_times(target);
        ui::kv(
            "Note",
            &format!("prediction using {rate_secs_per_block:.2}s / block"),
        );
        return Ok(());
    }

    // No args: show the head.
    spinner.finish_and_clear();
    ui::kv("Block", &head_height.to_string());
    print_times(head_time);
    Ok(())
}
