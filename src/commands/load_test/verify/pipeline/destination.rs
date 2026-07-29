//! Destination-chain capabilities used by the shared GMP polling loop.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use alloy::providers::Provider;
use eyre::{Result, WrapErr};
use futures::StreamExt;

use super::required_payload_hash;
use crate::commands::load_test::verify::PendingTx;
use crate::commands::load_test::verify::checks::{
    batch_check_solana_incoming_messages, check_evm_command_executed,
    check_evm_is_message_approved, check_evm_is_message_executed,
};
use crate::commands::load_test::verify::legacy;
use crate::commands::load_test::verify::state::Phase;
use crate::evm::AxelarAmplifierGateway;
use crate::ui;

/// Typed result of one destination-chain observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::commands::load_test::verify) enum DestinationStatus {
    Pending,
    Approved { command_id: Option<[u8; 32]> },
    Executed { command_id: Option<[u8; 32]> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::commands::load_test::verify) struct DestinationObservation {
    pub(in crate::commands::load_test::verify) index: usize,
    pub(in crate::commands::load_test::verify) status: DestinationStatus,
}

pub(in crate::commands::load_test::verify) type DestinationCheckFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Vec<DestinationObservation>>> + Send + 'a>>;

/// Chain-specific capability required by the shared GMP polling loop.
pub(in crate::commands::load_test::verify) trait DestinationVerifier {
    fn approval_label(&self) -> &str;
    fn execution_label(&self) -> &str;

    /// Whether this destination needs `PendingTx::contract_addr` backfilled
    /// from the route.  Streaming senders cannot populate it themselves, so
    /// the pipeline parses the address from its typed arguments and fills in
    /// any `Address::ZERO` it receives.
    fn needs_contract_address(&self) -> bool {
        false
    }

    fn check<'a>(
        &'a self,
        source_chain: &'a str,
        txs: &'a [PendingTx],
        indices: &'a [usize],
    ) -> DestinationCheckFuture<'a>;
}

/// EVM destination adapter. Amplifier and legacy gateways expose different
/// approval APIs, but both satisfy the same destination verification
/// capability.
pub(in crate::commands::load_test::verify) enum EvmDestinationVerifier<'a, P: Provider> {
    Amplifier {
        gw_contract: &'a AxelarAmplifierGateway::AxelarAmplifierGatewayInstance<&'a P>,
        /// When true, only conclude execution after the approval has actually
        /// been observed (`isMessageApproved == true`) at least once. The
        /// amplifier fast-path otherwise reads an unapproved result as
        /// "approved+executed between polls" — valid only once the message is
        /// known to be en route (it passed the `routed` phase). A
        /// consensus→amplifier route enters the Approved phase immediately, so
        /// it must observe the real approval or it would false-positive on the
        /// first poll.
        require_observed_approval: bool,
    },
    /// Legacy (consensus) EVM destination — verified via the old
    /// `AxelarGateway`: locate the emitted `ContractCallApproved` (authoritative
    /// `commandId`) then confirm `isCommandExecuted`. `from_block` bounds the
    /// approval-event scan to blocks produced since verification started.
    /// `match_by_payload` selects how the approval log is matched: an EVM source
    /// pins it with the exact `sourceTxHash` (precise); a non-EVM source has no
    /// EVM tx hash, so it matches on the (unique) `payloadHash` + dest address.
    Legacy {
        gw_contract: &'a AxelarAmplifierGateway::AxelarAmplifierGatewayInstance<&'a P>,
        from_block: u64,
        match_by_payload: bool,
    },
}

async fn check_evm_amplifier_destination<P: Provider>(
    gw_contract: &AxelarAmplifierGateway::AxelarAmplifierGatewayInstance<&P>,
    require_observed_approval: bool,
    source_chain: &str,
    txs: &[PendingTx],
    indices: &[usize],
) -> Result<Vec<DestinationObservation>> {
    let mut futures = Vec::with_capacity(indices.len());
    for &index in indices {
        let Some(phase) = txs[index].phase() else {
            continue;
        };
        let message_id = txs[index].message_id.clone();
        let source_address = txs[index].source_address.clone();
        let contract_address = txs[index].contract_addr;
        let payload_hash = required_payload_hash(&txs[index])?;
        futures.push(async move {
            let approved = check_evm_is_message_approved(
                gw_contract,
                source_chain,
                &message_id,
                &source_address,
                contract_address,
                payload_hash,
            )
            .await?;
            let executed =
                check_evm_is_message_executed(gw_contract, source_chain, &message_id).await?;
            let status = match phase {
                Phase::Approved if executed => DestinationStatus::Executed { command_id: None },
                Phase::Approved if approved => DestinationStatus::Approved { command_id: None },
                Phase::Approved if !require_observed_approval => {
                    DestinationStatus::Executed { command_id: None }
                }
                Phase::Executed if !approved => DestinationStatus::Executed { command_id: None },
                _ => DestinationStatus::Pending,
            };
            Ok(DestinationObservation { index, status })
        });
    }
    let results: Vec<Result<_>> = futures::stream::iter(futures)
        .buffer_unordered(20)
        .collect()
        .await;
    let mut observations = Vec::with_capacity(results.len());
    for result in results {
        match result {
            Ok(observation) => observations.push(observation),
            Err(error) => ui::warn(&format!(
                "destination check RPC error (keeping in-flight for GMP-API recheck): {error}"
            )),
        }
    }
    Ok(observations)
}

