//! Cosmos observations for verification polling.

use eyre::Result;
use futures::future::try_join_all;
use serde::Deserialize;
use serde::de::IgnoredAny;
use serde_json::json;

use super::{ContractQueryObservation, observe_contract_query};
use crate::cosmos::{CosmwasmQueryPending, lcd_cosmwasm_smart_query};

/// Max messages per Cosmos LCD batch query. The query is base64-encoded in
/// the URL, so each message adds ~500 chars. Ten stays below common URL limits.
const COSMOS_BATCH_SIZE: usize = 10;

type PresenceList = Vec<Option<IgnoredAny>>;

#[derive(Deserialize)]
struct VotingStatus {
    status: String,
}

pub(in crate::commands::load_test::verify) async fn check_cosmos_routed(
    lcd: &str,
    cosm_gateway: &str,
    source_chain: &str,
    message_id: &str,
) -> Result<bool> {
    let query = json!({
        "outgoing_messages": [{
            "source_chain": source_chain,
            "message_id": message_id,
        }]
    });

    let response = match observe_contract_query(
        lcd,
        cosm_gateway,
        &query,
        CosmwasmQueryPending::OutgoingMessage,
    )
    .await?
    {
        ContractQueryObservation::Ready(response) => response,
        ContractQueryObservation::Pending => return Ok(false),
    };
    let items: PresenceList = response;
    Ok(items.into_iter().any(|item| item.is_some()))
}

pub(in crate::commands::load_test::verify) async fn check_hub_approved(
    lcd: &str,
    axelarnet_gateway: &str,
    source_chain: &str,
    message_id: &str,
) -> Result<bool> {
    let query = json!({
        "executable_messages": {
            "cc_ids": [{
                "source_chain": source_chain,
                "message_id": message_id,
            }]
        }
    });

    let response = match observe_contract_query(
        lcd,
        axelarnet_gateway,
        &query,
        CosmwasmQueryPending::ExecutableMessage,
    )
    .await?
    {
        ContractQueryObservation::Ready(response) => response,
        ContractQueryObservation::Pending => return Ok(false),
    };
    let items: PresenceList = response;
    Ok(items.into_iter().any(|item| item.is_some()))
}

pub(super) async fn batch_check_voting_verifier_owned(
    lcd: &str,
    voting_verifier: &str,
    source_chain: &str,
    destination_chain: &str,
    destination_address: &str,
    transactions: &[(usize, String, String, String)],
) -> Result<Vec<(usize, bool)>> {
    let futures: Vec<_> = transactions
        .chunks(COSMOS_BATCH_SIZE)
        .map(|chunk| async move {
            let messages: Vec<_> = chunk
                .iter()
                .map(|(_, message_id, source_address, payload_hash)| {
                    json!({
                        "cc_id": {
                            "source_chain": source_chain,
                            "message_id": message_id
                        },
                        "source_address": source_address,
                        "destination_chain": destination_chain,
                        "destination_address": destination_address,
                        "payload_hash": payload_hash,
                    })
                })
                .collect::<Vec<_>>();
            let query = json!({ "messages_status": messages });
            let response = lcd_cosmwasm_smart_query(lcd, voting_verifier, &query).await?;
            let items: Vec<Option<VotingStatus>> = serde_json::from_value(response)
                .map_err(|error| eyre::eyre!("invalid VotingVerifier response: {error}"))?;
            if items.len() != chunk.len() {
                return Err(eyre::eyre!(
                    "VotingVerifier messages_status returned {} items for {} messages",
                    items.len(),
                    chunk.len()
                ));
            }

            Ok(items
                .iter()
                .zip(chunk)
                .map(|(item, (index, ..))| {
                    (
                        *index,
                        item.as_ref()
                            .is_some_and(|item| item.status.to_lowercase().contains("succeeded")),
                    )
                })
                .collect::<Vec<_>>())
        })
        .collect::<Vec<_>>();
    Ok(try_join_all(futures).await?.into_iter().flatten().collect())
}

pub(super) async fn batch_check_cosmos_routed_owned(
    lcd: &str,
    cosm_gateway: &str,
    source_chain: &str,
    transactions: &[(usize, String)],
) -> Result<Vec<(usize, bool)>> {
    let futures: Vec<_> = transactions
        .chunks(COSMOS_BATCH_SIZE)
        .map(|chunk| async move {
            let cc_ids: Vec<_> = chunk
                .iter()
                .map(|(_, message_id)| {
                    json!({ "source_chain": source_chain, "message_id": message_id })
                })
                .collect();
            let query = json!({ "outgoing_messages": cc_ids });
            let response = match observe_contract_query(
                lcd,
                cosm_gateway,
                &query,
                CosmwasmQueryPending::OutgoingMessage,
            )
            .await?
            {
                ContractQueryObservation::Ready(response) => response,
                ContractQueryObservation::Pending => {
                    return Ok::<_, eyre::Report>(
                        chunk
                            .iter()
                            .map(|(index, _)| (*index, false))
                            .collect::<Vec<_>>(),
                    );
                }
            };
            let items: PresenceList = response;
            if items.len() != chunk.len() {
                return Err(eyre::eyre!(
                    "Gateway outgoing_messages returned {} items for {} messages",
                    items.len(),
                    chunk.len()
                ));
            }
            Ok(items
                .iter()
                .zip(chunk)
                .map(|(item, (index, _))| (*index, item.is_some()))
                .collect())
        })
        .collect();
    Ok(try_join_all(futures).await?.into_iter().flatten().collect())
}

pub(super) async fn batch_check_hub_approved_owned(
    lcd: &str,
    axelarnet_gateway: &str,
    source_chain: &str,
    transactions: &[(usize, String)],
) -> Result<Vec<(usize, bool)>> {
    let futures: Vec<_> = transactions
        .chunks(COSMOS_BATCH_SIZE)
        .map(|chunk| async move {
            let cc_ids: Vec<_> = chunk
                .iter()
                .map(|(_, message_id)| {
                    json!({ "source_chain": source_chain, "message_id": message_id })
                })
                .collect();
            let query = json!({ "executable_messages": { "cc_ids": cc_ids } });
            let response = match observe_contract_query(
                lcd,
                axelarnet_gateway,
                &query,
                CosmwasmQueryPending::ExecutableMessage,
            )
            .await?
            {
                ContractQueryObservation::Ready(response) => response,
                ContractQueryObservation::Pending => {
                    return Ok::<_, eyre::Report>(
                        chunk
                            .iter()
                            .map(|(index, _)| (*index, false))
                            .collect::<Vec<_>>(),
                    );
                }
            };
            let items: PresenceList = response;
            if items.len() != chunk.len() {
                return Err(eyre::eyre!(
                    "AxelarnetGateway executable_messages returned {} items for {} messages",
                    items.len(),
                    chunk.len()
                ));
            }
            Ok(items
                .iter()
                .zip(chunk)
                .map(|(item, (index, _))| (*index, item.is_some()))
                .collect())
        })
        .collect();
    Ok(try_join_all(futures).await?.into_iter().flatten().collect())
}
