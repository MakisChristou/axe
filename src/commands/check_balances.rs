//! `axe check-balances <network>` — pre-flight wallet balance check.
//!
//! Mirrors the (now-removed) `axelar-contract-deployments/scripts/
//! check-wallet-balances.js` from PR #1383: derives each chain's wallet
//! address from the corresponding env key, queries the native-token balance,
//! and exits non-zero if any wallet is below its per-chain threshold.
//!
//! Designed as a fail-fast pre-step for the cron amplifier-routes workflows:
//! if a wallet is underfunded, every load-test route that uses it will fail
//! mid-run with a confusing on-chain error; checking up front saves CI time
//! and produces a clear "fund this address" message in the first job.

use alloy::primitives::Address;
use alloy::providers::{Provider, ProviderBuilder};
use alloy::signers::local::PrivateKeySigner;
use comfy_table::{Attribute, Cell, Color, ContentArrangement, Table};
use eyre::{Result, WrapErr, eyre};
use solana_sdk::signature::Signer;

use crate::config::{ChainConfig, ChainsConfig};
use crate::config_source;
use crate::solana::{load_keypair, rpc_client};
use crate::stellar::{StellarClient, StellarWallet};
use crate::sui::{SuiClient, SuiWallet};
use crate::types::Network;
use crate::ui;
use crate::xrpl::{XrplClient, XrplWallet};

/// One chain's preflight target. Owned strings only — see
/// CLAUDE.md "Owned types in bundle structs".
#[derive(Clone, Debug)]
struct ChainTarget {
    chain_key: String,
    kind: ChainKind,
    threshold_units: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChainKind {
    Evm,
    Solana,
    Sui,
    Stellar,
    Xrpl,
}

impl ChainKind {
    const fn label(self) -> &'static str {
        match self {
            Self::Evm => "EVM",
            Self::Solana => "Solana",
            Self::Sui => "Sui",
            Self::Stellar => "Stellar",
            Self::Xrpl => "XRPL",
        }
    }

    const fn default_symbol(self) -> &'static str {
        match self {
            Self::Evm => "ETH",
            Self::Solana => "SOL",
            Self::Sui => "SUI",
            Self::Stellar => "XLM",
            Self::Xrpl => "XRP",
        }
    }
}

/// Which balance to check on a chain: the native gas token, or the AXE ITS
/// token the daily cron transfers. AXE underfunding is handled more leniently
/// than gas — see [`BalanceRow::is_fatal`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Asset {
    Native,
    Axe,
}

/// Minimum AXE a cron ITS *source* wallet must hold to reuse the pre-registered
/// interchain token instead of silently deploying a throwaway one each run.
/// Mirrors `WHOLE_TOKENS_PER_KEY` (`WHOLE_TOKENS_PER_TX` × 100) in the load-test
/// ITS modules — below it, `reusable_config_axe` (and the Stellar/Sol
/// equivalents) fall through to a fresh deploy, which on Stellar then hits the
/// `TxBadSeq` sequence race. Whole AXE tokens.
const AXE_MIN_WHOLE_TOKENS: f64 = 100.0;

#[derive(Clone, Debug)]
struct BalanceRow {
    chain_key: String,
    kind: ChainKind,
    asset: Asset,
    address: Option<String>,
    balance: Option<f64>,
    threshold: f64,
    token_symbol: String,
    note: String,
}

impl BalanceRow {
    fn is_underfunded(&self) -> bool {
        self.balance.is_none_or(|b| b < self.threshold)
    }

    /// Whether this row fails the preflight (blocks the whole cron run).
    /// Native-gas underfunding — including an unreachable RPC — is always
    /// fatal. An AXE row is fatal only on a *confirmed* low balance; an AXE
    /// read error is downgraded to a warning ([`Self::is_axe_read_error`]) so a
    /// flaky token read can't wedge the cron when the gas balances are fine.
    fn is_fatal(&self) -> bool {
        match self.asset {
            Asset::Native => self.is_underfunded(),
            Asset::Axe => self.balance.is_some_and(|b| b < self.threshold),
        }
    }

    /// AXE balance couldn't be read (RPC/decode error) — advisory, not fatal.
    fn is_axe_read_error(&self) -> bool {
        self.asset == Asset::Axe && self.balance.is_none()
    }
}

