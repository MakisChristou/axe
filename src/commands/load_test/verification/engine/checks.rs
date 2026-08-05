//! Destination checks used by the polling pipeline and
//! the orchestrators in this `verify` module.

#[path = "checks/evm.rs"]
mod evm;
#[path = "checks/solana.rs"]
mod solana;

pub(super) use evm::{
    check_evm_command_executed, check_evm_is_message_approved, check_evm_is_message_executed,
};
pub(super) use solana::{batch_check_solana_incoming_messages, check_solana_incoming_message};
