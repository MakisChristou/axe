//! Hyperliquid big-blocks opt-in.
//!
//! HyperEVM splits its block stream into two types:
//!   * **small blocks** (~2s cadence, ~2M gas) for normal transactions, and
//!   * **big blocks**   (~60s cadence, 30M gas) for contract deployments and
//!     other large txs.
//!
//! Contract-deploy txs from a wallet that hasn't opted into big blocks are
//! rejected by the Hyperliquid validator with a "tx exceeds small-block gas
//! limit" error. axe deploys `SenderReceiver` (GMP) and a fresh ITS token
//! (ITS source-side) on first run for each chain, so the wallet must be in
//! big-blocks mode for both flows when Hyperliquid is involved.
//!
//! `evmUserModify` is an **L1 action** on Hyperliquid — its signing scheme is
//! NOT the typed-data flow you'd guess from EIP-712 + struct fields. Instead:
//!
//!   1. msgpack-encode the bare action object: `{type, usingBigBlocks}`
//!   2. concat: `msgpack || nonce_be_u64 || 0x00` (the `0x00` byte means
//!      "no vault address"; `expires_after` is omitted entirely when None)
//!   3. keccak256 → `action_hash` (a.k.a. `connectionId`)
//!   4. EIP-712 sign over `{source: "a"|"b", connectionId: action_hash}`
//!      with the "Exchange" domain (chainId 1337, verifyingContract 0x0)
//!      and primary type `Agent { source: string, connectionId: bytes32 }`
//!   5. POST `{action, nonce, signature}` to `/exchange`
//!
//! Mirrors the reference implementation in `hyperliquid-python-sdk`'s
//! `utils/signing.py::sign_l1_action`. Keeping this comment block accurate
//! is important — the wire format has no schema validation and a misordered
//! byte just yields HTTP 422.

use std::time::{SystemTime, UNIX_EPOCH};

use alloy::primitives::{B256, address, keccak256};
use alloy::signers::Signer;
use alloy::sol;
use alloy::sol_types::SolStruct;
use eyre::{Result, eyre};
use serde_json::json;

use crate::types::Network;

const MAINNET_API: &str = "https://api.hyperliquid.xyz/exchange";
const TESTNET_API: &str = "https://api.hyperliquid-testnet.xyz/exchange";

/// Whether to talk to the Hyperliquid mainnet or testnet API endpoint.
#[derive(Clone, Copy, Debug)]
pub enum HyperliquidEnv {
    Mainnet,
    Testnet,
}

impl HyperliquidEnv {
    fn api_url(self) -> &'static str {
        match self {
            HyperliquidEnv::Mainnet => MAINNET_API,
            HyperliquidEnv::Testnet => TESTNET_API,
        }
    }

    /// The single-character "source" field embedded in the phantom-agent
    /// message — `a` for mainnet, `b` for testnet. Hardcoded by the
    /// protocol.
    fn phantom_source(self) -> &'static str {
        match self {
            HyperliquidEnv::Mainnet => "a",
            HyperliquidEnv::Testnet => "b",
        }
    }
}

/// Pick the right Hyperliquid environment for the target Axelar network.
/// Stagenet and devnet-amplifier share testnet (no separate Hyperliquid
/// endpoint exists for those).
pub fn env_for(network: Network) -> HyperliquidEnv {
    match network {
        Network::Mainnet => HyperliquidEnv::Mainnet,
        _ => HyperliquidEnv::Testnet,
    }
}

sol! {
    /// EIP-712 inner struct that the wallet actually signs. `connectionId`
    /// is the keccak256 over the msgpack-encoded action plus nonce + vault
    /// flag — Hyperliquid calls this "the phantom agent".
    #[allow(missing_docs)]
    struct Agent {
        string source;
        bytes32 connectionId;
    }
}

/// msgpack encoding of `{"type": "evmUserModify", "usingBigBlocks": <bool>}`.
///
/// Hand-rolled because pulling in a general-purpose msgpack crate just to
/// encode this single action would be over-engineering. The byte sequence
/// is verified against the reference Python SDK output:
///
/// ```text
/// 82                             fixmap (2 entries)
/// a4 74 79 70 65                 fixstr 4: "type"
/// ad 65 76 6d 55 73 65 72 4d ... fixstr 13: "evmUserModify"
/// ae 75 73 69 6e 67 42 69 67 ... fixstr 14: "usingBigBlocks"
/// c3 | c2                        bool true | false
/// ```
///
/// If we ever need to sign other L1 actions, switch to `rmp-serde`.
fn msgpack_evm_user_modify(enable: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(36);
    out.push(0x82); // fixmap, 2 entries
    out.push(0xa4); // fixstr len 4
    out.extend_from_slice(b"type");
    out.push(0xad); // fixstr len 13
    out.extend_from_slice(b"evmUserModify");
    out.push(0xae); // fixstr len 14
    out.extend_from_slice(b"usingBigBlocks");
    out.push(if enable { 0xc3 } else { 0xc2 });
    out
}