async fn legacy_approval_status<P: Provider>(
    gw_contract: &AxelarAmplifierGateway::AxelarAmplifierGatewayInstance<&P>,
    tx: &PendingTx,
    from_block: u64,
    match_by_payload: bool,
) -> Result<DestinationStatus> {
    let payload_hash = required_payload_hash(tx)?;
    let found = if match_by_payload {
        legacy::find_contract_call_approved_by_payload(
            gw_contract.provider(),
            *gw_contract.address(),
            tx.contract_addr,
            payload_hash,
            from_block,
        )
        .await?
    } else {
        let source_tx_hash = legacy::source_tx_hash_from_message_id(&tx.message_id)?;
        legacy::find_contract_call_approved(
            gw_contract.provider(),
            *gw_contract.address(),
            tx.contract_addr,
            payload_hash,
            source_tx_hash,
            from_block,
        )
        .await?
    };
    let Some(command_id) = found else {
        return Ok(DestinationStatus::Pending);
    };
    if check_evm_command_executed(gw_contract, command_id.into()).await? {
        Ok(DestinationStatus::Executed {
            command_id: Some(command_id),
        })
    } else {
        Ok(DestinationStatus::Approved {
            command_id: Some(command_id),
        })
    }
}

async fn check_evm_legacy_destination<P: Provider>(
    gw_contract: &AxelarAmplifierGateway::AxelarAmplifierGatewayInstance<&P>,
    from_block: u64,
    match_by_payload: bool,
    txs: &[PendingTx],
    indices: &[usize],
) -> Result<Vec<DestinationObservation>> {
    let mut observations = Vec::with_capacity(indices.len());
    for &index in indices {
        let tx = &txs[index];
        let status = match tx.phase() {
            Some(Phase::Approved) => {
                legacy_approval_status(gw_contract, tx, from_block, match_by_payload).await?
            }
            Some(Phase::Executed) => {
                let command_id = tx.command_id().ok_or_else(|| {
                    eyre::eyre!(
                        "legacy tx {} in Executed phase without a commandId",
                        tx.message_id
                    )
                })?;
                if check_evm_command_executed(gw_contract, command_id.into()).await? {
                    DestinationStatus::Executed {
                        command_id: Some(command_id),
                    }
                } else {
                    DestinationStatus::Pending
                }
            }
            _ => DestinationStatus::Pending,
        };
        observations.push(DestinationObservation { index, status });
    }
    Ok(observations)
}

impl<P: Provider> DestinationVerifier for EvmDestinationVerifier<'_, P> {
    fn approval_label(&self) -> &str {
        match self {
            Self::Amplifier { .. } => "EVM approval",
            Self::Legacy { .. } => "EVM(legacy) approval",
        }
    }

    fn execution_label(&self) -> &str {
        match self {
            Self::Amplifier { .. } => "EVM execution",
            Self::Legacy { .. } => "EVM(legacy) execution",
        }
    }

    fn needs_contract_address(&self) -> bool {
        true
    }

    fn check<'a>(
        &'a self,
        source_chain: &'a str,
        txs: &'a [PendingTx],
        indices: &'a [usize],
    ) -> DestinationCheckFuture<'a> {
        Box::pin(async move {
            match self {
                Self::Amplifier {
                    gw_contract,
                    require_observed_approval,
                } => {
                    check_evm_amplifier_destination(
                        gw_contract,
                        *require_observed_approval,
                        source_chain,
                        txs,
                        indices,
                    )
                    .await
                }
                Self::Legacy {
                    gw_contract,
                    from_block,
                    match_by_payload,
                } => {
                    check_evm_legacy_destination(
                        gw_contract,
                        *from_block,
                        *match_by_payload,
                        txs,
                        indices,
                    )
                    .await
                }
            }
        })
    }
}

pub(in crate::commands::load_test::verify) struct SolanaDestinationVerifier {
    pub rpc_client: Arc<solana_client::rpc_client::RpcClient>,
    pub network: crate::types::Network,
}

impl DestinationVerifier for SolanaDestinationVerifier {
    fn approval_label(&self) -> &str {
        "Solana approval"
    }

    fn execution_label(&self) -> &str {
        "Solana execution"
    }

