//! Phase-A deploy cache. After a successful deploy we drop a JSON file so
//! the next test run for the same `(network, src, dst, deployer)` can skip
//! the deploy and go straight to Phase B as long as the destination token
//! still responds to `name()`.

use std::path::{Path, PathBuf};

use alloy::primitives::Address;
use alloy::providers::Provider;

use crate::evm::ERC20;

/// Cache of a successful Phase A run. Keyed on
/// `(network, src, dst, deployer_pubkey)` so a fresh run can skip the deploy
/// and go straight to Phase B if the previously-deployed token still exists.
#[derive(serde::Serialize, serde::Deserialize)]
pub(super) struct ItsTestCache {
    pub(super) deployer: String,
    pub(super) salt_hex: String,
    pub(super) token_id_hex: String,
    pub(super) dest_token_address: String,
}

pub(super) fn cache_path(
    network: crate::types::Network,
    src: &str,
    dst: &str,
    deployer: &str,
) -> PathBuf {
    let data_dir = dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("axe");
    data_dir.join(format!("its-test-{network}-{src}-{dst}-{deployer}.json"))
}

pub(super) async fn read_cache(path: &Path) -> Option<ItsTestCache> {
    let content = tokio::fs::read_to_string(path).await.ok()?;
    serde_json::from_str(&content).ok()
}

pub(super) async fn save_cache(path: &Path, cache: &ItsTestCache) -> eyre::Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(path, serde_json::to_string_pretty(cache)?).await?;
    Ok(())
}

/// If a cached Phase-A deploy for this (src, dst, deployer) tuple still has a
/// valid destination token (responds to `name()`), return it so we can skip
/// Phase A entirely. Returns None on any cache miss / staleness.
pub(super) async fn try_load_cached_phase_a<P: Provider>(
    cache_file: &Path,
    fresh_token: bool,
    sol_pubkey: &solana_sdk::pubkey::Pubkey,
    dst_provider: &P,
) -> Option<(String, [u8; 32], Address)> {
    if fresh_token {
        return None;
    }
    let c = read_cache(cache_file).await?;
    if c.deployer != sol_pubkey.to_string() {
        return None;
    }
    let tid_bytes: [u8; 32] = match alloy::hex::decode(c.token_id_hex.trim_start_matches("0x")) {
        Ok(b) if b.len() == 32 => {
            let mut a = [0u8; 32];
            a.copy_from_slice(&b);
            a
        }
        _ => return None,
    };
    let addr: Address = c.dest_token_address.parse().ok()?;
    let token = ERC20::new(addr, dst_provider);
    match token.name().call().await {
        Ok(name) => Some((name, tid_bytes, addr)),
        Err(_) => None,
    }
}
