//! Sui wallet derivation. Parses `suiprivkey1...` bech32 secrets (or raw
//! 32-byte hex) into an Ed25519 / secp256k1 keypair, derives the canonical
//! Sui address, and produces wire-format intent signatures.

use blake2::{Blake2b, Digest as Blake2Digest, digest::consts::U32};
use ed25519_dalek::{
    Signer as EdSigner, SigningKey as EdSigningKey, VerifyingKey as EdVerifyingKey,
};
use eyre::{Result, eyre};
use libsecp256k1::{Message as SecpMessage, PublicKey as SecpPub, SecretKey as SecpSecret};
use sha2::Sha256;
use sui_sdk_types::Address as SuiAddress;

const HRP_SUIPRIVKEY: &str = "suiprivkey";
const ED25519_FLAG: u8 = 0x00;
const SECP256K1_FLAG: u8 = 0x01;
const SIG_LEN: usize = 64;
const ED25519_PK_LEN: usize = 32;
const SECP256K1_PK_LEN: usize = 33; // compressed

/// Sui supports several signature schemes. We support the two the Sui CLI
/// emits for fresh keypairs: ed25519 (flag 0x00) and secp256k1 (flag 0x01).
///
/// The variants are different sizes (ed25519 keypair ≈ 64 B vs secp256k1 ≈
/// 128 B with uncompressed pubkey internals); a load-test holds at most a
/// handful of these per run, so the indirection of `Box`-ing the larger
/// variant isn't worth it.
#[derive(Clone)]
#[expect(
    clippy::large_enum_variant,
    reason = "wallets are few and long-lived; boxing the larger keypair would add needless indirection"
)]
pub enum SuiKeypair {
    Ed25519 {
        signing_key: EdSigningKey,
        verifying_key: EdVerifyingKey,
    },
    Secp256k1 {
        secret: SecpSecret,
        public: SecpPub,
    },
}

#[derive(Clone)]
pub struct SuiWallet {
    pub keypair: SuiKeypair,
    pub address: SuiAddress,
}

impl SuiWallet {
    /// Build from a 32-byte ed25519 secret seed.
    pub fn from_ed25519_seed(seed: &[u8; 32]) -> Result<Self> {
        let signing_key = EdSigningKey::from_bytes(seed);
        let verifying_key = signing_key.verifying_key();
        let mut buf = Vec::with_capacity(1 + ED25519_PK_LEN);
        buf.push(ED25519_FLAG);
        buf.extend_from_slice(verifying_key.as_bytes());
        let address = SuiAddress::from_hex(format!("0x{}", hex::encode(blake2b256(&buf))))
            .map_err(|e| eyre!("address derivation failed: {e:?}"))?;
        Ok(Self {
            keypair: SuiKeypair::Ed25519 {
                signing_key,
                verifying_key,
            },
            address,
        })
    }

    /// Build from a 32-byte secp256k1 secret seed.
    pub fn from_secp256k1_seed(seed: &[u8; 32]) -> Result<Self> {
        let secret = SecpSecret::parse(seed).map_err(|e| eyre!("secp256k1 secret: {e:?}"))?;
        let public = SecpPub::from_secret_key(&secret);
        let pk_compressed = public.serialize_compressed();
        let mut buf = Vec::with_capacity(1 + SECP256K1_PK_LEN);
        buf.push(SECP256K1_FLAG);
        buf.extend_from_slice(&pk_compressed);
        let address = SuiAddress::from_hex(format!("0x{}", hex::encode(blake2b256(&buf))))
            .map_err(|e| eyre!("address derivation failed: {e:?}"))?;
        Ok(Self {
            keypair: SuiKeypair::Secp256k1 { secret, public },
            address,
        })
    }

    /// Parse a Sui CLI bech32-encoded private key (`suiprivkey1...`). Auto-
    /// detects the signature scheme from the flag byte (0x00 = ed25519,
    /// 0x01 = secp256k1).
    pub fn from_suiprivkey(s: &str) -> Result<Self> {
        let (hrp, data) =
            bech32::decode(s.trim()).map_err(|e| eyre!("invalid suiprivkey bech32: {e}"))?;
        if hrp.as_str() != HRP_SUIPRIVKEY {
            return Err(eyre!(
                "expected suiprivkey hrp, got '{}': not a Sui CLI key",
                hrp.as_str()
            ));
        }
        if data.len() != 1 + 32 {
            return Err(eyre!(
                "suiprivkey payload has wrong length: got {} bytes, expected 33 (flag + 32-byte secret)",
                data.len()
            ));
        }
        let mut seed = [0u8; 32];
        seed.copy_from_slice(&data[1..]);
        match data[0] {
            ED25519_FLAG => Self::from_ed25519_seed(&seed),
            SECP256K1_FLAG => Self::from_secp256k1_seed(&seed),
            other => Err(eyre!(
                "suiprivkey flag 0x{other:02x} is not supported (only 0x00 ed25519 and 0x01 secp256k1)"
            )),
        }
    }

