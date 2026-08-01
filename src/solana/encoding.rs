//! Pure derivations: ITS PDA seeds, the on-chain interchain-token-id
//! formula, ATA addresses, and the keypair file loader.
//!
//! Nothing in this module performs I/O beyond `dirs::home_dir` — it's all
//! byte-level constructions, parameterized by the target [`Network`]'s ITS
//! program ID. Keeping it isolated means the load-test and command paths can
//! call PDA helpers without pulling `RpcClient` into the dependency graph.

use eyre::Result;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, read_keypair_file};
use tokio::task::spawn_blocking;

use crate::types::Network;

const ITS_SEED: &[u8] = b"interchain-token-service";
const TOKEN_MANAGER_SEED: &[u8] = b"token-manager";
const INTERCHAIN_TOKEN_SEED: &[u8] = b"interchain-token";
const PREFIX_INTERCHAIN_TOKEN_SALT: &[u8] = b"interchain-token-salt";
const PREFIX_INTERCHAIN_TOKEN_ID: &[u8] = b"interchain-token-id";

pub fn clone_keypair(keypair: &Keypair) -> Keypair {
    let bytes = keypair.to_bytes();
    let mut seed = [0; 32];
    seed.copy_from_slice(&bytes[..32]);

    Keypair::new_from_array(seed)
}

pub fn find_its_root_pda(network: Network) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[ITS_SEED], &network.solana_its_id())
}

pub fn find_token_manager_pda(
    network: Network,
    its_root: &Pubkey,
    token_id: &[u8; 32],
) -> (Pubkey, u8) {
    Pubkey::find_program_address(
        &[TOKEN_MANAGER_SEED, its_root.as_ref(), token_id],
        &network.solana_its_id(),
    )
}

pub fn find_interchain_token_pda(
    network: Network,
    its_root: &Pubkey,
    token_id: &[u8],
) -> (Pubkey, u8) {
    Pubkey::find_program_address(
        &[INTERCHAIN_TOKEN_SEED, its_root.as_ref(), token_id],
        &network.solana_its_id(),
    )
}

pub(super) fn spl_associated_token_account_program_id() -> Pubkey {
    Pubkey::from_str_const("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL")
}

pub(super) fn mpl_token_metadata_program_id() -> Pubkey {
    Pubkey::from_str_const("metaqbxxUerdq28cj1RbAWkYQm3ybzjb6a8bt518x1s")
}

pub fn get_associated_token_address(
    wallet: &Pubkey,
    mint: &Pubkey,
    token_program: &Pubkey,
) -> Pubkey {
    Pubkey::find_program_address(
        &[wallet.as_ref(), token_program.as_ref(), mint.as_ref()],
        &spl_associated_token_account_program_id(),
    )
    .0
}

/// Derive the interchain token ID from deployer and salt.
pub fn interchain_token_id(network: Network, deployer: &Pubkey, salt: &[u8; 32]) -> [u8; 32] {
    let chain_name_hash = network.solana_its_chain_name_hash();
    let deploy_salt = solana_sdk::keccak::hashv(&[
        PREFIX_INTERCHAIN_TOKEN_SALT,
        &chain_name_hash,
        deployer.as_ref(),
        salt,
    ])
    .to_bytes();
    solana_sdk::keccak::hashv(&[PREFIX_INTERCHAIN_TOKEN_ID, &deploy_salt]).to_bytes()
}

/// Load a Solana keypair from a file path, or fall back to ~/.config/solana/id.json.
pub async fn load_keypair(path: Option<&str>) -> Result<Keypair> {
    let key_path = match path {
        Some(p) => p.to_string(),
        None => {
            let home =
                dirs::home_dir().ok_or_else(|| eyre::eyre!("cannot determine home directory"))?;
            home.join(".config/solana/id.json")
                .to_string_lossy()
                .into_owned()
        }
    };
    spawn_blocking(move || {
        read_keypair_file(&key_path)
            .map_err(|e| eyre::eyre!("failed to read Solana keypair from {key_path}: {e}"))
    })
    .await?
}
