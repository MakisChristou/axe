//! Shared source-side submission for Sui ITS routes.
//!
//! Sui keeps its existing sequential, single-wallet behavior. This adapter
//! only moves the chain-specific PTB mechanics behind the common
//! [`TransactionSubmitter`] capability; it does not add sustained mode.

use std::time::Instant;

use super::LoadTestArgs;
use super::identifiers::TokenId;
use super::metrics::{TxMetrics, TxOutcome};
use super::submitter::TransactionSubmitter;
use super::units::Mist;
use crate::sui::{SuiClient, SuiItsContractsConfig, SuiWallet};
use crate::ui;
use eyre::{Result, eyre};

pub(super) struct PreparedSuiIts {
    pub client: SuiClient,
    pub wallet: SuiWallet,
    pub contracts: SuiItsContractsConfig,
    pub coin_type: String,
    pub token_id: TokenId,
    pub balance: u64,
}

pub(super) async fn prepare_source(args: &LoadTestArgs) -> Result<PreparedSuiIts> {
    let source_chain = &args.source_chain;
    let (default_rpc, _) = crate::sui::read_sui_chain_config(&args.config, source_chain)?;
    let rpc = if args.source_rpc.is_empty() {
        default_rpc
    } else {
        args.source_rpc.clone()
    };
    let client = SuiClient::new(&rpc);
    let contracts = crate::sui::read_sui_its_config(&args.config, source_chain)?;
    ui::address(
        "Example::its::Singleton",
        &format!("0x{}", hex::encode(contracts.its_singleton.as_bytes())),
    );
    ui::address(
        "InterchainTokenService",
        &format!("0x{}", hex::encode(contracts.its_object.as_bytes())),
    );
    let wallet = super::load_sui_main_wallet()?;
    ui::kv("Sui wallet", &wallet.address_hex());
    let balance = client.get_balance(&wallet.address).await?;
    ui::kv(
        "Sui balance",
        &format!("{balance} mist ({:.4} SUI)", balance as f64 / 1e9),
    );
    let (token_id, coin_type) = super::resolve_sui_axe_token(
        &args.config,
        source_chain,
        args.token_id.as_deref(),
        args.coin_type.as_deref(),
    )?;
    let coin_type = match coin_type {
        Some(coin_type) => coin_type,
        None => {
            ui::info("resolving Sui coin type via dev-inspect...");
            client
                .dev_inspect_registered_coin_type(
                    &wallet.address,
                    contracts.its_pkg,
                    contracts.its_object,
                    token_id,
                )
                .await?
        }
    };
    ui::kv("token id", &format!("0x{}", hex::encode(token_id)));
    ui::kv("coin type", &coin_type);
    let (_, coin_balance) = client
        .pick_coin_of_type(&wallet.address, &coin_type)
        .await?;
    ui::kv("Coin<T> balance", &coin_balance.to_string());
    Ok(PreparedSuiIts {
        client,
        wallet,
        contracts,
        coin_type,
        token_id: token_id.into(),
        balance,
    })
}

pub(super) fn parse_hub_gas(value: Option<&str>, default: Mist) -> Result<Mist> {
    let per_command = match value {
        Some(value) => value
            .parse::<u64>()
            .map_err(|error| eyre!("invalid --gas-value: {error}"))?,
        None => default.get(),
    };
    let gas = Mist::new(per_command.saturating_mul(2));
    ui::kv(
        "cross-chain gas",
        &format!("{} mist (paid via Sui GasService)", gas.get()),
    );
    Ok(gas)
}

pub(super) fn ensure_gas_balance(balance: u64, gas: Mist, budget: Mist) -> Result<()> {
    let required = gas.get().saturating_add(budget.get());
    if balance < required {
        eyre::bail!(
            "Sui wallet has insufficient SUI: {balance} mist; need ≥ {required} mist (gas budget + cross-chain gas)."
        );
    }
    Ok(())
}

#[derive(Clone)]
pub(super) struct ItsSuiSubmitter {
    pub client: SuiClient,
    pub wallet: SuiWallet,
    pub contracts: SuiItsContractsConfig,
    pub coin_type: String,
    pub token_id: TokenId,
    pub destination_chain: String,
    pub destination_address_bytes: Vec<u8>,
    pub transfer_amount: u64,
    pub gas_value: Mist,
    pub gas_budget: Mist,
    pub its_hub_address: String,
}

impl TransactionSubmitter for ItsSuiSubmitter {
    type Job = ();

    async fn submit(&self, (): Self::Job) -> TxMetrics {
        let send_start = Instant::now();
        let result =
            crate::sui::send_its_interchain_transfer(crate::sui::InterchainTransferRequest {
                client: &self.client,
                wallet: &self.wallet,
                contracts: &self.contracts,
                coin_type_tag: &self.coin_type,
                token_id: self.token_id.into_bytes(),
                destination_chain: &self.destination_chain,
                destination_address_bytes: &self.destination_address_bytes,
                transfer_amount: self.transfer_amount,
                gas_value_mist: self.gas_value.get(),
                gas_budget_mist: self.gas_budget.get(),
            })
            .await;

        match result {
            Ok(result) if result.success => {
                let latency_ms = send_start
                    .elapsed()
                    .as_millis()
                    .try_into()
                    .unwrap_or(u64::MAX);
                TxMetrics {
                    confirm_time_ms: Some(latency_ms),
                    latency_ms: Some(latency_ms),
                    payload_hash: result.payload_hash_hex,
                    source_address: format!("0x{}", result.source_address_hex),
                    gmp_destination_chain: "axelar".to_string(),
                    gmp_destination_address: self.its_hub_address.clone(),
                    send_instant: Some(send_start),
                    ..TxMetrics::succeeded(
                        format!("{}-{}", result.digest, result.event_index),
                        latency_ms,
                    )
                }
            }
            Ok(result) => failed_metrics(TxOutcome::from_external(
                false,
                result.error,
                "Sui ITS tx failed",
            )),
            Err(error) => failed_metrics(TxOutcome::failed(error.to_string())),
        }
    }
}

fn failed_metrics(outcome: super::metrics::TxOutcome) -> TxMetrics {
    TxMetrics::from_outcome("", 0, outcome)
}

pub(super) async fn run_its_sequential(
    submitter: ItsSuiSubmitter,
    num_txs: usize,
) -> eyre::Result<super::submitter::BurstResult> {
    super::submitter::run_serial(submitter, vec![(); num_txs], None).await
}

#[cfg(test)]
mod tests {
    use super::{ensure_gas_balance, parse_hub_gas};
    use crate::commands::load_test::units::Mist;

    #[test]
    fn hub_gas_is_unit_safe_and_doubled() {
        assert_eq!(
            parse_hub_gas(Some("40"), Mist::new(10))
                .expect("valid gas")
                .get(),
            80
        );
        assert_eq!(
            parse_hub_gas(None, Mist::new(10))
                .expect("default gas")
                .get(),
            20
        );
    }

    #[test]
    fn balance_check_includes_cross_chain_gas_and_budget() {
        assert!(ensure_gas_balance(30, Mist::new(20), Mist::new(10)).is_ok());
        assert!(ensure_gas_balance(29, Mist::new(20), Mist::new(10)).is_err());
    }
}
