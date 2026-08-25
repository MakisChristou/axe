//! Solana ITS compute-unit (CU) measurement harness.
//!
//! The Solana analog of the Foundry EVM `GasHarness.t.sol`. It drives the
//! published Axelar ITS / gateway / gas-service programs under LiteSVM and
//! prints `compute_units_consumed` for the operations the fee-api prices, so we
//! can validate them against the observed mainnet numbers:
//!
//!   * source `interchain_transfer`  ~67k CU
//!   * source gateway `call_contract` (gmp)
//!   * destination `execute`         ~2.08M CU  (split across CPI stages)
//!   * gateway `approve_messages`    ~3.1M CU
//!
//! The programs are the real mainnet binaries dumped into `tests/testdata/*.so`,
//! so the CU reflects the deployed bytecode. Prerequisite ITS/gateway/gas state
//! (config PDAs, a deployed token, funded ATAs) is injected directly with
//! `set_account` rather than run through the init/deploy instructions, keeping
//! each measurement to the single instruction under test.
//!
//!   cargo test -p its-cu-harness -- --nocapture

use anchor_lang::{system_program, AccountSerialize, AnchorSerialize, Discriminator};
use anchor_spl::{
    associated_token::{get_associated_token_address_with_program_id, ID as ATA_PROGRAM_ID},
    token::spl_token::state::{Account as SplTokenAccount, AccountState, Mint as SplMint},
    token_2022::ID as TOKEN_2022_ID,
};
use litesvm::LiteSVM;
use solana_sdk::{
    account::Account,
    instruction::{AccountMeta, Instruction},
    message::Message,
    native_token::LAMPORTS_PER_SOL,
    program_option::COption,
    program_pack::Pack,
    pubkey::Pubkey,
    signature::Keypair,
    signer::Signer,
    transaction::Transaction,
};

use solana_axelar_gas_service::state::Treasury;
use solana_axelar_gateway::GatewayConfig;
use solana_axelar_its::{
    instructions::interchain_token::interchain_transfer::make_interchain_transfer_instruction,
    state::{FlowState, InterchainTokenService, TokenManager, Type},
    ID as ITS_ID,
};

const CU_LIMIT: u32 = 1_400_000; // max CU a single Solana transaction may request

// ── program binaries (real mainnet, dumped via `solana program dump`) ────────

fn testdata(name: &str) -> String {
    format!("{}/tests/testdata/{}", env!("CARGO_MANIFEST_DIR"), name)
}

fn fresh_svm() -> LiteSVM {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(ITS_ID, testdata("its.so"))
        .expect("load its.so");
    svm.add_program_from_file(solana_axelar_gateway::ID, testdata("gateway.so"))
        .expect("load gateway.so");
    svm.add_program_from_file(solana_axelar_gas_service::ID, testdata("gas_service.so"))
        .expect("load gas_service.so");
    svm
}

// ── account-data builders ────────────────────────────────────────────────────

/// Anchor borsh account: 8-byte discriminator + borsh body.
fn borsh_account<T: AccountSerialize>(state: &T) -> Vec<u8> {
    let mut data = Vec::new();
    state.try_serialize(&mut data).expect("serialize account");
    data
}

/// Anchor zero-copy (Pod) account: 8-byte discriminator + raw struct bytes.
fn zero_copy_account<T: bytemuck::Pod + Discriminator>(state: &T) -> Vec<u8> {
    let mut data = Vec::new();
    data.extend_from_slice(T::DISCRIMINATOR.as_ref());
    data.extend_from_slice(bytemuck::bytes_of(state));
    data
}

fn mint_data(mint_authority: Pubkey, supply: u64, decimals: u8) -> Vec<u8> {
    let mut buf = vec![0u8; SplMint::LEN];
    SplMint {
        mint_authority: COption::Some(mint_authority),
        supply,
        decimals,
        is_initialized: true,
        freeze_authority: COption::None,
    }
    .pack_into_slice(&mut buf);
    buf
}

fn token_account_data(mint: Pubkey, owner: Pubkey, amount: u64) -> Vec<u8> {
    let mut buf = vec![0u8; SplTokenAccount::LEN];
    SplTokenAccount {
        mint,
        owner,
        amount,
        delegate: COption::None,
        state: AccountState::Initialized,
        is_native: COption::None,
        delegated_amount: 0,
        close_authority: COption::None,
    }
    .pack_into_slice(&mut buf);
    buf
}

// ── set_account helpers ──────────────────────────────────────────────────────