/// Per-network chain → threshold (in native token units).
///
/// Values mirror the removed JS script's `THRESHOLDS` map verbatim, except
/// Solana (not present in the JS) is set to 0.3 SOL — large enough to cover
/// many GMP-fleet transactions including PDA-creating rent-exempt deposits.
fn chain_targets(network: Network) -> Vec<ChainTarget> {
    match network {
        Network::Mainnet | Network::Testnet => production_chain_targets(network),
        Network::Stagenet => stagenet_chain_targets(),
        Network::DevnetAmplifier => devnet_chain_targets(),
    }
}

/// Stagenet wallet targets. Chain ids come from stagenet.json: Monad is plain
/// `monad`, Solana is `solana-stagenet-3`, and there is NO Stellar deployment.
fn stagenet_chain_targets() -> Vec<ChainTarget> {
    let evm = |chain_key: &str, threshold_units: f64| ChainTarget {
        chain_key: chain_key.to_string(),
        kind: ChainKind::Evm,
        threshold_units,
    };
    vec![
        evm("hyperliquid", 0.01),
        evm("xrpl-evm", 2.0),
        evm("hedera", 5.0),
        evm("monad", 50.0),
        evm("avalanche", 0.5),
        evm("flow", 1.0),
        ChainTarget {
            chain_key: "xrpl".to_string(),
            kind: ChainKind::Xrpl,
            threshold_units: 3.0,
        },
        ChainTarget {
            chain_key: "sui".to_string(),
            kind: ChainKind::Sui,
            threshold_units: 0.05,
        },
        ChainTarget {
            chain_key: "solana-stagenet-3".to_string(),
            kind: ChainKind::Solana,
            threshold_units: 0.3,
        },
    ]
}

/// Devnet-amplifier wallet targets. Chain ids come from devnet-amplifier.json.
/// Absent by design: Stellar (no deployment), XRPL (`xrpl-dev` only exposes a
/// wss:// RPC the HTTP client can't probe), Hyperliquid/Hedera/Monad (not
/// deployed on this network).
fn devnet_chain_targets() -> Vec<ChainTarget> {
    let evm = |chain_key: &str, threshold_units: f64| ChainTarget {
        chain_key: chain_key.to_string(),
        kind: ChainKind::Evm,
        threshold_units,
    };
    vec![
        evm("avalanche-fuji", 0.5),
        evm("eth-sepolia", 0.5),
        evm("xrpl-evm-devnet", 2.0),
        evm("flow", 1.0),
        ChainTarget {
            chain_key: "sui-2".to_string(),
            kind: ChainKind::Sui,
            threshold_units: 0.05,
        },
        ChainTarget {
            chain_key: "solana-18".to_string(),
            kind: ChainKind::Solana,
            threshold_units: 0.3,
        },
    ]
}

fn production_chain_targets(network: Network) -> Vec<ChainTarget> {
    let stellar_key = stellar_chain_key(network);
    // The Monad amplifier chain id differs by network: `monad` on mainnet,
    // `monad-3` on testnet (the active testnet deployment).
    let monad_key = monad_chain_key(network);
    vec![
        ChainTarget {
            chain_key: "hyperliquid".to_string(),
            kind: ChainKind::Evm,
            threshold_units: 0.01,
        },
        ChainTarget {
            chain_key: "xrpl-evm".to_string(),
            kind: ChainKind::Evm,
            threshold_units: 2.0,
        },
        ChainTarget {
            // Hedera native gas token is HBAR. A Hedera ITS interchainTransfer
            // costs ~1–2 HBAR (no HTS-CREATE — WHBAR fee is reserved for the
            // initial token deployment); 5 HBAR keeps a few-route buffer.
            chain_key: "hedera".to_string(),
            kind: ChainKind::Evm,
            threshold_units: 5.0,
        },
        ChainTarget {
            // Monad gas token is MON. Source-side EVM gas is sub-cent, but
            // Monad → Hedera cross-chain gas is *~5 MON per tx* (Hedera HTS
            // precompile pricing → Axelar API estimate). axe pays 2× that for
            // the source→hub→dest legs, so each Monad-source ITS run burns
            // ~10 MON. 50 MON keeps a ~5-run buffer above the cron's pace.
            chain_key: monad_key.to_string(),
            kind: ChainKind::Evm,
            threshold_units: 50.0,
        },
        ChainTarget {
            chain_key: "xrpl".to_string(),
            kind: ChainKind::Xrpl,
            threshold_units: 3.0,
        },
        ChainTarget {
            chain_key: stellar_key.to_string(),
            kind: ChainKind::Stellar,
            // 2.5 XLM headroom: Stellar reserves ~1 XLM for the account itself
            // and our test routes spend up to ~0.5 XLM per run (top-ups +
            // surge fees), so 2.5 XLM keeps a few routes of buffer above the
            // unspendable reserve. Bumped from 0.5 XLM after a CI run hit
            // PAYMENT_UNDERFUNDED on a wallet that was above the old floor
            // but below `balance − reserve`.
            threshold_units: 2.5,
        },
        ChainTarget {
            chain_key: "sui".to_string(),
            kind: ChainKind::Sui,
            threshold_units: 0.05,
        },
        ChainTarget {
            chain_key: "solana".to_string(),
            kind: ChainKind::Solana,
            threshold_units: 0.3,
        },
        ChainTarget {
            // Avalanche is the legacy (consensus-EVM) ITS *source* in the
            // cron fleet. Source-side gas is sub-cent, but each run tops up a
            // derived key (~0.05 AVAX) and pays cross-chain gas (~0.005 AVAX);
            // 0.5 AVAX keeps a several-run buffer. Same key on both networks.
            chain_key: "avalanche".to_string(),
            kind: ChainKind::Evm,
            threshold_units: 0.5,
        },
    ]
}

