//! Typed cross-chain identifiers.

use std::fmt;
use std::ops::Deref;

use alloy::primitives::FixedBytes;

/// A 32-byte ITS token identifier, independent of any chain SDK's preferred
/// byte container.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct TokenId([u8; 32]);

impl TokenId {
    pub(super) const fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub(super) const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub(super) const fn into_bytes(self) -> [u8; 32] {
        self.0
    }

    pub(super) fn into_fixed_bytes(self) -> FixedBytes<32> {
        FixedBytes::from(self.0)
    }
}

impl From<[u8; 32]> for TokenId {
    fn from(value: [u8; 32]) -> Self {
        Self::new(value)
    }
}

impl From<FixedBytes<32>> for TokenId {
    fn from(value: FixedBytes<32>) -> Self {
        Self::new(value.into())
    }
}

impl fmt::Display for TokenId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&hex::encode(self.0))
    }
}

/// A protocol-formatted Axelar message identifier.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(super) struct MessageId(String);

impl MessageId {
    pub(super) fn into_string(self) -> String {
        self.0
    }
}

impl From<String> for MessageId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for MessageId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl Deref for MessageId {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl AsRef<str> for MessageId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for MessageId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// A parsed 32-byte payload hash. Hex strings are accepted only at explicit
/// parsing boundaries, never inside polling state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct PayloadHash(FixedBytes<32>);

impl PayloadHash {
    pub(super) const fn into_fixed_bytes(self) -> FixedBytes<32> {
        self.0
    }
}

impl From<FixedBytes<32>> for PayloadHash {
    fn from(value: FixedBytes<32>) -> Self {
        Self(value)
    }
}

#[cfg(test)]
mod tests {
    use super::{MessageId, PayloadHash, TokenId};
    use alloy::primitives::FixedBytes;

    #[test]
    fn token_id_bridges_chain_sdk_byte_types_without_changing_bytes() {
        let bytes = [7u8; 32];
        let token_id = TokenId::from(FixedBytes::from(bytes));

        assert_eq!(token_id.as_bytes(), &bytes);
        assert_eq!(token_id.into_bytes(), bytes);
        assert_eq!(token_id.into_fixed_bytes(), FixedBytes::from(bytes));
    }

    #[test]
    fn verification_identifiers_keep_format_and_hash_bytes() {
        let message_id = MessageId::from("0xabc-1");
        let hash = PayloadHash::from(FixedBytes::from([9u8; 32]));

        assert_eq!(message_id.as_ref(), "0xabc-1");
        assert_eq!(hash.into_fixed_bytes(), FixedBytes::from([9u8; 32]));
    }
}