struct Injector<'a> {
    svm: &'a mut LiteSVM,
}

impl Injector<'_> {
    fn put(&mut self, key: Pubkey, owner: Pubkey, data: Vec<u8>) {
        let lamports = self.svm.minimum_balance_for_rent_exemption(data.len()).max(1);
        self.put_with_lamports(key, owner, data, lamports);
    }

    fn put_with_lamports(&mut self, key: Pubkey, owner: Pubkey, data: Vec<u8>, lamports: u64) {
        self.svm
            .set_account(
                key,
                Account {
                    lamports,
                    data,
                    owner,
                    executable: false,
                    rent_epoch: 0,
                },
            )
            .expect("set_account");
    }
}

/// A wrapped-SOL (native mint) token account: `amount` tokens backed by
/// `amount + rent_reserve` lamports, with the native-reserve marker set.
fn wsol_token_account_data(owner: Pubkey, amount: u64, rent_reserve: u64) -> Vec<u8> {
    let mut buf = vec![0u8; SplTokenAccount::LEN];
    SplTokenAccount {
        mint: anchor_spl::token::spl_token::native_mint::ID,
        owner,
        amount,
        delegate: COption::None,
        state: AccountState::Initialized,
        is_native: COption::Some(rent_reserve),
        delegated_amount: 0,
        close_authority: COption::None,
    }
    .pack_into_slice(&mut buf);
    buf
}

/// Prepends a `SetComputeUnitLimit` instruction so the tx has headroom, then
/// sends it and returns the measured `compute_units_consumed`. The compute-budget
/// instruction itself is a fixed 150-CU native op (reported separately by callers).
fn measure(svm: &mut LiteSVM, ix: Instruction, signers: &[&Keypair]) -> u64 {
    let cu_ix = set_compute_unit_limit_ix(CU_LIMIT);
    let payer = signers[0].pubkey();
    let blockhash = svm.latest_blockhash();
    let msg = Message::new_with_blockhash(&[cu_ix, ix], Some(&payer), &blockhash);
    let mut tx = Transaction::new_unsigned(msg);
    tx.partial_sign(signers, blockhash);
    match svm.send_transaction(tx) {
        Ok(meta) => {
            if std::env::var("CU_LOGS").is_ok() {
                for line in &meta.logs {
                    eprintln!("  log: {line}");
                }
            }
            meta.compute_units_consumed
        }
        Err(failed) => {
            for line in &failed.meta.logs {
                eprintln!("  log: {line}");
            }
            panic!("transaction failed: {:?}", failed.err);
        }
    }
}

/// ComputeBudget `SetComputeUnitLimit` (instruction tag 2, u32 LE limit).
fn set_compute_unit_limit_ix(limit: u32) -> Instruction {
    let program_id: Pubkey = "ComputeBudget111111111111111111111111111111"
        .parse()
        .unwrap();
    let mut data = vec![2u8];
    data.extend_from_slice(&limit.to_le_bytes());
    Instruction {
        program_id,
        accounts: vec![],
        data,
    }
}

// ── measurements ─────────────────────────────────────────────────────────────