/// The Monad amplifier chain id differs by network: `monad` on mainnet and
/// stagenet, `monad-3` on testnet. (Devnet-amplifier has no Monad.)
fn monad_chain_key(network: Network) -> &'static str {
    match network {
        Network::Mainnet | Network::Stagenet | Network::DevnetAmplifier => "monad",
        Network::Testnet => "monad-3",
    }
}

/// Stellar chain id per network. Only mainnet and testnet have a Stellar
/// deployment; the other arms exist for totality and are never reached by the
/// per-network target builders.
fn stellar_chain_key(network: Network) -> &'static str {
    match network {
        Network::Mainnet => "stellar",
        Network::Testnet | Network::Stagenet | Network::DevnetAmplifier => "stellar-2026-q1-2",
    }
}

/// Chains the daily ITS cron spends AXE *from* as a source — each must hold
/// `>= AXE_MIN_WHOLE_TOKENS` or the run deploys a throwaway token instead of
/// reusing the canonical one. Scoped to the AXE sources whose wallet is
/// derivable from a local secret (EVM + Stellar); Sui and Solana also transfer
/// AXE and hold ~1M, but their preflight keys aren't in the local env so their
/// readers aren't exercised here. XRPL / XRPL-EVM cron routes move XRP (not
/// AXE), and destination-only chains (e.g. Ethereum) never spend source AXE, so
/// both are excluded. A source whose `contracts.AXE` isn't recorded yet reads
/// as an advisory "?" (non-fatal) until the config + seed land.
fn axe_targets(network: Network) -> Vec<ChainTarget> {
    let axe = |chain_key: &str, kind: ChainKind| ChainTarget {
        chain_key: chain_key.to_string(),
        kind,
        threshold_units: AXE_MIN_WHOLE_TOKENS,
    };
    match network {
        Network::Mainnet | Network::Testnet => {
            let mut targets = vec![
                axe("hyperliquid", ChainKind::Evm),
                axe(monad_chain_key(network), ChainKind::Evm),
                axe("hedera", ChainKind::Evm),
                // Avalanche is the legacy (consensus-EVM) ITS source in the
                // fleet (Avalanche→Monad, Avalanche→Ethereum) — it holds the
                // canonical AXE seeded from xrpl-evm. Same key on both.
                axe("avalanche", ChainKind::Evm),
            ];
            // Stellar is a mainnet ITS AXE source (Stellar ↔ Hyperliquid); it
            // was removed from the testnet ITS cron, so only check on mainnet.
            if network == Network::Mainnet {
                targets.push(axe(stellar_chain_key(network), ChainKind::Stellar));
            }
            targets
        }
        // Plausible ITS AXE sources on the non-production networks. All rows
        // are advisory until `contracts.AXE` entries land in their configs.
        Network::Stagenet => vec![
            axe("monad", ChainKind::Evm),
            axe("hedera", ChainKind::Evm),
            axe("avalanche", ChainKind::Evm),
            axe("flow", ChainKind::Evm),
        ],
        Network::DevnetAmplifier => vec![
            axe("avalanche-fuji", ChainKind::Evm),
            axe("eth-sepolia", ChainKind::Evm),
            axe("flow", ChainKind::Evm),
        ],
    }
}