    fn check<'a>(
        &'a self,
        _source_chain: &'a str,
        txs: &'a [PendingTx],
        indices: &'a [usize],
    ) -> DestinationCheckFuture<'a> {
        Box::pin(async move {
            let data = indices
                .iter()
                .map(|&index| {
                    let command_id = txs[index].command_id().ok_or_else(|| {
                        eyre::eyre!(
                            "tx {} missing Solana command_id for destination check",
                            txs[index].message_id
                        )
                    })?;
                    Ok((index, command_id))
                })
                .collect::<Result<Vec<_>>>()?;
            let client = Arc::clone(&self.rpc_client);
            let network = self.network;
            let results = tokio::task::spawn_blocking(move || {
                batch_check_solana_incoming_messages(&client, network, &data)
            })
            .await
            .wrap_err("Solana destination check task failed")??;
            Ok(results
                .into_iter()
                .map(|(index, status)| DestinationObservation {
                    index,
                    status: match status {
                        None => DestinationStatus::Pending,
                        Some(0) => DestinationStatus::Approved { command_id: None },
                        Some(_) => DestinationStatus::Executed { command_id: None },
                    },
                })
                .collect())
        })
    }
}

pub(in crate::commands::load_test::verify) struct StellarDestinationVerifier {
    pub client: crate::stellar::StellarClient,
    pub gateway_contract: String,
    pub signer_pk: [u8; 32],
}

impl DestinationVerifier for StellarDestinationVerifier {
    fn approval_label(&self) -> &str {
        "Stellar approval"
    }

    fn execution_label(&self) -> &str {
        "Stellar execution"
    }

    fn check<'a>(
        &'a self,
        source_chain: &'a str,
        txs: &'a [PendingTx],
        indices: &'a [usize],
    ) -> DestinationCheckFuture<'a> {
        Box::pin(async move {
            let mut observations = Vec::with_capacity(indices.len());
            for &index in indices {
                let tx = &txs[index];
                let approved = self
                    .client
                    .gateway_is_message_approved(crate::stellar::MessageApprovalQuery {
                        signer_account_pk: &self.signer_pk,
                        gateway_contract: &self.gateway_contract,
                        source_chain,
                        message_id: &tx.message_id,
                        source_address: &tx.source_address,
                        contract_address: &tx.gmp_destination_address,
                        payload_hash: required_payload_hash(tx)?.0,
                    })
                    .await?
                    .ok_or_else(|| {
                        eyre::eyre!(
                            "Stellar gateway returned non-bool approval result for tx {}",
                            tx.message_id
                        )
                    })?;
                let status = match tx.phase() {
                    Some(Phase::Approved) if approved => {
                        DestinationStatus::Approved { command_id: None }
                    }
                    Some(Phase::Executed) => {
                        let executed = self
                            .client
                            .gateway_is_message_executed(
                                &self.signer_pk,
                                &self.gateway_contract,
                                source_chain,
                                &tx.message_id,
                            )
                            .await?
                            .ok_or_else(|| {
                                eyre::eyre!(
                                    "Stellar gateway returned non-bool execution result for tx {}",
                                    tx.message_id
                                )
                            })?;
                        if executed {
                            DestinationStatus::Executed { command_id: None }
                        } else {
                            DestinationStatus::Pending
                        }
                    }
                    _ => DestinationStatus::Pending,
                };
                observations.push(DestinationObservation { index, status });
            }
            Ok(observations)
        })
    }
}

pub(in crate::commands::load_test::verify) struct SuiDestinationVerifier {
    pub client: crate::sui::SuiClient,
    pub gateway_pkg: String,
}

impl DestinationVerifier for SuiDestinationVerifier {
    fn approval_label(&self) -> &str {
        "Sui approval"
    }

    fn execution_label(&self) -> &str {
        "Sui execution"
    }

    fn check<'a>(
        &'a self,
        source_chain: &'a str,
        txs: &'a [PendingTx],
        indices: &'a [usize],
    ) -> DestinationCheckFuture<'a> {
        Box::pin(async move {
            let approved_event_type = format!("{}::events::MessageApproved", self.gateway_pkg);
            let executed_event_type = format!("{}::events::MessageExecuted", self.gateway_pkg);
            let mut observations = Vec::with_capacity(indices.len());
            for &index in indices {
                let tx = &txs[index];
                let approved = self
                    .client
                    .has_message_approved(&approved_event_type, source_chain, &tx.message_id)
                    .await?;
                let executed = self
                    .client
                    .has_message_executed(&executed_event_type, source_chain, &tx.message_id)
                    .await?;
                let status = if executed {
                    DestinationStatus::Executed { command_id: None }
                } else if tx.is_phase(Phase::Approved) && approved {
                    DestinationStatus::Approved { command_id: None }
                } else {
                    DestinationStatus::Pending
                };
                observations.push(DestinationObservation { index, status });
            }
            Ok(observations)
        })
    }
}
