//! Axelar / cosmos integration. Wallet derivation, LCD REST + Tendermint RPC
//! queries, tx building and broadcast, and event parsing for the amplifier
//! pipeline. Submodules are layered:
//!
//! - [`signers`]: cosmos wallet derivation from a mnemonic.
//! - [`rpc`]: read-only LCD/Tendermint queries + axelar config readers.
//! - [`tx`]: tx building, simulation, sign-and-broadcast.
//! - [`events`]: parsing event attributes out of cosmos tx responses to
//!   discover proposal IDs, second-leg ITS metadata, and routing/approval
//!   state.

mod events;
mod rpc;
mod signers;
mod tx;

pub use events::{
    SecondLegInfo, check_cosmos_routed, check_hub_approved, discover_second_leg,
    extract_proposal_id,
};
pub use rpc::{
    CosmwasmQueryError, CosmwasmQueryPending, check_axelar_balance, fetch_verifier_set,
    lcd_cosmwasm_smart_query, lcd_cosmwasm_smart_query_typed, lcd_fetch_code_id,
    lcd_query_proposal, read_axelar_config, read_axelar_contract_field, read_axelar_rpc,
    rpc_block_info, rpc_block_time, rpc_tx_search,
};
pub use signers::derive_axelar_wallet;
pub use tx::{
    build_execute_msg_any, build_execute_msg_any_with_funds, build_submit_proposal_any,
    sign_and_broadcast_cosmos_tx,
};