pub async fn run(network: Network) -> Result<()> {
    let config_path = config_source::resolve(network, None).await?.into_path();
    let config = ChainsConfig::load(&config_path).await?;

    ui::section(&format!("wallet balance check: {network}"));
    ui::kv("config", &config_path.display().to_string());

    let native = chain_targets(network);
    let axe = axe_targets(network);
    let mut rows = Vec::with_capacity(native.len() + axe.len());
    for target in &native {
        rows.push(probe_row(&config, target, network, Asset::Native).await);
    }
    for target in &axe {
        rows.push(probe_row(&config, target, network, Asset::Axe).await);
    }

    render_table(&rows);

    // AXE read errors are advisory — warn, but don't block the cron on them
    // (the native-gas gate still applies; a genuine low AXE balance is fatal).
    for row in rows.iter().filter(|r| r.is_axe_read_error()) {
        ui::warn(&format!(
            "AXE balance unread for {} ({}) — {} (not blocking)",
            row.chain_key,
            row.kind.label(),
            if row.note.is_empty() {
                "unknown error"
            } else {
                &row.note
            },
        ));
    }

    let fatal: Vec<&BalanceRow> = rows.iter().filter(|r| r.is_fatal()).collect();
    if !fatal.is_empty() {
        ui::error(&format!(
            "{} wallet(s) below minimum threshold or unreachable:",
            fatal.len()
        ));
        for row in &fatal {
            ui::error(&format!("  {}", format_underfunded(row)));
        }
        return Err(eyre!(
            "wallet preflight failed — fund the addresses above (or fix the listed errors) and retry"
        ));
    }

    ui::success("all wallets above minimum thresholds (native gas + AXE)");
    Ok(())
}

async fn probe_row(
    config: &ChainsConfig,
    target: &ChainTarget,
    network: Network,
    asset: Asset,
) -> BalanceRow {
    let token_symbol = match asset {
        Asset::Axe => "AXE".to_string(),
        Asset::Native => config
            .chains
            .get(&target.chain_key)
            .and_then(|c| c.token_symbol.clone())
            .unwrap_or_else(|| target.kind.default_symbol().to_string()),
    };

    let probe = match asset {
        Asset::Native => probe_balance(config, target, network).await,
        Asset::Axe => probe_axe(config, target, network).await,
    };

    match probe {
        Ok((address, balance)) => BalanceRow {
            chain_key: target.chain_key.clone(),
            kind: target.kind,
            asset,
            address: Some(address),
            balance: Some(balance),
            threshold: target.threshold_units,
            token_symbol,
            note: String::new(),
        },
        Err(err) => BalanceRow {
            chain_key: target.chain_key.clone(),
            kind: target.kind,
            asset,
            address: None,
            balance: None,
            threshold: target.threshold_units,
            token_symbol,
            note: shorten_error(&err.to_string()),
        },
    }
}

/// Read the cron wallet's AXE-token balance (whole tokens) on `target`'s chain.
/// Resolves the token address + decimals from the chain's `contracts.AXE`
/// config entry, then queries per chain kind. Read-only on every path.
async fn probe_axe(
    config: &ChainsConfig,
    target: &ChainTarget,
    network: Network,
) -> Result<(String, f64)> {
    let chain = config
        .chains
        .get(&target.chain_key)
        .ok_or_else(|| eyre!("chain '{}' not in {network} config", target.chain_key))?;
    let axe = chain
        .contracts
        .as_ref()
        .and_then(|m| m.get("AXE"))
        .ok_or_else(|| eyre!("no contracts.AXE entry for {}", target.chain_key))?;
    let token = axe
        .address
        .as_deref()
        .ok_or_else(|| eyre!("contracts.AXE.address missing for {}", target.chain_key))?;
    let decimals = axe
        .extra
        .get("decimals")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(18) as u32;
    match target.kind {
        ChainKind::Evm => probe_axe_evm(chain, token, decimals).await,
        ChainKind::Stellar => probe_axe_stellar(chain, token, decimals, network).await,
        other => Err(eyre!(
            "AXE balance check not implemented for {}",
            other.label()
        )),
    }
}

