//! Resolution of the chains-config JSON (`{network}.json` from the
//! `axelar-contract-deployments` repo) for commands that read it.
//!
//! Order: explicit `--config`/`CHAINS_CONFIG` path, then the sibling
//! `../axelar-contract-deployments` checkout, then a cached copy fetched
//! from GitHub. The sibling relative-path literal lives only in this
//! module. Write-back paths (deploy/init) require a real checkout — see
//! [`ConfigSource::require_checkout`].

use std::path::{Path, PathBuf};
use std::time::Duration;

use eyre::{Result, WrapErr, eyre};

use crate::config::ChainsConfig;
use crate::state::data_dir;
use crate::types::Network;
use crate::ui;

const SIBLING_INFO_DIR: &str = "../axelar-contract-deployments/axelar-chains-config/info";
const BASE_URL: &str = "https://raw.githubusercontent.com/axelarnetwork/axelar-contract-deployments/main/axelar-chains-config/info";
/// How long a cached config is served before re-fetching from GitHub.
const CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Where a resolved chains-config file lives. Write-back paths (deploy/init)
/// require `Checkout`.
#[derive(Debug)]
pub enum ConfigSource {
    /// Explicit --config / CHAINS_CONFIG path, or the sibling
    /// ../axelar-contract-deployments checkout. Writable.
    Checkout(PathBuf),
    /// Read-only copy fetched from GitHub into the cache dir.
    Cached(PathBuf),
}

impl ConfigSource {
    pub fn path(&self) -> &Path {
        match self {
            Self::Checkout(p) | Self::Cached(p) => p,
        }
    }

    pub fn into_path(self) -> PathBuf {
        match self {
            Self::Checkout(p) | Self::Cached(p) => p,
        }
    }

    pub fn require_checkout(self) -> Result<PathBuf> {
        match self {
            Self::Checkout(p) => Ok(p),
            Self::Cached(_) => Err(eyre!(
                "deploy requires a local axelar-contract-deployments checkout; \
                 pass --config <path-to-info/<network>.json> or set TARGET_JSON"
            )),
        }
    }
}

/// Resolve the chains-config file for `network`. An explicit path (a
/// `--config` arg, or the `CHAINS_CONFIG` env var when no arg is given)
/// wins; otherwise the sibling checkout, then a cached GitHub fetch.
pub async fn resolve(network: Network, explicit: Option<PathBuf>) -> Result<ConfigSource> {
    let explicit = explicit.or_else(|| std::env::var_os("CHAINS_CONFIG").map(Into::into));
    let cache_dir = data_dir()?.join("chains-config");
    resolve_in(
        network,
        explicit,
        Path::new(SIBLING_INFO_DIR),
        &cache_dir,
        BASE_URL,
    )
    .await
}

async fn resolve_in(
    network: Network,
    explicit: Option<PathBuf>,
    sibling_info_dir: &Path,
    cache_dir: &Path,
    base_url: &str,
) -> Result<ConfigSource> {
    if let Some(path) = explicit {
        // A path whose filename names a network must name *this* network —
        // otherwise a stale CHAINS_CONFIG silently runs e.g. "mainnet" logic
        // against testnet.json. Custom filenames are exempt.
        if let Some(named) = crate::commands::load_test::detect_network_from_config(&path)
            && named != network
        {
            return Err(eyre!(
                "chains config '{}' (--config/CHAINS_CONFIG) targets {named}, but the \
                 requested network is {network}; drop one or make them match",
                path.display()
            ));
        }
        if tokio::fs::try_exists(&path).await? {
            return Ok(ConfigSource::Checkout(path));
        }
        return Err(eyre!("chains config '{}' does not exist", path.display()));
    }

    let sibling = sibling_info_dir.join(format!("{network}.json"));
    if tokio::fs::try_exists(&sibling).await? {
        return Ok(ConfigSource::Checkout(sibling));
    }

    let cached = cache_dir.join(format!("{network}.json"));
    if is_fresh(&cached).await {
        return Ok(ConfigSource::Cached(cached));
    }

    let url = format!("{base_url}/{network}.json");
    match fetch_validated(&url).await {
        Ok(body) => {
            write_atomic(&cached, &body).await?;
            Ok(ConfigSource::Cached(cached))
        }
        Err(e) if tokio::fs::try_exists(&cached).await? => {
            ui::warn(&format!(
                "fetch of {url} failed ({e}); using cached config from {}; \
                 delete it or pass --config to refresh",
                cached.display()
            ));
            Ok(ConfigSource::Cached(cached))
        }
        Err(e) => Err(eyre!(
            "no chains config for {network}: no sibling checkout at {}, no cache at {}, \
             and fetch of {url} failed: {e}. \
             Pass --config <path> or connect to the network.",
            sibling.display(),
            cached.display(),
        )),
    }
}

