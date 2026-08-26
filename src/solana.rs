//! Solana-side primitives: gateway/ITS instruction builders, the manual
//! destination-side approval flow, and the byte-level encoders + balance
//! preflight that don't fit neatly into either bucket.
//!
//! Submodules:
//! - [`balance`]: per-flow lamport thresholds + the `check_solana_balance`
//!   preflight printed before any send.
//! - [`rpc`]: shared `RpcClient` constructor (always finalized commitment)
//!   and the `fetch_confirmed_tx` retry loop tuned for public devnet RPC's
//!   eventual-consistency window.
//! - [`encoding`]: pure derivations — ITS PDA seeds, the on-chain
//!   `interchain_token_id` formula, ATA derivation, and `load_keypair`.
//! - [`gateway`]: the source-side `call_contract` send path, the
//!   destination-side manual approval flow (init session → verify
//!   signatures → approve → execute), and the `CallContractEvent` parser.
//! - [`its`]: ITS instruction builders — `deploy_interchain_token`,
//!   `deploy_remote_interchain_token`, `interchain_transfer`.
//!
//! All public surfaces are re-exported from this `mod.rs` so existing
//! `crate::solana::*` imports keep compiling unchanged.

mod balance;
mod encoding;
mod gateway;
mod its;
mod programs;
mod rpc;

pub use balance::{
    MIN_SOL_ITS_LAMPORTS, MIN_SOL_RELAY_LAMPORTS, MIN_SOL_SEND_LAMPORTS, check_solana_balance,
};
pub use encoding::{
    clone_keypair, find_interchain_token_pda, find_its_root_pda, get_associated_token_address,
    interchain_token_id, load_keypair,
};
pub use gateway::pay_gas_lamports;
pub use gateway::{
    approve_messages_on_gateway, decode_execute_data, execute_on_memo,
    extract_gateway_call_contract_payload, extract_its_message_id, send_call_contract,
    solana_call_contract_index,
};
pub use its::{
    DeployInterchainTokenRequest, InterchainTransferRequest, send_its_deploy_interchain_token,
    send_its_deploy_remote_interchain_token, send_its_interchain_transfer,
};
pub use rpc::rpc_client;
pub(crate) use rpc::sign_send_confirm;

#[cfg(test)]
use solana_sdk::pubkey::Pubkey;

#[cfg(test)]
mod tests {
    use super::Pubkey;

    #[test]
    fn derive_testnet_gateway_pdas() {
        // Testnet Solana gateway
        let gateway_id: Pubkey = "gtwJ8LWDRWZpbvCqp8sDeTgy3GSyuoEsiaKC8wSXJqq"
            .parse()
            .unwrap();

        // GatewayConfig PDA
        let (config_pda, _) = Pubkey::find_program_address(&[b"gateway"], &gateway_id);
        println!("GatewayConfig PDA: {config_pda}");
        assert_eq!(
            config_pda.to_string(),
            "8mnEaWDXqbpDwyiGLR1T8DTc8AHuk2Fs6Pf4fRDv97WY"
        );

        // VerifierSetTracker PDA for the on-chain verifier set
        let onchain_hash =
            hex::decode("7b8163c3123a65f351c1d5b1e94c44841e731ea57b51f55479207380cab933c5")
                .unwrap();
        let (tracker_pda, _) =
            Pubkey::find_program_address(&[b"ver-set-tracker", &onchain_hash], &gateway_id);
        println!("VerifierSetTracker PDA (on-chain):  {tracker_pda}");
        assert_eq!(
            tracker_pda.to_string(),
            "F1PVLJQSGxBr28QWsRJTaTJiua7yKZQ5r97KG154uZum"
        );

        // VerifierSetTracker PDA for the MultisigProver's current set
        let prover_hash =
            hex::decode("046c15e70bf840b19ef2e727bbfe6fae18155077342b2aa41d766a2f6db32cb1")
                .unwrap();
        let (tracker_pda2, _) =
            Pubkey::find_program_address(&[b"ver-set-tracker", &prover_hash], &gateway_id);
        println!("VerifierSetTracker PDA (prover):    {tracker_pda2}");

        // These should be DIFFERENT — confirming the mismatch
        assert_ne!(tracker_pda, tracker_pda2);
        println!("\nVerifier set mismatch confirmed!");
        println!("Gateway knows:      7b8163c3...");
        println!("MultisigProver uses: 046c15e7...");
        println!("rotate_signers needed on the Solana gateway");
    }
}

#[test]
fn derive_devnet_gateway_pdas() {
    let gateway_id: Pubkey = "gtwT4uGVTYSPnTGv6rSpMheyFyczUicxVWKqdtxNGw9"
        .parse()
        .unwrap();

    let (config_pda, _) = Pubkey::find_program_address(&[b"gateway"], &gateway_id);
    println!("=== DEVNET-AMPLIFIER ===");
    println!("GatewayConfig PDA: {config_pda}");

    // MultisigProver verifier set: caa238976160fcea5d5e5f4f3ea2ce0bed9106847e2d6d939de746c890c1faed
    let prover_hash =
        hex::decode("caa238976160fcea5d5e5f4f3ea2ce0bed9106847e2d6d939de746c890c1faed").unwrap();
    let (tracker_pda, _) =
        Pubkey::find_program_address(&[b"ver-set-tracker", &prover_hash], &gateway_id);
    println!("VerifierSetTracker PDA (prover set): {tracker_pda}");
    println!("Check on-chain: solana account {tracker_pda} --url https://api.devnet.solana.com");
}

#[test]
fn derive_stagenet_gateway_pdas() {
    let gateway_id: Pubkey = "gtwYHfHHipAoj8Hfp3cGr3vhZ8f3UtptGCQLqjBkaSZ"
        .parse()
        .unwrap();

    let (config_pda, _) = Pubkey::find_program_address(&[b"gateway"], &gateway_id);
    println!("=== STAGENET ===");
    println!("GatewayConfig PDA: {config_pda}");

    // MultisigProver verifier set: 315ad3ca3e873b65dbc5dd4a446a62018ea368b5d9f29232fa090875fdaa50b8
    let prover_hash =
        hex::decode("315ad3ca3e873b65dbc5dd4a446a62018ea368b5d9f29232fa090875fdaa50b8").unwrap();
    let (tracker_pda, _) =
        Pubkey::find_program_address(&[b"ver-set-tracker", &prover_hash], &gateway_id);
    println!("VerifierSetTracker PDA (prover set): {tracker_pda}");
    println!("Check on-chain: solana account {tracker_pda} --url https://api.testnet.solana.com");
}