async fn probe_axe_evm(
    chain: &ChainConfig,
    token_addr: &str,
    decimals: u32,
) -> Result<(String, f64)> {
    let rpc_url = chain
        .rpc
        .as_deref()
        .ok_or_else(|| eyre!("no rpc in config"))?;
    let key = std::env::var("EVM_PRIVATE_KEY").wrap_err("EVM_PRIVATE_KEY env not set")?;
    let signer: PrivateKeySigner = key
        .parse()
        .wrap_err("EVM_PRIVATE_KEY is not a valid hex private key")?;
    let addr: Address = signer.address();
    let token: Address = token_addr
        .parse()
        .wrap_err("contracts.AXE.address is not a valid EVM address")?;
    let provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);
    let erc20 = crate::evm::ERC20::new(token, &provider);
    let raw = crate::retry::retry_all("probe_axe_evm.balanceOf", || async {
        erc20.balanceOf(addr).call().await
    })
    .await?;
    Ok((format!("{addr:#x}"), token_units(raw, decimals)))
}

async fn probe_axe_stellar(
    chain: &ChainConfig,
    token_addr: &str,
    decimals: u32,
    network: Network,
) -> Result<(String, f64)> {
    let rpc_url = chain
        .rpc
        .as_deref()
        .ok_or_else(|| eyre!("no rpc in config"))?;
    let network_type = stellar_network_type(chain, network);
    let client = StellarClient::new(rpc_url, &network_type)?;
    let key = std::env::var("STELLAR_PRIVATE_KEY").wrap_err("STELLAR_PRIVATE_KEY env not set")?;
    let wallet = StellarWallet::from_secret_str(&key)?;
    let pk = wallet.public_key_bytes;
    let raw = crate::retry::retry_all("probe_axe_stellar.balance", || async {
        client.token_balance_view(&pk, token_addr, &pk).await
    })
    .await?;
    Ok((wallet.address(), raw as f64 / 10f64.powi(decimals as i32)))
}

/// Scale a raw `U256` token amount to whole tokens as `f64`, given `decimals`.
/// Generalizes [`wei_to_eth`] to arbitrary-precision tokens (AXE is 18 on most
/// EVM chains, 6 on the Hedera HTS fork).
fn token_units(raw: alloy::primitives::U256, decimals: u32) -> f64 {
    let divisor = alloy::primitives::U256::from(10u64).pow(alloy::primitives::U256::from(decimals));
    let whole = (raw / divisor).to::<u128>() as f64;
    let remainder = (raw % divisor).to::<u128>() as f64 / 10f64.powi(decimals as i32);
    whole + remainder
}

async fn probe_balance(
    config: &ChainsConfig,
    target: &ChainTarget,
    network: Network,
) -> Result<(String, f64)> {
    let chain = config
        .chains
        .get(&target.chain_key)
        .ok_or_else(|| eyre!("chain '{}' not in {network} config", target.chain_key))?;
    match target.kind {
        ChainKind::Evm => probe_evm(chain).await,
        ChainKind::Solana => probe_solana(chain).await,
        ChainKind::Sui => probe_sui(chain).await,
        ChainKind::Stellar => probe_stellar(chain, network).await,
        ChainKind::Xrpl => probe_xrpl(chain).await,
    }
}

async fn probe_evm(chain: &ChainConfig) -> Result<(String, f64)> {
    let rpc_url = chain
        .rpc
        .as_deref()
        .ok_or_else(|| eyre!("no rpc in config"))?;
    let key = std::env::var("EVM_PRIVATE_KEY").wrap_err("EVM_PRIVATE_KEY env not set")?;
    let signer: PrivateKeySigner = key
        .parse()
        .wrap_err("EVM_PRIVATE_KEY is not a valid hex private key")?;
    let addr: Address = signer.address();
    let provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);
    // Wrap with retry: any transient RPC error means the preflight fails
    // and the whole cron run is skipped, even though wallets are funded.
    // Geometric backoff over 3 attempts (~3.5s worst case).
    let wei = crate::retry::retry_all("probe_evm.get_balance", || async {
        provider.get_balance(addr).await
    })
    .await?;
    Ok((format!("{addr:#x}"), wei_to_eth(wei)))
}