/// Source `interchain_transfer`: burns the native interchain token from the
/// caller and emits the outbound GMP message via the gateway `call_contract` +
/// gas-service `pay_gas` CPIs.
///
/// `data = None`  → an `itsTransfer`.
/// `data = Some`  → an `itsTransferWithCall` (same instruction; the payload also
///                  carries an arbitrary data blob that gets keccak-hashed).
///
/// Mainnet target: ~67k CU.
fn measure_source_transfer(data: Option<Vec<u8>>) -> u64 {
    let mut svm = fresh_svm();

    let payer = Keypair::new();
    let authority = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100 * LAMPORTS_PER_SOL).unwrap();
    svm.airdrop(&authority.pubkey(), LAMPORTS_PER_SOL).unwrap();

    let token_id = [7u8; 32];
    let amount = 1_000_000u64;

    // Build the instruction first; the returned accounts struct carries every
    // derived PDA/ATA we then have to inject.
    let (ix, accts) = make_interchain_transfer_instruction(
        token_id,
        amount,
        TOKEN_2022_ID,
        payer.pubkey(),
        authority.pubkey(),
        "ethereum".to_string(),
        vec![0x11u8; 20],
        5_000, // realistic gas_value so the gas-service pay_gas CPI does real work
        None,
        None,
        data,
    );

    let its_bump = InterchainTokenService::find_pda().1;
    let tm_bump = TokenManager::find_pda(token_id, accts.its_root_pda).1;
    let gw_bump = GatewayConfig::find_pda().1;
    let treasury_bump = Treasury::find_pda().1;

    let mut inj = Injector { svm: &mut svm };

    // Gateway root config (zero-copy) — read by the gateway during call_contract.
    let mut gw_cfg: GatewayConfig = bytemuck::Zeroable::zeroed();
    gw_cfg.bump = gw_bump;
    inj.put(
        accts.gateway_root_pda,
        solana_axelar_gateway::ID,
        zero_copy_account(&gw_cfg),
    );

    // Gas-service treasury (zero-copy).
    let mut treasury: Treasury = bytemuck::Zeroable::zeroed();
    treasury.bump = treasury_bump;
    inj.put(
        accts.gas_treasury,
        solana_axelar_gas_service::ID,
        zero_copy_account(&treasury),
    );

    // ITS root config — must be unpaused and trust the destination chain.
    let its_root = InterchainTokenService {
        its_hub_address: "axelar1itshub".to_string(),
        chain_name: "solana".to_string(),
        paused: false,
        trusted_chains: vec!["ethereum".to_string()],
        bump: its_bump,
    };
    inj.put(accts.its_root_pda, ITS_ID, borsh_account(&its_root));

    // Token manager for a native interchain token.
    let token_manager = TokenManager {
        ty: Type::NativeInterchainToken,
        token_id,
        token_address: accts.token_mint,
        associated_token_account: accts.token_manager_ata,
        flow_slot: FlowState {
            flow_limit: None,
            flow_in: 0,
            flow_out: 0,
            epoch: 0,
        },
        bump: tm_bump,
    };
    inj.put(accts.token_manager_pda, ITS_ID, borsh_account(&token_manager));

    // The Token-2022 mint (token manager is the mint authority, required for the
    // native/mint-burn manager type) and the two associated token accounts.
    inj.put(
        accts.token_mint,
        TOKEN_2022_ID,
        mint_data(accts.token_manager_pda, amount, 9),
    );
    inj.put(
        accts.authority_token_account,
        TOKEN_2022_ID,
        token_account_data(accts.token_mint, authority.pubkey(), amount),
    );
    inj.put(
        accts.token_manager_ata,
        TOKEN_2022_ID,
        token_account_data(accts.token_mint, accts.token_manager_pda, 0),
    );

    measure(&mut svm, ix, &[&payer, &authority])
}

#[test]
fn source_interchain_transfer_its_transfer() {
    let cu = measure_source_transfer(None);
    println!("\n=== source interchain_transfer (itsTransfer, data=None) ===");
    println!("compute_units_consumed = {cu}  (incl. 150 CU compute-budget ix)");
    println!("pure ITS instruction    = {}  (mainnet target ~67k)", cu - 150);
}

#[test]
fn source_interchain_transfer_its_transfer_with_call() {
    let cu = measure_source_transfer(Some(vec![0xab; 128]));
    println!("\n=== source interchain_transfer (itsTransferWithCall, data=Some(128B)) ===");
    println!("compute_units_consumed = {cu}  (incl. 150 CU compute-budget ix)");
    println!("pure ITS instruction    = {}  (mainnet target ~67k)", cu - 150);
}

/// Source gmp: a bare gateway `call_contract` from a wallet (the `gmp` operation
/// the fee-api prices for a plain contract call, distinct from an ITS transfer).
/// This is also the CPI the ITS makes on every outbound transfer.
#[test]
fn gmp_gateway_call_contract() {
    use anchor_lang::{InstructionData, ToAccountMetas};

    let mut svm = fresh_svm();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100 * LAMPORTS_PER_SOL).unwrap();

    let (gateway_root, gw_bump) = GatewayConfig::find_pda();
    let mut gw_cfg: GatewayConfig = bytemuck::Zeroable::zeroed();
    gw_cfg.bump = gw_bump;
    Injector { svm: &mut svm }.put(
        gateway_root,
        solana_axelar_gateway::ID,
        zero_copy_account(&gw_cfg),
    );

    let accounts = solana_axelar_gateway::accounts::CallContract {
        caller: payer.pubkey(),
        signing_pda: None, // direct wallet call, not a program
        gateway_root_pda: gateway_root,
        event_authority: solana_axelar_gateway::EVENT_AUTHORITY_AND_BUMP.0,
        program: solana_axelar_gateway::ID,
    };
    let mut metas = accounts.to_account_metas(None);
    metas[0].is_signer = true; // caller is a direct signer (UncheckedAccount in the struct)

    let data = solana_axelar_gateway::instruction::CallContract {
        destination_chain: "ethereum".to_string(),
        destination_contract_address: "0x1111111111111111111111111111111111111111".to_string(),
        payload: vec![0xcd; 128],
        signing_pda_bump: 0,
    }
    .data();
    let ix = Instruction {
        program_id: solana_axelar_gateway::ID,
        accounts: metas,
        data,
    };

    let cu = measure(&mut svm, ix, &[&payer]);
    println!("\n=== gmp gateway call_contract ===");
    println!("compute_units_consumed   = {cu}  (incl. 150 CU compute-budget ix)");
    println!("pure gateway instruction = {}", cu - 150);
}

