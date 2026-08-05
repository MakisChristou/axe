//! EVM fee mode detection.
//!
//! Some consensus chains (e.g. Kava) predate EIP-1559: their blocks carry no
//! `baseFeePerGas` and `eth_feeHistory` returns nulls that break alloy's 1559
//! fee estimation with a hard deserialization error — before any legacy
//! fallback runs. We detect that case and send legacy type-0 transactions with
//! an explicit `gas_price`, which routes alloy's gas filler through the legacy
//! path and skips `eth_feeHistory` entirely.

use alloy::eips::BlockNumberOrTag;
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use eyre::Result;

/// How to price EVM transactions on a given chain.
#[derive(Clone, Copy, Debug)]
pub(crate) enum EvmFeeMode {
    /// EIP-1559 chain — let alloy's default filler estimate the 1559 fees.
    Eip1559,
    /// Legacy (pre-1559) chain — send type-0 txs with this `gas_price`.
    Legacy { gas_price: u128 },
}

impl EvmFeeMode {
    /// Probe the chain: a latest block with no `baseFeePerGas` means the chain
    /// has no EIP-1559, so fetch the legacy `gas_price` to use instead.
    ///
    /// Reads the block as raw JSON rather than alloy's typed `Block` — some
    /// chains (e.g. Moonbeam) return blocks missing fields alloy requires
    /// (`mixHash`), which would fail typed deserialization even though all we
    /// need is the optional `baseFeePerGas`.
    pub(crate) async fn detect<P: Provider>(provider: &P) -> Result<Self> {
        let block: serde_json::Value = provider
            .raw_request(
                "eth_getBlockByNumber".into(),
                (BlockNumberOrTag::Latest, false),
            )
            .await?;
        let has_base_fee = block.get("baseFeePerGas").is_some_and(|v| !v.is_null());
        if has_base_fee {
            Ok(Self::Eip1559)
        } else {
            Ok(Self::Legacy {
                gas_price: provider.get_gas_price().await?,
            })
        }
    }

    /// The legacy `gas_price` if this is a legacy chain, else `None`. Apply to
    /// an alloy contract `CallBuilder` via `.gas_price(..)`.
    pub(crate) fn legacy_gas_price(&self) -> Option<u128> {
        match self {
            Self::Eip1559 => None,
            Self::Legacy { gas_price } => Some(*gas_price),
        }
    }

    /// Apply the fee mode to a raw `TransactionRequest` (legacy → set
    /// `gas_price`, which makes it a type-0 tx; 1559 → leave untouched).
    pub(crate) fn apply(&self, tx: TransactionRequest) -> TransactionRequest {
        match self.legacy_gas_price() {
            Some(gp) => tx.gas_price(gp),
            None => tx,
        }
    }
}