async fn probe_solana(chain: &ChainConfig) -> Result<(String, f64)> {
    let rpc_url = chain
        .rpc
        .as_deref()
        .ok_or_else(|| eyre!("no rpc in config"))?;
    // SOLANA_PRIVATE_KEY (when set) is a file path written by the workflow's
    // composite action. Falls back to ~/.config/solana/id.json otherwise.
    let key_path = std::env::var("SOLANA_PRIVATE_KEY").ok();
    let keypair = load_keypair(key_path.as_deref()).await?;
    let pubkey = keypair.pubkey();
    let client = rpc_client(rpc_url);
    let lamports = crate::retry::retry_all("probe_solana.get_balance", || async {
        client.get_balance(&pubkey)
    })
    .await
    .map_err(|e| eyre!("get_balance({pubkey}): {e}"))?;
    Ok((pubkey.to_string(), lamports as f64 / 1_000_000_000.0))
}

async fn probe_sui(chain: &ChainConfig) -> Result<(String, f64)> {
    let rpc_url = chain
        .rpc
        .as_deref()
        .ok_or_else(|| eyre!("no rpc in config"))?;
    let key = std::env::var("SUI_PRIVATE_KEY").wrap_err("SUI_PRIVATE_KEY env not set")?;
    let wallet = SuiWallet::from_secret_str(&key)?;
    let client = SuiClient::new(rpc_url);
    // SuiClient already iterates endpoint fallbacks; the time-axis retry
    // layered on top handles the "429 from every endpoint" case we hit
    // when the cron runs preflight concurrently across mainnet+testnet.
    let mist = crate::retry::retry_all("probe_sui.get_balance", || async {
        client.get_balance(&wallet.address).await
    })
    .await?;
    Ok((wallet.address_hex(), mist as f64 / 1_000_000_000.0))
}

async fn probe_stellar(chain: &ChainConfig, network: Network) -> Result<(String, f64)> {
    let rpc_url = chain
        .rpc
        .as_deref()
        .ok_or_else(|| eyre!("no rpc in config"))?;
    let network_type = stellar_network_type(chain, network);
    let client = StellarClient::new(rpc_url, &network_type)?;
    let key = std::env::var("STELLAR_PRIVATE_KEY").wrap_err("STELLAR_PRIVATE_KEY env not set")?;
    let wallet = StellarWallet::from_secret_str(&key)?;
    let address = wallet.address();
    let stroops = crate::retry::retry_all("probe_stellar.native_balance_stroops", || async {
        client.native_balance_stroops(&address).await
    })
    .await?
    .ok_or_else(|| eyre!("account {address} not found on Stellar"))?;
    // 1 XLM = 10^7 stroops
    Ok((address, stroops as f64 / 10_000_000.0))
}

async fn probe_xrpl(chain: &ChainConfig) -> Result<(String, f64)> {
    let rpc_url = chain
        .rpc
        .as_deref()
        .ok_or_else(|| eyre!("no rpc in config"))?;
    let key = std::env::var("XRPL_PRIVATE_KEY").wrap_err("XRPL_PRIVATE_KEY env not set")?;
    let wallet = XrplWallet::from_secret_str(&key)?;
    let address = wallet.address();
    let client = XrplClient::new(rpc_url);
    let info = crate::retry::retry_all("probe_xrpl.account_info", || async {
        client.account_info(&address).await
    })
    .await?
    .ok_or_else(|| eyre!("account {address} not activated on XRPL"))?;
    // 1 XRP = 10^6 drops
    Ok((address, info.balance_drops as f64 / 1_000_000.0))
}

fn stellar_network_type(chain: &ChainConfig, network: Network) -> String {
    chain
        .extra
        .get("networkType")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| match network {
            Network::Mainnet => "mainnet".to_string(),
            Network::Testnet | Network::Stagenet | Network::DevnetAmplifier => {
                "testnet".to_string()
            }
        })
}

fn wei_to_eth(wei: alloy::primitives::U256) -> f64 {
    let divisor = alloy::primitives::U256::from(1_000_000_000_000_000_000u64); // 10^18
    let whole = wei / divisor;
    let remainder = wei % divisor;
    let whole_f64: f64 = whole.to::<u128>() as f64;
    let remainder_f64: f64 = remainder.to::<u128>() as f64 / 1e18;
    whole_f64 + remainder_f64
}