/// Destination inbound `execute`: the ITS instruction the relayer submits once a
/// GMP message has been approved on the gateway. It CPIs `validate_message`
/// (marking the message executed) and then mints the interchain token to the
/// recipient's ATA (give-token).
///
/// Route B: rather than run the gateway's signature-verification/approval flow
/// (secp256k1 over the Axelar verifier set — the millions of CU the fee-api
/// prices separately), we inject an already-Approved `IncomingMessage` whose
/// stored hash matches `message.hash()`, so `validate_message` succeeds. This
/// isolates the ITS-side cost of `execute` itself.
#[test]
fn destination_execute_interchain_transfer() {
    use anchor_lang::{InstructionData, ToAccountMetas};
    use solana_axelar_std::hasher::LeafHash;

    let mut svm = fresh_svm();

    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100 * LAMPORTS_PER_SOL).unwrap();

    let token_id = [9u8; 32];
    let amount = 1_000_000u64;
    let source_chain = "ethereum";
    let its_hub_address = "axelar1itshub";
    let receiver = Pubkey::new_from_array([3u8; 32]); // fixed destination wallet (non-program)

    let (its_root, its_bump) = InterchainTokenService::find_pda();
    let (token_manager_pda, tm_bump) = TokenManager::find_pda(token_id, its_root);
    let token_mint = TokenManager::find_token_mint(token_id, its_root).0;
    let token_manager_ata =
        get_associated_token_address_with_program_id(&token_manager_pda, &token_mint, &TOKEN_2022_ID);
    let destination_ata =
        get_associated_token_address_with_program_id(&receiver, &token_mint, &TOKEN_2022_ID);
    let (gateway_root, gw_bump) = GatewayConfig::find_pda();

    // The inbound GMP payload: ReceiveFromHub → InterchainTransfer (borsh).
    let inner = solana_axelar_its::encoding::InterchainTransfer {
        token_id,
        source_address: b"0xSourceContract".to_vec(),
        destination_address: receiver.to_bytes().to_vec(),
        amount,
        data: None,
    };
    let hub = solana_axelar_its::encoding::HubMessage::ReceiveFromHub {
        source_chain: source_chain.to_string(),
        message: solana_axelar_its::encoding::Message::InterchainTransfer(inner),
    };
    let mut payload = Vec::new();
    hub.serialize(&mut payload).unwrap();
    let payload_hash = solana_keccak_hasher::hash(&payload).to_bytes();

    // The cross-chain message; its source must be the trusted ITS hub.
    let message = solana_axelar_gateway::Message {
        cc_id: solana_axelar_std::CrossChainId {
            chain: source_chain.to_string(),
            id: "0xtxhash-0".to_string(),
        },
        source_address: its_hub_address.to_string(),
        destination_chain: "solana".to_string(),
        destination_address: ITS_ID.to_string(),
        payload_hash,
    };
    let command_id = message.command_id();
    let message_hash = message.hash();

    let (incoming_message_pda, im_bump) =
        solana_axelar_gateway::IncomingMessage::find_pda(&command_id);
    let (signing_pda, signer_bump) =
        solana_axelar_gateway::ValidateMessageSigner::find_pda(&command_id, &ITS_ID);

    let mut inj = Injector { svm: &mut svm };

    // Gateway root config + an already-approved incoming message.
    let mut gw_cfg: GatewayConfig = bytemuck::Zeroable::zeroed();
    gw_cfg.bump = gw_bump;
    inj.put(gateway_root, solana_axelar_gateway::ID, zero_copy_account(&gw_cfg));

    let mut incoming: solana_axelar_gateway::IncomingMessage = bytemuck::Zeroable::zeroed();
    incoming.bump = im_bump;
    incoming.signing_pda_bump = signer_bump;
    incoming.status = solana_axelar_gateway::state::MessageStatus::approved();
    incoming.message_hash = message_hash;
    incoming.payload_hash = payload_hash;
    inj.put(
        incoming_message_pda,
        solana_axelar_gateway::ID,
        zero_copy_account(&incoming),
    );

    // ITS root: unpaused, hub address matches the message source, source chain trusted.
    let its_root_state = InterchainTokenService {
        its_hub_address: its_hub_address.to_string(),
        chain_name: "solana".to_string(),
        paused: false,
        trusted_chains: vec![source_chain.to_string()],
        bump: its_bump,
    };
    inj.put(its_root, ITS_ID, borsh_account(&its_root_state));

    // Token manager + mint (token manager is the mint authority for give-token) +
    // the token-manager and recipient ATAs (pre-created so give-token just mints).
    let token_manager = TokenManager {
        ty: Type::NativeInterchainToken,
        token_id,
        token_address: token_mint,
        associated_token_account: token_manager_ata,
        flow_slot: FlowState {
            flow_limit: None,
            flow_in: 0,
            flow_out: 0,
            epoch: 0,
        },
        bump: tm_bump,
    };
    inj.put(token_manager_pda, ITS_ID, borsh_account(&token_manager));
    inj.put(
        token_mint,
        TOKEN_2022_ID,
        mint_data(token_manager_pda, 0, 9),
    );
    inj.put(
        token_manager_ata,
        TOKEN_2022_ID,
        token_account_data(token_mint, token_manager_pda, 0),
    );
    inj.put(
        destination_ata,
        TOKEN_2022_ID,
        token_account_data(token_mint, receiver, 0),
    );

    // Build the execute instruction account list.
    // First the gateway "executable" accounts (validate_message CPI), then the
    // ITS-specific accounts, the #[event_cpi] pair, and finally the give-token
    // remaining accounts [destination, destination_token_authority, destination_ata].
    let mut metas = solana_axelar_gateway::executable::helpers::AxelarExecuteAccounts {
        incoming_message_pda,
        signing_pda,
        gateway_root_pda: gateway_root,
        event_authority: solana_axelar_gateway::EVENT_AUTHORITY_AND_BUMP.0,
        axelar_gateway_program: solana_axelar_gateway::ID,
    }
    .to_account_metas(None);

    metas.extend([
        AccountMeta::new(payer.pubkey(), true),
        AccountMeta::new_readonly(its_root, false),
        AccountMeta::new(token_manager_pda, false),
        AccountMeta::new(token_mint, false),
        AccountMeta::new(token_manager_ata, false),
        AccountMeta::new_readonly(TOKEN_2022_ID, false),
        AccountMeta::new_readonly(ATA_PROGRAM_ID, false),
        AccountMeta::new_readonly(system_program::ID, false),
        // #[event_cpi] pair (ITS)
        AccountMeta::new_readonly(solana_axelar_its::EVENT_AUTHORITY_AND_BUMP.0, false),
        AccountMeta::new_readonly(ITS_ID, false),
        // remaining accounts for give-token
        AccountMeta::new_readonly(receiver, false),
        AccountMeta::new_readonly(receiver, false), // destination_token_authority == destination (wallet)
        AccountMeta::new(destination_ata, false),
    ]);

    let data = solana_axelar_its::instruction::Execute {
        message,
        payload,
    }
    .data();
    let ix = Instruction {
        program_id: ITS_ID,
        accounts: metas,
        data,
    };

    let cu = measure(&mut svm, ix, &[&payer]);
    println!("\n=== destination execute (inbound interchain_transfer, give-token) ===");
    println!("compute_units_consumed = {cu}  (incl. 150 CU compute-budget ix)");
    println!(
        "pure ITS instruction    = {}  (Niko's 2.08M target is the gateway verification flow, not this)",
        cu - 150
    );
}