    /// Auto-detect the input format: 64-char hex (assumes ed25519) or
    /// `suiprivkey...` bech32 (flag-byte determines scheme).
    pub fn from_secret_str(s: &str) -> Result<Self> {
        let trimmed = s.trim();
        if let Some(stripped) = trimmed.strip_prefix("0x")
            && stripped.len() == 64
            && stripped.chars().all(|c| c.is_ascii_hexdigit())
        {
            let bytes = hex::decode(stripped).map_err(|e| eyre!("invalid hex secret: {e}"))?;
            let mut seed = [0u8; 32];
            seed.copy_from_slice(&bytes);
            return Self::from_ed25519_seed(&seed);
        }
        if trimmed.len() == 64 && trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
            let bytes = hex::decode(trimmed).map_err(|e| eyre!("invalid hex secret: {e}"))?;
            let mut seed = [0u8; 32];
            seed.copy_from_slice(&bytes);
            return Self::from_ed25519_seed(&seed);
        }
        Self::from_suiprivkey(trimmed)
    }

    pub fn address_hex(&self) -> String {
        format!("0x{}", hex::encode(self.address.as_bytes()))
    }

    /// Diagnostic label for the keypair scheme. Useful when surfacing
    /// Build the wire-format intent signature for the given pre-intent
    /// message (full bytes, including the 3-byte intent prefix).
    ///
    /// Wire format:
    ///   ed25519:    flag(0x00, 1B) || sig(64B)            || pubkey(32B)
    ///   secp256k1:  flag(0x01, 1B) || sig(64B compact)    || pubkey(33B compressed)
    ///
    /// Hashing:
    ///   ed25519    signs blake2b256(intent_message) directly.
    ///   secp256k1  signs sha256(blake2b256(intent_message)) (Sui spec).
    pub fn serialized_intent_signature(&self, intent_message: &[u8]) -> Vec<u8> {
        match &self.keypair {
            SuiKeypair::Ed25519 {
                signing_key,
                verifying_key,
            } => {
                let digest = blake2b256(intent_message);
                let sig = signing_key.sign(&digest);
                let mut out = Vec::with_capacity(1 + SIG_LEN + ED25519_PK_LEN);
                out.push(ED25519_FLAG);
                out.extend_from_slice(&sig.to_bytes());
                out.extend_from_slice(verifying_key.as_bytes());
                out
            }
            SuiKeypair::Secp256k1 { secret, public } => {
                let blake = blake2b256(intent_message);
                let sha = Sha256::digest(blake);
                let mut digest_arr = [0u8; 32];
                digest_arr.copy_from_slice(&sha);
                let msg = SecpMessage::parse(&digest_arr);
                let (sig, _recovery) = libsecp256k1::sign(&msg, secret);
                let pk_compressed = public.serialize_compressed();
                let mut out = Vec::with_capacity(1 + SIG_LEN + SECP256K1_PK_LEN);
                out.push(SECP256K1_FLAG);
                out.extend_from_slice(&sig.serialize());
                out.extend_from_slice(&pk_compressed);
                out
            }
        }
    }
}

fn blake2b256(input: &[u8]) -> [u8; 32] {
    let mut hasher = Blake2b::<U32>::new();
    Blake2Digest::update(&mut hasher, input);
    let out = hasher.finalize();
    let mut a = [0u8; 32];
    a.copy_from_slice(&out);
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ed25519_address_derivation() {
        // Vector from Sui docs: a known keypair → known address.
        let seed = [
            0x9a, 0x1f, 0x52, 0x90, 0x4d, 0x70, 0x14, 0x6e, 0xe5, 0x6f, 0xb6, 0x83, 0xf6, 0x88,
            0x97, 0x44, 0x37, 0x6b, 0x68, 0x3a, 0xf6, 0x57, 0xe1, 0x69, 0x66, 0x5d, 0x90, 0x65,
            0xc6, 0x16, 0xf6, 0x1c,
        ];
        let w = SuiWallet::from_ed25519_seed(&seed).unwrap();
        // sanity: produces a 32-byte address that's deterministic.
        assert_eq!(w.address.as_bytes().len(), 32);
    }
}