/// True when `path` exists and was written within [`CACHE_TTL`].
async fn is_fresh(path: &Path) -> bool {
    tokio::fs::metadata(path)
        .await
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|mtime| mtime.elapsed().ok())
        .is_some_and(|age| age < CACHE_TTL)
}

/// Fetch `url` and confirm the body parses as a [`ChainsConfig`] so a 404
/// page or truncated download can never poison the cache.
async fn fetch_validated(url: &str) -> Result<String> {
    let body = reqwest::get(url).await?.error_for_status()?.text().await?;
    ChainsConfig::from_json_str(&body)
        .wrap_err_with(|| format!("fetched {url} but it does not parse as a chains config"))?;
    Ok(body)
}

/// Write via a pid-suffixed temp file + rename so a concurrent axe process
/// never observes a partially written cache entry.
async fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| eyre!("cache path {} has no parent directory", path.display()))?;
    tokio::fs::create_dir_all(parent).await?;
    let tmp = parent.join(format!(
        "{}.tmp.{}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    tokio::fs::write(&tmp, contents).await?;
    tokio::fs::rename(&tmp, path).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::future::Future;
    use std::time::SystemTime;

    /// A guaranteed-dead endpoint: connecting to port 1 on localhost is
    /// refused immediately, so these tests stay offline and fast.
    const DEAD_URL: &str = "http://127.0.0.1:1";

    /// Minimal valid `ChainsConfig` JSON for fixture files.
    const MINIMAL: &str = r#"{ "chains": {}, "axelar": {} }"#;

    /// Self-cleaning unique dir under the OS temp dir (no tempfile dep).
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("axe-config-source-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("create temp dir");
            Self { path }
        }

        fn file(&self, rel: &str, contents: &str) -> PathBuf {
            let p = self.path.join(rel);
            fs::create_dir_all(p.parent().expect("file has parent")).expect("create parent");
            fs::write(&p, contents).expect("write fixture");
            p
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn backdate(path: &Path, secs: u64) {
        let mtime = SystemTime::now() - Duration::from_secs(secs);
        fs::File::options()
            .write(true)
            .open(path)
            .expect("open cache file")
            .set_modified(mtime)
            .expect("set mtime");
    }

    fn block_on<F: Future>(future: F) -> F::Output {
        tokio::runtime::Runtime::new().unwrap().block_on(future)
    }

    fn resolve_dead(dir: &TempDir, explicit: Option<PathBuf>) -> Result<ConfigSource> {
        block_on(resolve_in(
            Network::Testnet,
            explicit,
            &dir.path.join("sibling"),
            &dir.path.join("cache"),
            DEAD_URL,
        ))
    }

    #[test]
    fn explicit_path_naming_other_network_bails() {
        let dir = TempDir::new("axe-cfg-mismatch");
        let other = dir.path.join("mainnet.json");
        std::fs::write(&other, "{}").unwrap();
        let err = resolve_dead(&dir, Some(other)).unwrap_err();
        assert!(err.to_string().contains("targets mainnet"), "{err}");
    }

    #[test]
    fn explicit_wins_over_sibling() {
        let dir = TempDir::new("explicit-wins");
        let explicit = dir.file("explicit.json", MINIMAL);
        dir.file("sibling/testnet.json", MINIMAL);

        let src = resolve_dead(&dir, Some(explicit.clone())).unwrap();
        match src {
            ConfigSource::Checkout(p) => assert_eq!(p, explicit),
            other => panic!("expected Checkout, got {other:?}"),
        }
    }

    #[test]
    fn explicit_but_missing_errors() {
        let dir = TempDir::new("explicit-missing");
        dir.file("sibling/testnet.json", MINIMAL);

        let err = resolve_dead(&dir, Some(dir.path.join("nope.json"))).unwrap_err();
        assert!(err.to_string().contains("does not exist"), "{err}");
    }

    #[test]
    fn sibling_beats_fresh_cache_without_http() {
        let dir = TempDir::new("sibling-wins");
        let sibling = dir.file("sibling/testnet.json", MINIMAL);
        dir.file("cache/testnet.json", MINIMAL);

        // DEAD_URL proves no HTTP is attempted on this path.
        let src = resolve_dead(&dir, None).unwrap();
        match src {
            ConfigSource::Checkout(p) => assert_eq!(p, sibling),
            other => panic!("expected Checkout, got {other:?}"),
        }
    }

    #[test]
    fn fresh_cache_hits_without_http() {
        let dir = TempDir::new("fresh-cache");
        let cached = dir.file("cache/testnet.json", MINIMAL);

        let src = resolve_dead(&dir, None).unwrap();
        match src {
            ConfigSource::Cached(p) => assert_eq!(p, cached),
            other => panic!("expected Cached, got {other:?}"),
        }
    }

    #[test]
    fn stale_cache_survives_dead_fetch() {
        let dir = TempDir::new("stale-cache");
        let cached = dir.file("cache/testnet.json", MINIMAL);
        backdate(&cached, 25 * 60 * 60);

        let src = resolve_dead(&dir, None).unwrap();
        match src {
            ConfigSource::Cached(p) => assert_eq!(p, cached),
            other => panic!("expected Cached, got {other:?}"),
        }
    }

    #[test]
    fn no_cache_dead_fetch_names_escape_hatches() {
        let dir = TempDir::new("all-fail");

        let err = resolve_dead(&dir, None).unwrap_err();
        let msg = err.to_string();
        let sibling = dir.path.join("sibling/testnet.json");
        let cached = dir.path.join("cache/testnet.json");
        assert!(msg.contains(&sibling.display().to_string()), "{msg}");
        assert!(msg.contains(&cached.display().to_string()), "{msg}");
        assert!(msg.contains("--config"), "{msg}");
    }

    #[test]
    fn write_atomic_overwrites_and_leaves_no_tmp_file() {
        let dir = TempDir::new("atomic");
        let target = dir.path.join("cache/testnet.json");

        block_on(write_atomic(&target, "first")).unwrap();
        block_on(write_atomic(&target, "second")).unwrap();

        assert_eq!(fs::read_to_string(&target).unwrap(), "second");
        let names: Vec<_> = fs::read_dir(target.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["testnet.json"], "no tmp leftovers");
    }

    #[test]
    fn require_checkout_rejects_cached_passes_checkout() {
        let p = PathBuf::from("some/testnet.json");
        assert!(ConfigSource::Cached(p.clone()).require_checkout().is_err());
        assert_eq!(
            ConfigSource::Checkout(p.clone())
                .require_checkout()
                .unwrap(),
            p
        );
    }

    /// Live fetch of the real testnet config from GitHub. Replaces the old
    /// sibling-checkout-dependent `loads_real_testnet_json` in `config.rs`.
    /// Run manually after schema bumps with `cargo test -- --ignored`.
    #[test]
    #[ignore = "network"]
    fn fetches_and_parses_real_testnet_json() {
        let dir = TempDir::new("live");
        let src = block_on(resolve_in(
            Network::Testnet,
            None,
            &dir.path.join("sibling"),
            &dir.path.join("cache"),
            BASE_URL,
        ))
        .expect("live fetch resolves");

        let cfg = block_on(ChainsConfig::load(src.path())).expect("testnet.json loads + parses");
        assert!(cfg.chains.contains_key("hedera"), "hedera chain present");
        assert!(cfg.chains.contains_key("solana"), "solana chain present");
        assert!(cfg.axelar.lcd.is_some(), "axelar.lcd present");
        assert!(
            cfg.axelar.parse_gas_price().is_ok(),
            "axelar.gasPrice parses",
        );
    }
}