/// Source itsDeployment: the app's canonical-token deployment (see
/// `apps/contracts/scripts/deploy-its-solana.ts`) is two ITS instructions:
///
///   1. `register_canonical_interchain_token`  — local; creates the lock/unlock
///      token manager for an existing SPL mint. No GMP.
///   2. `deploy_remote_canonical_interchain_token` — the GMP-emitting one
///      (gateway `call_contract` + gas `pay_gas`); this is the source cost the
///      fee-api prices for a deployment.
///
/// Both read the token's name/symbol from its MPL metadata account. Measured
/// against the same mainnet ITS `.so` as the transfer flows.
#[test]
fn source_its_deployment_canonical() {
    use borsh::BorshSerialize;
    use solana_axelar_its::instructions::canonical_token::{
        deploy_remote_canonical_token::make_deploy_remote_canonical_token_instruction,
        register_canonical_token::make_register_canonical_token_instruction,
    };

    let mut svm = fresh_svm();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100 * LAMPORTS_PER_SOL).unwrap();

    // A pre-existing canonical SPL token (classic SPL Token program, like USDC).
    let token_program = anchor_spl::token::ID;
    let mint = Pubkey::new_from_array([5u8; 32]);
    let (its_root, its_bump) = InterchainTokenService::find_pda();
    let (gateway_root, gw_bump) = GatewayConfig::find_pda();
    let (gas_treasury, treasury_bump) = Treasury::find_pda();

    let mut inj = Injector { svm: &mut svm };

    inj.put(mint, token_program, mint_data(Pubkey::new_unique(), 1_000_000_000, 6));

    // MPL Metadata account for the mint (read, not CPI'd, by both instructions).
    let (metadata_pda, _) = mpl_token_metadata::accounts::Metadata::find_pda(&mint);
    let md = mpl_token_metadata::accounts::Metadata {
        key: mpl_token_metadata::types::Key::MetadataV1,
        update_authority: Pubkey::new_unique(),
        mint,
        name: "Canonical Token".to_string(),
        symbol: "CANON".to_string(),
        uri: String::new(),
        seller_fee_basis_points: 0,
        creators: None,
        primary_sale_happened: false,
        is_mutable: true,
        edition_nonce: None,
        token_standard: None,
        collection: None,
        uses: None,
        collection_details: None,
        programmable_config: None,
    };
    let mut md_data = Vec::new();
    md.serialize(&mut md_data).unwrap();
    inj.put(metadata_pda, mpl_token_metadata::ID, md_data);

    // ITS root (trusts the destination chain), gateway root, gas treasury.
    let its_root_state = InterchainTokenService {
        its_hub_address: "axelar1itshub".to_string(),
        chain_name: "solana".to_string(),
        paused: false,
        trusted_chains: vec!["ethereum".to_string()],
        bump: its_bump,
    };
    inj.put(its_root, ITS_ID, borsh_account(&its_root_state));
    let mut gw_cfg: GatewayConfig = bytemuck::Zeroable::zeroed();
    gw_cfg.bump = gw_bump;
    inj.put(gateway_root, solana_axelar_gateway::ID, zero_copy_account(&gw_cfg));
    let mut treasury: Treasury = bytemuck::Zeroable::zeroed();
    treasury.bump = treasury_bump;
    inj.put(gas_treasury, solana_axelar_gas_service::ID, zero_copy_account(&treasury));

    // 1. Local registration (creates the token manager).
    let (ix_reg, _) = make_register_canonical_token_instruction(payer.pubkey(), mint, token_program);
    let cu_reg = measure(&mut svm, ix_reg, &[&payer]);

    // 2. Remote deploy (emits the GMP message).
    let (ix_dep, _) =
        make_deploy_remote_canonical_token_instruction(payer.pubkey(), mint, "ethereum".to_string(), 5_000);
    let cu_dep = measure(&mut svm, ix_dep, &[&payer]);

    let reg = cu_reg - 150;
    let dep = cu_dep - 150;
    println!("\n=== source itsDeployment (canonical) ===");
    println!("register_canonical_interchain_token       = {reg}  (local, no GMP)");
    println!("deploy_remote_canonical_interchain_token  = {dep}  (GMP-emitting — the fee-api source cost)");
    println!("combined total                            = {}", reg + dep);
}