fn render_table(rows: &[BalanceRow]) {
    let mut table = Table::new();
    table.load_preset(comfy_table::presets::UTF8_FULL);
    table.set_content_arrangement(ContentArrangement::Dynamic);
    table.set_header(vec![
        header_cell("Chain"),
        header_cell("Type"),
        header_cell("Address"),
        header_cell("Balance"),
        header_cell("Min"),
        header_cell("Status"),
        header_cell("Note"),
    ]);
    for row in rows {
        let (status_label, status_color) = if row.is_fatal() {
            ("LOW", Color::Red)
        } else if row.is_axe_read_error() {
            ("?", Color::Yellow)
        } else {
            ("ok", Color::Green)
        };
        let chain_label = match row.asset {
            Asset::Native => row.chain_key.clone(),
            Asset::Axe => format!("{} (AXE)", row.chain_key),
        };
        table.add_row(vec![
            Cell::new(&chain_label).fg(Color::Cyan),
            Cell::new(row.kind.label()),
            Cell::new(row.address.as_deref().unwrap_or("-")),
            Cell::new(format_balance(row.balance, &row.token_symbol)),
            Cell::new(format!("{:.4} {}", row.threshold, row.token_symbol)),
            Cell::new(status_label).fg(status_color),
            Cell::new(row.note.as_str()).fg(Color::DarkGrey),
        ]);
    }
    println!();
    println!("{table}");
}

fn header_cell(label: &str) -> Cell {
    Cell::new(label)
        .fg(Color::Cyan)
        .add_attribute(Attribute::Bold)
}

fn format_balance(balance: Option<f64>, symbol: &str) -> String {
    match balance {
        Some(b) => format!("{b:.4} {symbol}"),
        None => "-".to_string(),
    }
}

fn format_underfunded(row: &BalanceRow) -> String {
    let address = row.address.as_deref().unwrap_or("?");
    match row.balance {
        Some(b) => format!(
            "{} ({}): {} has {:.4} {} (need >= {} {})",
            row.chain_key,
            row.kind.label(),
            address,
            b,
            row.token_symbol,
            row.threshold,
            row.token_symbol
        ),
        None => format!(
            "{} ({}): {} — {}",
            row.chain_key,
            row.kind.label(),
            address,
            if row.note.is_empty() {
                "balance unknown"
            } else {
                &row.note
            },
        ),
    }
}

