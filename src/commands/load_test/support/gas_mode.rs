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
    /// EIP-1559 chain. Carries the node's own `eth_gasPrice` quote and the
    /// latest base fee so `apply` can floor the tip and max fee at what the
    /// mempool actually accepts: some chains (the Blast family) enforce a
    /// minimum gas price orders of magnitude above `2 x baseFee`, and a
    /// base-fee-derived estimate is accepted by the node, silently dropped,
    /// and never mined (observed on blast-sepolia: base fee 252 wei, node
    /// floor ~0.001 gwei). On chains without such a floor the quote is
    /// `baseFee + market tip`, so the floor collapses into the normal
    /// estimate and nothing overpays.
    Eip1559 {
        gas_price: u128,
        base_fee: u128,
        suggested_tip: u128,
    },
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
        let base_fee = block
            .get("baseFeePerGas")
            .and_then(|v| v.as_str())
            .and_then(|hex| u128::from_str_radix(hex.trim_start_matches("0x"), 16).ok());
        let gas_price = provider.get_gas_price().await?;
        match base_fee {
            Some(base_fee) => Ok(Self::Eip1559 {
                gas_price,
                base_fee,
                // Some chains put the whole floor in the base fee (quote ==
                // base), so `quote - base` alone can collapse to nothing.
                // Blend in the node's own tip suggestion as a second floor.
                suggested_tip: provider.get_max_priority_fee_per_gas().await.unwrap_or(0),
            }),
            None => Ok(Self::Legacy { gas_price }),
        }
    }

    /// The legacy `gas_price` if this is a legacy chain, else `None`. Apply to
    /// an alloy contract `CallBuilder` via `.gas_price(..)`.
    pub(crate) fn legacy_gas_price(&self) -> Option<u128> {
        match self {
            Self::Eip1559 { .. } => None,
            Self::Legacy { gas_price } => Some(*gas_price),
        }
    }

    /// Apply the fee mode to a raw `TransactionRequest`: legacy sets
    /// `gas_price` (making it a type-0 tx), 1559 sets explicit tip and max
    /// fee floored at the node's own quote so the tx clears any enforced
    /// minimum gas price (see [`EvmFeeMode::Eip1559`]).
    pub(crate) fn apply(&self, tx: TransactionRequest) -> TransactionRequest {
        match self {
            Self::Legacy { gas_price } => tx.gas_price(*gas_price),
            Self::Eip1559 {
                gas_price,
                base_fee,
                suggested_tip,
            } => {
                let tip = gas_price
                    .saturating_sub(*base_fee)
                    .max(*suggested_tip)
                    .max(1);
                let max_fee = (*gas_price).max(base_fee.saturating_mul(2).saturating_add(tip));
                tx.max_priority_fee_per_gas(tip).max_fee_per_gas(max_fee)
            }
        }
    }
}
