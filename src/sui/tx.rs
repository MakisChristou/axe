//! Programmable Transaction Block (PTB) construction, BCS encoding, intent
//! framing, and the `sign_and_submit` convenience that ties [`PtbBuilder`]
//! together with [`super::rpc::SuiClient`] and [`super::wallet::SuiWallet`].

use eyre::{Result, eyre};
use serde_json::Value;
use sui_sdk_types::{
    Address as SuiAddress, Argument, Command, GasPayment, Identifier, Input, MoveCall,
    ObjectReference, ProgrammableTransaction, SharedInput, SplitCoins, Transaction,
    TransactionExpiration, TransactionKind, TypeTag,
};

use super::rpc::SuiClient;
use super::wallet::SuiWallet;

/// Sui's intent scope for a TransactionData payload: [scope=0, version=0, app_id=0].
const TX_INTENT: [u8; 3] = [0, 0, 0];

#[derive(Debug, Clone)]
pub struct SubmittedTx {
    pub digest: String,
    pub success: bool,
    pub error: Option<String>,
    /// Move events are module-defined JSON-RPC payloads. Callers deserialize
    /// the event type they expect at the protocol boundary.
    pub events: Vec<Value>,
}

/// Build a Move-call PTB that invokes `package::module::function(args...)`,
/// with optional `splitGas: Some(amount)` to split off `amount` mist from the
/// gas coin and pass it as one of the args.
///
/// Returns the fully-formed `Transaction` (unsigned), ready to BCS-serialize
/// and sign.
pub struct PtbBuilder {
    inputs: Vec<Input>,
    commands: Vec<Command>,
}

impl Default for PtbBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl PtbBuilder {
    pub fn new() -> Self {
        Self {
            inputs: Vec::new(),
            commands: Vec::new(),
        }
    }

    /// Add a pure (BCS-serialized primitive) input. Returns its `Argument`.
    pub fn pure_bytes(&mut self, bytes: Vec<u8>) -> Argument {
        let idx = self.inputs.len() as u16;
        self.inputs.push(Input::Pure(bytes));
        Argument::Input(idx)
    }

    pub fn pure_u64(&mut self, n: u64) -> Result<Argument> {
        let bytes = bcs::to_bytes(&n).map_err(|e| eyre!("bcs u64: {e}"))?;
        Ok(self.pure_bytes(bytes))
    }

    pub fn pure_address(&mut self, addr: SuiAddress) -> Result<Argument> {
        let bytes = bcs::to_bytes(&addr).map_err(|e| eyre!("bcs address: {e}"))?;
        Ok(self.pure_bytes(bytes))
    }

    pub fn pure_vec_u8(&mut self, v: &[u8]) -> Result<Argument> {
        let bytes = bcs::to_bytes(&v.to_vec()).map_err(|e| eyre!("bcs vec<u8>: {e}"))?;
        Ok(self.pure_bytes(bytes))
    }

    pub fn pure_string(&mut self, s: &str) -> Result<Argument> {
        let bytes = bcs::to_bytes(&s.to_string()).map_err(|e| eyre!("bcs string: {e}"))?;
        Ok(self.pure_bytes(bytes))
    }

    /// Add a shared-object input.
    pub fn shared_object(
        &mut self,
        object_id: SuiAddress,
        initial_shared_version: u64,
        mutable: bool,
    ) -> Argument {
        let idx = self.inputs.len() as u16;
        self.inputs.push(Input::Shared(SharedInput::new(
            object_id,
            initial_shared_version,
            mutable,
        )));
        Argument::Input(idx)
    }

    /// Add an owned- (or immutable-) object input. Used when a Move call needs
    /// a `Coin<T>` (or similar) object the sender owns at a specific version.
    pub fn owned_object(&mut self, obj_ref: ObjectReference) -> Argument {
        let idx = self.inputs.len() as u16;
        self.inputs.push(Input::ImmutableOrOwned(obj_ref));
        Argument::Input(idx)
    }

    /// Consume the builder into a `TransactionKind::ProgrammableTransaction`,
    /// for callers that need to BCS-encode it directly (e.g. dev-inspect).
    pub fn into_transaction_kind(self) -> TransactionKind {
        TransactionKind::ProgrammableTransaction(ProgrammableTransaction {
            inputs: self.inputs,
            commands: self.commands,
        })
    }

    pub fn split_coin(&mut self, coin: Argument, amount: Argument) -> Argument {
        let cmd_idx = self.commands.len() as u16;
        self.commands.push(Command::SplitCoins(SplitCoins {
            coin,
            amounts: vec![amount],
        }));
        // SplitCoins returns a Vec<Coin<T>>; the first split is NestedResult(cmd, 0).
        Argument::NestedResult(cmd_idx, 0)
    }

    pub fn move_call(
        &mut self,
        package: SuiAddress,
        module: &str,
        function: &str,
        type_arguments: Vec<TypeTag>,
        arguments: Vec<Argument>,
    ) -> Result<Argument> {
        let cmd_idx = self.commands.len() as u16;
        let module_id = Identifier::new(module).map_err(|e| eyre!("module ident: {e:?}"))?;
        let function_id = Identifier::new(function).map_err(|e| eyre!("function ident: {e:?}"))?;
        self.commands.push(Command::MoveCall(MoveCall {
            package,
            module: module_id,
            function: function_id,
            type_arguments,
            arguments,
        }));
        Ok(Argument::Result(cmd_idx))
    }

    pub fn build(self, sender: SuiAddress, gas: GasPayment) -> Transaction {
        Transaction {
            kind: TransactionKind::ProgrammableTransaction(ProgrammableTransaction {
                inputs: self.inputs,
                commands: self.commands,
            }),
            sender,
            gas_payment: gas,
            expiration: TransactionExpiration::None,
        }
    }
}

/// BCS-encode a `Transaction` for signing/submitting.
pub fn bcs_encode_transaction(tx: &Transaction) -> Result<Vec<u8>> {
    bcs::to_bytes(tx).map_err(|e| eyre!("bcs encode tx: {e}"))
}

/// Build the intent message for a `Transaction`: `[0,0,0] || bcs(tx)`.
pub fn intent_message_for(tx_bcs: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(3 + tx_bcs.len());
    buf.extend_from_slice(&TX_INTENT);
    buf.extend_from_slice(tx_bcs);
    buf
}

/// Sign + submit a built `Transaction`. The wallet must own `gas_payment.objects`.
pub async fn sign_and_submit(
    client: &SuiClient,
    wallet: &SuiWallet,
    tx: Transaction,
) -> Result<SubmittedTx> {
    let tx_bcs = bcs_encode_transaction(&tx)?;
    let intent = intent_message_for(&tx_bcs);
    let sig = wallet.serialized_intent_signature(&intent);
    client.execute_transaction(&tx_bcs, &[sig]).await
}