fn shorten_error(error: &str) -> String {
    const LIMIT: usize = 80;
    if error.chars().count() <= LIMIT {
        return error.to_string();
    }
    let truncated: String = error.chars().take(LIMIT).collect();
    format!("{truncated}...")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mainnet_uses_stellar_top_level_key() {
        let targets = chain_targets(Network::Mainnet);
        let stellar = targets
            .iter()
            .find(|t| t.kind == ChainKind::Stellar)
            .unwrap();
        assert_eq!(stellar.chain_key, "stellar");
    }

    #[test]
    fn testnet_uses_quarterly_stellar_key() {
        let targets = chain_targets(Network::Testnet);
        let stellar = targets
            .iter()
            .find(|t| t.kind == ChainKind::Stellar)
            .unwrap();
        assert_eq!(stellar.chain_key, "stellar-2026-q1-2");
    }

    #[test]
    fn stagenet_and_devnet_use_their_own_chain_ids() {
        // Stagenet: no Stellar deployment; Monad is plain `monad`; Solana is
        // the `solana-stagenet-3` deployment.
        let stagenet = chain_targets(Network::Stagenet);
        assert!(!stagenet.iter().any(|t| t.kind == ChainKind::Stellar));
        assert!(stagenet.iter().any(|t| t.chain_key == "monad"));
        assert!(stagenet.iter().any(|t| t.chain_key == "solana-stagenet-3"));

        // Devnet-amplifier: its own suffixed ids; no Stellar, no XRPL (wss-only
        // RPC), no Hyperliquid/Hedera/Monad.
        let devnet = chain_targets(Network::DevnetAmplifier);
        assert!(!devnet.iter().any(|t| t.kind == ChainKind::Stellar));
        assert!(!devnet.iter().any(|t| t.kind == ChainKind::Xrpl));
        for key in ["avalanche-fuji", "eth-sepolia", "sui-2", "solana-18"] {
            assert!(
                devnet.iter().any(|t| t.chain_key == key),
                "missing devnet target {key}"
            );
        }
    }

    #[test]
    fn thresholds_match_removed_js_script() {
        // Map: chain_key -> expected threshold. Most values mirror the
        // removed `axelar-contract-deployments/scripts/check-wallet-balances.js`
        // THRESHOLDS. Two axe-specific divergences:
        //   * Solana (0.3) — not in the original JS.
        //   * Stellar (2.5) — bumped from the JS's 0.5 to leave headroom above
        //     the ~1 XLM Stellar account reserve plus per-route op spend.
        let expected: &[(&str, f64)] = &[
            ("hyperliquid", 0.01),
            ("xrpl-evm", 2.0),
            ("xrpl", 3.0),
            ("stellar", 2.5),
            ("sui", 0.05),
            ("solana", 0.3),
        ];
        let targets = chain_targets(Network::Mainnet);
        for (key, want) in expected {
            let got = targets
                .iter()
                .find(|t| t.chain_key == *key)
                .unwrap_or_else(|| panic!("no target for {key}"));
            assert!(
                (got.threshold_units - *want).abs() < f64::EPSILON,
                "{key}: got {} want {want}",
                got.threshold_units
            );
        }
    }

    #[test]
    fn underfunded_when_balance_below_threshold() {
        let row = BalanceRow {
            chain_key: "hyperliquid".to_string(),
            kind: ChainKind::Evm,
            asset: Asset::Native,
            address: Some("0xabc".to_string()),
            balance: Some(0.005),
            threshold: 0.01,
            token_symbol: "ETH".to_string(),
            note: String::new(),
        };
        assert!(row.is_underfunded());
    }

    #[test]
    fn ok_when_balance_above_threshold() {
        let row = BalanceRow {
            chain_key: "hyperliquid".to_string(),
            kind: ChainKind::Evm,
            asset: Asset::Native,
            address: Some("0xabc".to_string()),
            balance: Some(1.0),
            threshold: 0.01,
            token_symbol: "ETH".to_string(),
            note: String::new(),
        };
        assert!(!row.is_underfunded());
    }

    #[test]
    fn underfunded_when_balance_is_none() {
        // Probe error => no balance => treat as underfunded (better safe than sorry).
        let row = BalanceRow {
            chain_key: "hyperliquid".to_string(),
            kind: ChainKind::Evm,
            asset: Asset::Native,
            address: None,
            balance: None,
            threshold: 0.01,
            token_symbol: "ETH".to_string(),
            note: "RPC unreachable".to_string(),
        };
        assert!(row.is_underfunded());
        assert!(row.is_fatal(), "native read error must block the cron");
    }

    fn axe_row(balance: Option<f64>) -> BalanceRow {
        BalanceRow {
            chain_key: "stellar".to_string(),
            kind: ChainKind::Stellar,
            asset: Asset::Axe,
            address: Some("G...".to_string()),
            balance,
            threshold: AXE_MIN_WHOLE_TOKENS,
            token_symbol: "AXE".to_string(),
            note: String::new(),
        }
    }

    #[test]
    fn axe_confirmed_low_is_fatal() {
        assert!(axe_row(Some(50.0)).is_fatal());
    }

    #[test]
    fn axe_above_threshold_ok() {
        let row = axe_row(Some(100_000.0));
        assert!(!row.is_fatal());
        assert!(!row.is_axe_read_error());
    }

    #[test]
    fn axe_read_error_is_advisory_not_fatal() {
        // An unreadable AXE balance must NOT block the cron (unlike native gas).
        let row = axe_row(None);
        assert!(!row.is_fatal());
        assert!(row.is_axe_read_error());
    }

    #[test]
    fn axe_targets_scope_by_network() {
        let mainnet: Vec<_> = axe_targets(Network::Mainnet)
            .iter()
            .map(|t| t.chain_key.clone())
            .collect();
        assert!(mainnet.contains(&"stellar".to_string()));
        assert!(mainnet.contains(&"hyperliquid".to_string()));
        assert!(mainnet.contains(&"monad".to_string()));
        assert!(mainnet.contains(&"hedera".to_string()));
        assert!(mainnet.contains(&"avalanche".to_string()));

        let testnet: Vec<_> = axe_targets(Network::Testnet)
            .iter()
            .map(|t| t.chain_key.clone())
            .collect();
        assert!(testnet.contains(&"monad-3".to_string()));
        assert!(testnet.contains(&"avalanche".to_string()));
        assert!(
            !testnet.iter().any(|k| k.starts_with("stellar")),
            "Stellar was removed from the testnet ITS cron"
        );
    }
}