/// Destination itsTransferWithCall: the app's real WSOL "unwrap" flow. ITS
/// delivers WSOL to the `native-unwrapper` program (a lock/unlock canonical
/// token), then CPIs into its `execute_with_interchain_token`, which closes the
/// WSOL ATA into an escrow and splits the lamports (amount to the recipient, the
/// ATA rent to the rent treasury). Route B (injected Approved `IncomingMessage`)
/// as for the plain execute, but the destination is a program with a data
/// payload, so the full ITS-execute -> give-token -> program-CPI chain runs.
///
/// This is the measurement that replaces the conservative
/// `execution_compute_units.itsTransferWithCall` guess.
#[test]
fn destination_execute_with_call_unwrap() {
    use anchor_lang::{InstructionData, ToAccountMetas};
    use solana_axelar_gateway::payload::{AxelarMessagePayload, EncodingScheme};
    use solana_axelar_std::hasher::LeafHash;

    let nu_id: Pubkey = "unw1CzbeMFnmPH4fAYfNqCCZwBsWYPEGLeDtmaRsXEq".parse().unwrap();
    let rent_treasury: Pubkey = "unwsnr2WXUFJFVf1cue2cxPmHty7is6GwT6EpDWuqML".parse().unwrap();
    let native_mint = anchor_spl::token::spl_token::native_mint::ID;
    let token_program = anchor_spl::token::ID;

    let mut svm = fresh_svm();
    svm.add_program_from_file(nu_id, testdata("native_unwrapper.so"))
        .expect("load native_unwrapper.so");

    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100 * LAMPORTS_PER_SOL).unwrap();

    let token_id = [8u8; 32];
    // Above the ATA rent reserve (~2.04M lamports) so the flow only succeeds if
    // give-token actually delivered the WSOL (a no-op transfer would underflow
    // the unwrapper's amount/rent split).
    let amount = 5_000_000u64;
    let source_chain = "ethereum";
    let its_hub_address = "axelar1itshub";
    let recipient = Pubkey::new_from_array([4u8; 32]);

    let (its_root, its_bump) = InterchainTokenService::find_pda();
    let (token_manager_pda, tm_bump) = TokenManager::find_pda(token_id, its_root);
    let token_manager_ata =
        get_associated_token_address_with_program_id(&token_manager_pda, &native_mint, &token_program);
    let (dta, _) = Pubkey::find_program_address(
        &[solana_axelar_its::seed_prefixes::ITS_TOKEN_AUTHORITY_SEED],
        &nu_id,
    );
    let destination_ata =
        get_associated_token_address_with_program_id(&dta, &native_mint, &token_program);
    let (interchain_transfer_execute, _) =
        Pubkey::find_program_address(&[b"interchain-transfer-execute", nu_id.as_ref()], &ITS_ID);
    let (escrow, _) = Pubkey::find_program_address(&[b"escrow"], &nu_id);
    let (gateway_root, gw_bump) = GatewayConfig::find_pda();

    // The unwrapper's custom accounts (forwarded by ITS): recipient, rent
    // treasury, escrow. These must match both the payload's encoded accounts and
    // the top-level remaining accounts exactly.
    let custom = [
        solana_sdk::instruction::AccountMeta::new(recipient, false),
        solana_sdk::instruction::AccountMeta::new(rent_treasury, false),
        solana_sdk::instruction::AccountMeta::new(escrow, false),
    ];
    let recipient_bytes = recipient.to_bytes();
    let inner_payload = AxelarMessagePayload::new(&recipient_bytes, &custom, EncodingScheme::Borsh);
    let its_data = inner_payload.encode().unwrap();

    // ReceiveFromHub -> InterchainTransfer, destination = the unwrapper program.
    let inner = solana_axelar_its::encoding::InterchainTransfer {
        token_id,
        source_address: b"0xSourceContract".to_vec(),
        destination_address: nu_id.to_bytes().to_vec(),
        amount,
        data: Some(its_data),
    };
    let hub = solana_axelar_its::encoding::HubMessage::ReceiveFromHub {
        source_chain: source_chain.to_string(),
        message: solana_axelar_its::encoding::Message::InterchainTransfer(inner),
    };
    let mut payload = Vec::new();
    hub.serialize(&mut payload).unwrap();
    let payload_hash = solana_keccak_hasher::hash(&payload).to_bytes();

    let message = solana_axelar_gateway::Message {
        cc_id: solana_axelar_std::CrossChainId {
            chain: source_chain.to_string(),
            id: "0xtxhash-unwrap".to_string(),
        },
        source_address: its_hub_address.to_string(),
        destination_chain: "solana".to_string(),
        destination_address: ITS_ID.to_string(),
        payload_hash,
    };
    let command_id = message.command_id();
    let message_hash = message.hash();
    let (incoming_message_pda, im_bump) =
        solana_axelar_gateway::IncomingMessage::find_pda(&command_id);
    let (signing_pda, signer_bump) =
        solana_axelar_gateway::ValidateMessageSigner::find_pda(&command_id, &ITS_ID);

    let reserve = svm.minimum_balance_for_rent_exemption(SplTokenAccount::LEN);
    let mut inj = Injector { svm: &mut svm };

    // Gateway root + approved incoming message.
    let mut gw_cfg: GatewayConfig = bytemuck::Zeroable::zeroed();
    gw_cfg.bump = gw_bump;
    inj.put(gateway_root, solana_axelar_gateway::ID, zero_copy_account(&gw_cfg));
    let mut incoming: solana_axelar_gateway::IncomingMessage = bytemuck::Zeroable::zeroed();
    incoming.bump = im_bump;
    incoming.signing_pda_bump = signer_bump;
    incoming.status = solana_axelar_gateway::state::MessageStatus::approved();
    incoming.message_hash = message_hash;
    incoming.payload_hash = payload_hash;
    inj.put(incoming_message_pda, solana_axelar_gateway::ID, zero_copy_account(&incoming));

    // ITS root + a lock/unlock token manager holding the WSOL to be delivered.
    let its_root_state = InterchainTokenService {
        its_hub_address: its_hub_address.to_string(),
        chain_name: "solana".to_string(),
        paused: false,
        trusted_chains: vec![source_chain.to_string()],
        bump: its_bump,
    };
    inj.put(its_root, ITS_ID, borsh_account(&its_root_state));
    let token_manager = TokenManager {
        ty: Type::LockUnlock,
        token_id,
        token_address: native_mint,
        associated_token_account: token_manager_ata,
        flow_slot: FlowState { flow_limit: None, flow_in: 0, flow_out: 0, epoch: 0 },
        bump: tm_bump,
    };
    inj.put(token_manager_pda, ITS_ID, borsh_account(&token_manager));

    // WSOL mint (classic SPL, no mint authority) + the token-manager's WSOL vault
    // (holds `amount`) + the empty destination ATA give-token transfers into.
    let mut wsol_mint = vec![0u8; SplMint::LEN];
    SplMint {
        mint_authority: COption::None,
        supply: amount,
        decimals: 9,
        is_initialized: true,
        freeze_authority: COption::None,
    }
    .pack_into_slice(&mut wsol_mint);
    inj.put(native_mint, token_program, wsol_mint);
    inj.put_with_lamports(
        token_manager_ata,
        token_program,
        wsol_token_account_data(token_manager_pda, amount, reserve),
        reserve + amount,
    );
    inj.put_with_lamports(
        destination_ata,
        token_program,
        wsol_token_account_data(dta, 0, reserve),
        reserve,
    );

    // Program-owned escrow (empty Escrow account: 8-byte discriminator).
    let escrow_disc = [31u8, 213, 123, 187, 186, 22, 218, 155];
    inj.put(escrow, nu_id, escrow_disc.to_vec());

    // Build the execute instruction: executable accounts + ITS accounts + the
    // #[event_cpi] pair + remaining accounts for give-token-to-program.
    let mut metas = solana_axelar_gateway::executable::helpers::AxelarExecuteAccounts {
        incoming_message_pda,
        signing_pda,
        gateway_root_pda: gateway_root,
        event_authority: solana_axelar_gateway::EVENT_AUTHORITY_AND_BUMP.0,
        axelar_gateway_program: solana_axelar_gateway::ID,
    }
    .to_account_metas(None);
    metas.extend([
        AccountMeta::new(payer.pubkey(), true),
        AccountMeta::new_readonly(its_root, false),
        AccountMeta::new(token_manager_pda, false),
        AccountMeta::new(native_mint, false),
        AccountMeta::new(token_manager_ata, false),
        AccountMeta::new_readonly(token_program, false),
        AccountMeta::new_readonly(ATA_PROGRAM_ID, false),
        AccountMeta::new_readonly(system_program::ID, false),
        AccountMeta::new_readonly(solana_axelar_its::EVENT_AUTHORITY_AND_BUMP.0, false),
        AccountMeta::new_readonly(ITS_ID, false),
        // remaining: give-token-to-program
        AccountMeta::new_readonly(nu_id, false),
        AccountMeta::new_readonly(dta, false),
        AccountMeta::new(destination_ata, false),
        AccountMeta::new_readonly(interchain_transfer_execute, false),
        AccountMeta::new(recipient, false),
        AccountMeta::new(rent_treasury, false),
        AccountMeta::new(escrow, false),
    ]);

    let data = solana_axelar_its::instruction::Execute { message, payload }.data();
    let ix = Instruction { program_id: ITS_ID, accounts: metas, data };

    let cu = measure(&mut svm, ix, &[&payer]);
    println!("\n=== destination itsTransferWithCall (WSOL unwrap via native-unwrapper) ===");
    println!("compute_units_consumed = {cu}  (incl. 150 CU compute-budget ix)");
    println!("full destination execute = {}  (ITS execute + give-token + unwrapper CPI)", cu - 150);
}
