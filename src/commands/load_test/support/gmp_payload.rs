//! Typed GMP payload encoding.

use alloy::primitives::{Bytes, FixedBytes};
use alloy::sol;
use alloy::sol_types::SolValue;
use rand::Rng;
use solana_sdk::pubkey::Pubkey;

use crate::types::Network;

sol! {
    struct SolanaAccountRepr {
        bytes32 pubkey;
        bool is_signer;
        bool is_writable;
    }

    struct SolanaGatewayPayload {
        bytes execute_payload;
        SolanaAccountRepr[] accounts;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum GmpPayloadEncoding {
    AbiString,
    SolanaExecutable,
}

pub(super) enum GmpPayloadEncoder {
    AbiString,
    SolanaExecutable { counter_pda: Pubkey },
}

impl GmpPayloadEncoding {
    pub(super) fn prepare(self, network: Network) -> GmpPayloadEncoder {
        match self {
            Self::AbiString => GmpPayloadEncoder::AbiString,
            Self::SolanaExecutable => {
                let program_id = memo_program_id(network);
                let (counter_pda, _) = Pubkey::find_program_address(&[b"counter"], &program_id);

                GmpPayloadEncoder::SolanaExecutable { counter_pda }
            }
        }
    }
}

impl GmpPayloadEncoder {
    pub(super) fn encode(&self, custom: &Option<Vec<u8>>) -> Vec<u8> {
        match self {
            Self::AbiString => make_abi_string_payload(custom),
            Self::SolanaExecutable { counter_pda } => make_executable_payload(custom, counter_pda),
        }
    }
}

pub(crate) fn memo_program_id(network: Network) -> Pubkey {
    network.solana_memo_id()
}

fn random_message() -> Vec<u8> {
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill(&mut bytes);

    format!("hello from axe load test {}", hex::encode(bytes)).into_bytes()
}

fn make_abi_string_payload(custom: &Option<Vec<u8>>) -> Vec<u8> {
    match custom {
        Some(payload) => payload.clone(),
        None => (String::from_utf8_lossy(&random_message()).into_owned(),).abi_encode_params(),
    }
}

pub(crate) fn make_executable_payload(custom: &Option<Vec<u8>>, counter_pda: &Pubkey) -> Vec<u8> {
    let execute_payload = custom.clone().unwrap_or_else(random_message);
    let gateway_payload = SolanaGatewayPayload {
        execute_payload: Bytes::from(execute_payload),
        accounts: vec![SolanaAccountRepr {
            pubkey: FixedBytes::from(counter_pda.to_bytes()),
            is_signer: false,
            is_writable: true,
        }],
    };
    let encoded = gateway_payload.abi_encode_params();

    let mut payload = Vec::with_capacity(1 + encoded.len());
    payload.push(0x01);
    payload.extend(encoded);
    payload
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_abi_payload_is_preserved() {
        let custom = Some(vec![1, 2, 3]);

        assert_eq!(GmpPayloadEncoder::AbiString.encode(&custom), vec![1, 2, 3]);
    }

    #[test]
    fn executable_payload_uses_abi_scheme() {
        let counter_pda = Pubkey::new_unique();
        let payload = make_executable_payload(&Some(b"hello".to_vec()), &counter_pda);

        assert_eq!(payload.first(), Some(&0x01));
    }
}