/// Compute the action hash for `evmUserModify` with no vault address and
/// no `expires_after`. Matches `signing.action_hash` in the Python SDK.
fn action_hash_evm_user_modify(enable: bool, nonce_ms: u64) -> B256 {
    let mut data = msgpack_evm_user_modify(enable);
    data.extend_from_slice(&nonce_ms.to_be_bytes());
    data.push(0x00); // no vault address
    // expires_after intentionally omitted (matches `expires_after=None`)
    keccak256(&data)
}

/// Enable or disable Hyperliquid big-blocks for the supplied wallet.
///
/// Successful response from `/exchange` looks like
/// `{"status":"ok","response":{"type":"default"}}`; any logical failure
/// arrives as HTTP 200 with `"status":"err"` and a message field, which we
/// surface verbatim.
pub async fn set_big_blocks<S>(signer: &S, enable: bool, env: HyperliquidEnv) -> Result<()>
where
    S: Signer + Send + Sync,
{
    let nonce_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| eyre!("clock before unix epoch: {e}"))?
        .as_millis() as u64;

    let action_hash = action_hash_evm_user_modify(enable, nonce_ms);

    let agent = Agent {
        source: env.phantom_source().to_string(),
        connectionId: action_hash,
    };

    let domain = alloy::sol_types::eip712_domain! {
        name: "Exchange",
        version: "1",
        chain_id: 1337u64,
        verifying_contract: address!("0000000000000000000000000000000000000000"),
    };

    let digest = agent.eip712_signing_hash(&domain);
    let sig = signer
        .sign_hash(&digest)
        .await
        .map_err(|e| eyre!("Hyperliquid agent sign failed: {e}"))?;

    // Hyperliquid expects v as 27/28 (legacy form). alloy returns y-parity
    // as a bool; map 0 -> 27, 1 -> 28.
    let v = u64::from(sig.v()) + 27;

    let body = json!({
        "action": {
            "type": "evmUserModify",
            "usingBigBlocks": enable,
        },
        "nonce": nonce_ms,
        "signature": {
            "r": format!("0x{:064x}", sig.r()),
            "s": format!("0x{:064x}", sig.s()),
            "v": v,
        },
    });

    let resp = crate::http::client()
        .post(env.api_url())
        .json(&body)
        .send()
        .await
        .map_err(|e| eyre!("Hyperliquid POST failed: {e}"))?;

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        eyre::bail!("Hyperliquid /exchange returned HTTP {status}: {text}");
    }
    #[derive(serde::Deserialize)]
    struct ExchangeResponse {
        status: String,
    }
    let exchange_response: ExchangeResponse = serde_json::from_str(&text)
        .map_err(|error| eyre!("Hyperliquid returned malformed response: {error}: {text}"))?;
    if exchange_response.status != "ok" {
        eyre::bail!("Hyperliquid rejected big-blocks toggle: {text}");
    }
    Ok(())
}

/// Convenience wrapper: parse an EVM private key hex string into a signer
/// and enable big-blocks for it. Returns the wallet address on success so
/// the caller can log who got opted in.
pub async fn enable_big_blocks_from_key(
    private_key: &str,
    env: HyperliquidEnv,
) -> Result<alloy::primitives::Address> {
    let signer: alloy::signers::local::PrivateKeySigner = private_key
        .parse()
        .map_err(|e| eyre!("invalid EVM private key: {e}"))?;
    let address = signer.address();
    set_big_blocks(&signer, true, env).await?;
    Ok(address)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hand-rolled msgpack must match the reference Python SDK output for
    /// the same action. Regenerated via:
    ///   python -c "import msgpack; print(msgpack.packb({'type':'evmUserModify','usingBigBlocks':True}).hex())"
    #[test]
    fn msgpack_matches_python_sdk() {
        let expected_true =
            hex::decode("82a474797065ad65766d557365724d6f64696679ae7573696e67426967426c6f636b73c3")
                .unwrap();
        let expected_false =
            hex::decode("82a474797065ad65766d557365724d6f64696679ae7573696e67426967426c6f636b73c2")
                .unwrap();
        assert_eq!(msgpack_evm_user_modify(true), expected_true);
        assert_eq!(msgpack_evm_user_modify(false), expected_false);
    }

    /// action_hash for a known nonce + true matches the reference Python SDK output.
    #[test]
    fn action_hash_matches_python_sdk() {
        // From signing.action_hash({'type':'evmUserModify','usingBigBlocks':True}, None, 1778697419000, None)
        let expected =
            hex::decode("4ea5111bb9b01cbdd69274ced17fb531318c77006e405b589de1962b6469aba2")
                .unwrap();
        let got = action_hash_evm_user_modify(true, 1_778_697_419_000);
        assert_eq!(got.as_slice(), expected.as_slice());
    }
}
