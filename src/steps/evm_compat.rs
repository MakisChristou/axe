use alloy::{
    consensus::Transaction as _,
    eips::BlockNumberOrTag,
    hex,
    network::TransactionBuilder,
    primitives::{Address, Bytes, FixedBytes, U256, keccak256},
    providers::{Provider, ProviderBuilder},
    rpc::types::{Filter, Log, TransactionRequest},
    signers::local::PrivateKeySigner,
    sol,
};
use eyre::Result;

use crate::commands::deploy::DeployContext;
use crate::ui;

// ABI bindings for the TestRpcCompatibility contract
sol! {
    #[sol(rpc)]
    contract TestRpcCompat {
        function getValue() public view returns (uint256);
        function updateValue(uint256 newValue) external;
        event ValueUpdated(uint256 indexed value);
    }
}

/// Probe value the test contract is updated to and read back. Any non-zero
/// number works — 42 is conventional and easy to spot in logs.
const COMPAT_TEST_VALUE: u64 = 42;

/// Creation bytecode for TestRpcCompatibility.sol (compiled with solc 0.8.9, london).
/// Source: axelar-cgp-solidity/contracts/test/TestRpcCompatibility.sol
/// The test contract is not included in the published npm package, so we embed it here.
const TEST_CONTRACT_BYTECODE: &str = "608060405234801561001057600080fd5b50610127806100206000396000f3fe6080604052348015600f57600080fd5b5060043610603c5760003560e01c80632096525514604157806326a6ae51146056578063573c0bd3146067575b600080fd5b60005460405190815260200160405180910390f35b6065606136600460d9565b6076565b005b6065607236600460d9565b60a9565b600181905560405181907f468963a1d9dd9327ac085bcd5fa80a5a43a35360584c14d49aa7d24d33acc40390600090a250565b600081815560405182917f4273d0736f60e0dedfe745e86718093d8ec8646ebd2a60cd60643eeced56581191a250565b60006020828403121560ea57600080fd5b503591905056fea2646970667358221220af9c6356d5b307b8d254eca52bc45a435d9ba3d002de69c1401d89b156517f7f64736f6c63430008090033";

// ---------------------------------------------------------------------------
// Check tracking
// ---------------------------------------------------------------------------

enum CheckOutcome {
    Pass(String),
    Fail(String),
    Warn(String),
}

struct Check {
    name: &'static str,
    critical: bool,
    outcome: CheckOutcome,
}

impl Check {
    fn pass(name: &'static str, critical: bool, detail: String) -> Self {
        Self {
            name,
            critical,
            outcome: CheckOutcome::Pass(detail),
        }
    }
    fn fail(name: &'static str, critical: bool, detail: String) -> Self {
        Self {
            name,
            critical,
            outcome: CheckOutcome::Fail(detail),
        }
    }
    fn warn(name: &'static str, detail: String) -> Self {
        Self {
            name,
            critical: false,
            outcome: CheckOutcome::Warn(detail),
        }
    }
}

fn print_checks(checks: &[Check]) {
    for c in checks {
        match &c.outcome {
            CheckOutcome::Pass(d) => ui::success(&format!("{} — {d}", c.name)),
            CheckOutcome::Fail(d) => ui::error(&format!("{} — {d}", c.name)),
            CheckOutcome::Warn(d) => ui::warn(&format!("{} — {d}", c.name)),
        }
    }
}

fn summarise(checks: &[Check]) -> Result<()> {
    let critical_total = checks.iter().filter(|c| c.critical).count();
    let critical_pass = checks
        .iter()
        .filter(|c| c.critical && matches!(c.outcome, CheckOutcome::Pass(_)))
        .count();
    let optional_total = checks.iter().filter(|c| !c.critical).count();
    let optional_warn = checks
        .iter()
        .filter(|c| !c.critical && matches!(c.outcome, CheckOutcome::Warn(_)))
        .count();

    println!();
    let summary = format!(
        "{critical_pass}/{critical_total} critical PASS, {optional_warn}/{optional_total} optional WARN"
    );
    if critical_pass == critical_total {
        ui::success(&format!("Summary: {summary}"));
    } else {
        ui::error(&format!("Summary: {summary}"));
    }

    let failures: Vec<&Check> = checks
        .iter()
        .filter(|c| c.critical && matches!(c.outcome, CheckOutcome::Fail(_)))
        .collect();
    if !failures.is_empty() {
        println!();
        ui::error("Failed checks:");
        for c in &failures {
            if let CheckOutcome::Fail(d) = &c.outcome {
                ui::error(&format!("  {} — {d}", c.name));
            }
        }
        return Err(eyre::eyre!(
            "EVM compatibility check failed: {} critical check(s) failed",
            failures.len()
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

pub async fn run(ctx: &DeployContext, private_key: &str) -> Result<()> {
    let signer: PrivateKeySigner = private_key.parse()?;
    let deployer_addr = signer.address();
    let provider = ProviderBuilder::new()
        .wallet(signer)
        .connect_http(ctx.rpc_url.parse()?);

    ui::kv("rpc", &ctx.rpc_url);
    ui::kv("deployer", &format!("{deployer_addr}"));

    let mut checks: Vec<Check> = Vec::new();

    println!();
    ui::info("Phase 1: Node health");
    let (phase1, block_number) = run_phase_1_health(&provider, deployer_addr).await;
    print_checks(&phase1);
    let phase1_failures = phase1
        .iter()
        .filter(|c| c.critical && matches!(c.outcome, CheckOutcome::Fail(_)))
        .count();
    checks.extend(phase1);
    if phase1_failures > 0 {
        ui::error(&format!(
            "{phase1_failures} critical check(s) failed in Phase 1 — skipping contract tests"
        ));
        return summarise(&checks);
    }

    println!();
    ui::info("Phase 2: Contract lifecycle");
    let (phase2, contract_addr, update_block_number, update_receipt_logs) =
        run_phase_2_contract(&provider).await?;
    print_checks(&phase2);
    checks.extend(phase2);

    if let (Some(addr), Some(block_num)) = (contract_addr, update_block_number) {
        println!();
        ui::info("Phase 3: Event logs");
        let phase3 = run_phase_3_events(&provider, addr, block_num, &update_receipt_logs).await;
        print_checks(&phase3);
        checks.extend(phase3);
    }

    println!();
    ui::info("Optional checks");
    let optional = run_optional_checks(&provider, block_number).await;
    print_checks(&optional);
    checks.extend(optional);

    summarise(&checks)
}

// ---------------------------------------------------------------------------
// Phase 1 — node health (no transaction required)
// ---------------------------------------------------------------------------

/// Runs the nine `eth_*` sanity checks. Returns the produced `Check` list
/// plus the latest block number (so the optional checks at the end can
/// validate parent-hash consistency without re-querying).
async fn run_chain_identity_checks<P: Provider>(provider: &P) -> (Vec<Check>, Option<u64>) {
    let mut checks = Vec::new();

    // 1. eth_chainId — `chainId` is not currently persisted into State, so
    // we only assert the RPC returns *some* id. (Reintroduce a comparison
    // by adding `expected_chain_id: Option<u64>` to State and reading it
    // here.)
    let expected_chain_id: Option<u64> = None;
    match provider.get_chain_id().await {
        Ok(id) => match expected_chain_id {
            Some(expected) if id == expected => {
                checks.push(Check::pass("eth_chainId", true, format!("{id}")));
            }
            Some(expected) => {
                checks.push(Check::fail(
                    "eth_chainId",
                    true,
                    format!("got {id}, expected {expected}"),
                ));
            }
            None => {
                checks.push(Check::pass(
                    "eth_chainId",
                    true,
                    format!("{id} (no expected value in state)"),
                ));
            }
        },
        Err(e) => checks.push(Check::fail("eth_chainId", true, format!("{e}"))),
    }

    // 2. eth_syncing
    match provider
        .raw_request::<_, serde_json::Value>("eth_syncing".into(), ())
        .await
    {
        Ok(serde_json::Value::Bool(false)) => {
            checks.push(Check::pass("eth_syncing", true, "synced".into()));
        }
        Ok(_) => checks.push(Check::fail(
            "eth_syncing",
            true,
            "node is still syncing".into(),
        )),
        Err(e) => checks.push(Check::fail("eth_syncing", true, format!("{e}"))),
    }

    // 3. eth_blockNumber
    let block_number = match provider.get_block_number().await {
        Ok(n) if n > 0 => {
            checks.push(Check::pass("eth_blockNumber", true, format!("{n}")));
            Some(n)
        }
        Ok(n) => {
            checks.push(Check::fail("eth_blockNumber", true, format!("got {n}")));
            None
        }
        Err(e) => {
            checks.push(Check::fail("eth_blockNumber", true, format!("{e}")));
            None
        }
    };

    (checks, block_number)
}

async fn run_block_health_checks<P: Provider>(provider: &P) -> Vec<Check> {
    let mut checks = Vec::new();

    // 4. eth_getBlockByNumber("latest") — accept up to 120s drift from wall clock
    match provider.get_block_by_number(BlockNumberOrTag::Latest).await {
        Ok(Some(block)) => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let drift = now.abs_diff(block.header.timestamp);
            if drift <= 120 {
                checks.push(Check::pass(
                    "eth_getBlockByNumber(latest)",
                    true,
                    format!("block {}, timestamp {drift}s ago", block.header.number),
                ));
            } else {
                checks.push(Check::fail(
                    "eth_getBlockByNumber(latest)",
                    true,
                    format!("block timestamp drift {drift}s (>120s) — chain may be stalled"),
                ));
            }
        }
        Ok(None) => checks.push(Check::fail(
            "eth_getBlockByNumber(latest)",
            true,
            "returned null".into(),
        )),
        Err(e) => checks.push(Check::fail(
            "eth_getBlockByNumber(latest)",
            true,
            format!("{e}"),
        )),
    }

    // 5. eth_getBlockByNumber("finalized")
    match provider
        .get_block_by_number(BlockNumberOrTag::Finalized)
        .await
    {
        Ok(Some(block)) => checks.push(Check::pass(
            "eth_getBlockByNumber(finalized)",
            true,
            format!("block {}", block.header.number),
        )),
        Ok(None) => checks.push(Check::fail(
            "eth_getBlockByNumber(finalized)",
            true,
            "returned null — chain may not support finalized tag".into(),
        )),
        Err(e) => checks.push(Check::fail(
            "eth_getBlockByNumber(finalized)",
            true,
            format!("{e}"),
        )),
    }

    checks
}

async fn run_account_health_checks<P: Provider>(
    provider: &P,
    deployer_addr: Address,
) -> Vec<Check> {
    let mut checks = Vec::new();

    // 6. eth_gasPrice
    match provider.get_gas_price().await {
        Ok(price) if price > 0 => {
            checks.push(Check::pass("eth_gasPrice", true, format!("{price} wei")));
        }
        Ok(price) => checks.push(Check::fail(
            "eth_gasPrice",
            true,
            format!("returned {price}"),
        )),
        Err(e) => checks.push(Check::fail("eth_gasPrice", true, format!("{e}"))),
    }

    // 7. eth_getBalance
    match provider.get_balance(deployer_addr).await {
        Ok(bal) => checks.push(Check::pass("eth_getBalance", true, format!("{bal} wei"))),
        Err(e) => checks.push(Check::fail("eth_getBalance", true, format!("{e}"))),
    }

    // 8. eth_getTransactionCount
    match provider.get_transaction_count(deployer_addr).await {
        Ok(count) => checks.push(Check::pass(
            "eth_getTransactionCount",
            true,
            format!("nonce {count}"),
        )),
        Err(e) => checks.push(Check::fail("eth_getTransactionCount", true, format!("{e}"))),
    }

    // 9. eth_feeHistory
    match provider
        .get_fee_history(5, BlockNumberOrTag::Latest, &[50.0])
        .await
    {
        Ok(fee_history) if !fee_history.base_fee_per_gas.is_empty() => {
            let rewards = fee_history
                .reward
                .as_ref()
                .map(|r| r.len().to_string())
                .unwrap_or("none".into());
            checks.push(Check::pass(
                "eth_feeHistory",
                true,
                format!(
                    "baseFee entries: {}, rewards: {rewards}",
                    fee_history.base_fee_per_gas.len()
                ),
            ));
        }
        Ok(_) => checks.push(Check::fail(
            "eth_feeHistory",
            true,
            "empty baseFeePerGas".into(),
        )),
        Err(e) => checks.push(Check::fail("eth_feeHistory", true, format!("{e}"))),
    }

    checks
}

async fn run_phase_1_health<P: Provider>(
    provider: &P,
    deployer_addr: Address,
) -> (Vec<Check>, Option<u64>) {
    let (mut checks, block_number) = run_chain_identity_checks(provider).await;
    checks.extend(run_block_health_checks(provider).await);
    checks.extend(run_account_health_checks(provider, deployer_addr).await);
    (checks, block_number)
}

// ---------------------------------------------------------------------------
// Phase 2 — contract lifecycle (deploy + state read/write + tx lookup)
// ---------------------------------------------------------------------------

/// Deploys the test contract and runs the seven `eth_*` checks that need a
/// live contract on-chain. Returns the produced checks plus the contract
/// address, the block of the `updateValue(42)` tx, and that tx's receipt
/// logs (so Phase 3 can compare `logIndex`es).
async fn run_phase_2_contract<P: Provider>(
    provider: &P,
) -> Result<(Vec<Check>, Option<Address>, Option<u64>, Vec<Log>)> {
    let mut checks: Vec<Check> = Vec::new();

    // 10. Deploy test contract
    let bytecode_raw = hex::decode(TEST_CONTRACT_BYTECODE)?;
    let tx = TransactionRequest::default().with_deploy_code(Bytes::from(bytecode_raw));
    let (contract_addr, _deploy_block) = deploy_test_contract(provider, tx, &mut checks).await?;

    let mut update_tx_hash = None;
    let mut update_block_number = None;
    let mut update_receipt_logs = Vec::new();

    if let Some(addr) = contract_addr {
        let contract = TestRpcCompat::new(addr, provider);
        check_get_code(provider, addr, &mut checks).await;
        check_get_value_zero(&contract, &mut checks).await;
        check_estimate_gas(provider, &contract, addr, &mut checks).await;
        let outcome = send_update_value(&contract, &mut checks).await;
        if let Some((hash, block, logs)) = outcome {
            update_tx_hash = Some(hash);
            update_block_number = Some(block);
            update_receipt_logs = logs;
        }
        check_get_value_42(&contract, &mut checks).await;
        if let Some(hash) = update_tx_hash {
            check_tx_by_hash(provider, hash, addr, &mut checks).await;
        }
    }

    Ok((
        checks,
        contract_addr,
        update_block_number,
        update_receipt_logs,
    ))
}

async fn deploy_test_contract<P: Provider>(
    provider: &P,
    tx: TransactionRequest,
    checks: &mut Vec<Check>,
) -> Result<(Option<Address>, Option<u64>)> {
    match provider.send_transaction(tx).await {
        Ok(pending) => match pending.get_receipt().await {
            Ok(receipt) if receipt.status() => {
                let addr = receipt
                    .contract_address
                    .ok_or_else(|| eyre::eyre!("no contract address in receipt"))?;
                checks.push(Check::pass("deploy test contract", true, format!("{addr}")));
                Ok((Some(addr), Some(receipt.block_number.unwrap_or(0))))
            }
            Ok(_) => {
                checks.push(Check::fail(
                    "deploy test contract",
                    true,
                    "tx reverted (status=0)".into(),
                ));
                Ok((None, None))
            }
            Err(e) => {
                checks.push(Check::fail(
                    "deploy test contract",
                    true,
                    format!("receipt error: {e}"),
                ));
                Ok((None, None))
            }
        },
        Err(e) => {
            checks.push(Check::fail(
                "deploy test contract",
                true,
                format!("send error: {e}"),
            ));
            Ok((None, None))
        }
    }
}

async fn check_get_code<P: Provider>(provider: &P, addr: Address, checks: &mut Vec<Check>) {
    match provider.get_code_at(addr).await {
        Ok(code) if !code.is_empty() => {
            checks.push(Check::pass(
                "eth_getCode",
                true,
                format!("{} bytes", code.len()),
            ));
        }
        Ok(_) => checks.push(Check::fail("eth_getCode", true, "empty bytecode".into())),
        Err(e) => checks.push(Check::fail("eth_getCode", true, format!("{e}"))),
    }
}

async fn check_get_value_zero<P: Provider>(
    contract: &TestRpcCompat::TestRpcCompatInstance<&P>,
    checks: &mut Vec<Check>,
) {
    match contract.getValue().call().await {
        Ok(val) if val == U256::ZERO => {
            checks.push(Check::pass("eth_call(getValue)", true, "returned 0".into()));
        }
        Ok(val) => checks.push(Check::fail(
            "eth_call(getValue)",
            true,
            format!("expected 0, got {val}"),
        )),
        Err(e) => checks.push(Check::fail("eth_call(getValue)", true, format!("{e}"))),
    }
}

async fn check_estimate_gas<P: Provider>(
    provider: &P,
    contract: &TestRpcCompat::TestRpcCompatInstance<&P>,
    addr: Address,
    checks: &mut Vec<Check>,
) {
    let update_calldata = contract.updateValue(U256::from(COMPAT_TEST_VALUE));
    let estimate_tx = TransactionRequest::default()
        .to(addr)
        .input(update_calldata.calldata().clone().into());
    match provider.estimate_gas(estimate_tx).await {
        Ok(gas) if gas > 0 && gas < 100_000 => {
            checks.push(Check::pass("eth_estimateGas", true, format!("{gas} gas")));
        }
        Ok(gas) => checks.push(Check::fail(
            "eth_estimateGas",
            true,
            format!("{gas} gas (unexpected range)"),
        )),
        Err(e) => checks.push(Check::fail("eth_estimateGas", true, format!("{e}"))),
    }
}

async fn send_update_value<P: Provider>(
    contract: &TestRpcCompat::TestRpcCompatInstance<&P>,
    checks: &mut Vec<Check>,
) -> Option<(alloy::primitives::TxHash, u64, Vec<Log>)> {
    match contract
        .updateValue(U256::from(COMPAT_TEST_VALUE))
        .send()
        .await
    {
        Ok(pending) => {
            let hash = *pending.tx_hash();
            match pending.get_receipt().await {
                Ok(receipt) if receipt.status() => {
                    checks.push(Check::pass(
                        "updateValue tx",
                        true,
                        format!("status 1, block {}", receipt.block_number.unwrap_or(0)),
                    ));
                    Some((
                        hash,
                        receipt.block_number.unwrap_or(0),
                        receipt.inner.logs().to_vec(),
                    ))
                }
                Ok(_) => {
                    checks.push(Check::fail(
                        "updateValue tx",
                        true,
                        "reverted (status=0)".into(),
                    ));
                    None
                }
                Err(e) => {
                    checks.push(Check::fail(
                        "updateValue tx",
                        true,
                        format!("receipt error: {e}"),
                    ));
                    None
                }
            }
        }
        Err(e) => {
            checks.push(Check::fail(
                "updateValue tx",
                true,
                format!("send error: {e}"),
            ));
            None
        }
    }
}

async fn check_get_value_42<P: Provider>(
    contract: &TestRpcCompat::TestRpcCompatInstance<&P>,
    checks: &mut Vec<Check>,
) {
    match contract.getValue().call().await {
        Ok(val) if val == U256::from(COMPAT_TEST_VALUE) => {
            checks.push(Check::pass(
                "eth_call(getValue=42)",
                true,
                "state update confirmed".into(),
            ));
        }
        Ok(val) => checks.push(Check::fail(
            "eth_call(getValue=42)",
            true,
            format!("expected 42, got {val}"),
        )),
        Err(e) => checks.push(Check::fail("eth_call(getValue=42)", true, format!("{e}"))),
    }
}

async fn check_tx_by_hash<P: Provider>(
    provider: &P,
    hash: alloy::primitives::TxHash,
    addr: Address,
    checks: &mut Vec<Check>,
) {
    match provider.get_transaction_by_hash(hash).await {
        Ok(Some(tx)) if tx.inner.to() == Some(addr) => {
            checks.push(Check::pass(
                "eth_getTransactionByHash",
                true,
                "tx found, correct to address".into(),
            ));
        }
        Ok(Some(tx)) => {
            checks.push(Check::fail(
                "eth_getTransactionByHash",
                true,
                format!("to mismatch: {:?} vs {addr}", tx.inner.to()),
            ));
        }
        Ok(None) => checks.push(Check::fail(
            "eth_getTransactionByHash",
            true,
            "returned null".into(),
        )),
        Err(e) => checks.push(Check::fail(
            "eth_getTransactionByHash",
            true,
            format!("{e}"),
        )),
    }
}

// ---------------------------------------------------------------------------
// Phase 3 — event logs (only runs if Phase 2 produced a contract)
// ---------------------------------------------------------------------------

async fn run_phase_3_events<P: Provider>(
    provider: &P,
    addr: Address,
    block_num: u64,
    update_receipt_logs: &[Log],
) -> Vec<Check> {
    let mut checks: Vec<Check> = Vec::new();
    let event_sig = keccak256(b"ValueUpdated(uint256)");

    // 17. eth_getLogs with topic + block filter
    let filter = Filter::new()
        .address(addr)
        .event_signature(event_sig)
        .from_block(block_num)
        .to_block(block_num);
    let mut get_logs_log_index = None;
    match provider.get_logs(&filter).await {
        Ok(logs) if logs.is_empty() => checks.push(Check::fail(
            "eth_getLogs(topic+block)",
            true,
            "no logs found".into(),
        )),
        Ok(logs) => {
            let found = logs.iter().any(|log| {
                log.topics().len() >= 2
                    && log.topics()[0] == event_sig
                    && log.topics()[1] == FixedBytes::<32>::from(U256::from(COMPAT_TEST_VALUE))
            });
            if found {
                get_logs_log_index = logs.first().and_then(|l| l.log_index);
                checks.push(Check::pass(
                    "eth_getLogs(topic+block)",
                    true,
                    format!("{} log(s) found", logs.len()),
                ));
            } else {
                checks.push(Check::fail(
                    "eth_getLogs(topic+block)",
                    true,
                    "ValueUpdated(42) not found in logs".into(),
                ));
            }
        }
        Err(e) => checks.push(Check::fail(
            "eth_getLogs(topic+block)",
            true,
            format!("{e}"),
        )),
    }

    // 18. eth_getLogs with address + block range
    let range_filter = Filter::new()
        .address(addr)
        .from_block(block_num)
        .to_block(block_num);
    match provider.get_logs(&range_filter).await {
        Ok(logs) => {
            let found = logs
                .iter()
                .any(|log| log.topics().first() == Some(&event_sig));
            if found {
                checks.push(Check::pass(
                    "eth_getLogs(addr+range)",
                    true,
                    format!("{} log(s) found", logs.len()),
                ));
            } else {
                checks.push(Check::fail(
                    "eth_getLogs(addr+range)",
                    true,
                    "ValueUpdated event not found".into(),
                ));
            }
        }
        Err(e) => checks.push(Check::fail("eth_getLogs(addr+range)", true, format!("{e}"))),
    }

    // 19. logIndex consistency between the receipt and `getLogs`
    let receipt_log_index = update_receipt_logs
        .iter()
        .find(|l| l.topics().first() == Some(&event_sig))
        .and_then(|l| l.log_index);
    match (receipt_log_index, get_logs_log_index) {
        (Some(r), Some(g)) if r == g => checks.push(Check::pass(
            "logIndex consistency",
            true,
            format!("index {r} matches"),
        )),
        (Some(r), Some(g)) => checks.push(Check::fail(
            "logIndex consistency",
            true,
            format!("receipt={r}, getLogs={g}"),
        )),
        _ => checks.push(Check::warn(
            "logIndex consistency",
            "could not compare (missing log index)".into(),
        )),
    }

    checks
}

// ---------------------------------------------------------------------------
// Optional checks (warn-only) — `safe` block tag + parent hash consistency
// ---------------------------------------------------------------------------

async fn run_optional_checks<P: Provider>(provider: &P, block_number: Option<u64>) -> Vec<Check> {
    let mut checks: Vec<Check> = Vec::new();

    // 20. eth_getBlockByNumber("safe")
    match provider.get_block_by_number(BlockNumberOrTag::Safe).await {
        Ok(Some(block)) => checks.push(Check::pass(
            "eth_getBlockByNumber(safe)",
            false,
            format!("block {}", block.header.number),
        )),
        Ok(None) => checks.push(Check::warn(
            "eth_getBlockByNumber(safe)",
            "returned null — safe tag may not be supported".into(),
        )),
        Err(_) => checks.push(Check::warn(
            "eth_getBlockByNumber(safe)",
            "not supported".into(),
        )),
    }

    // 21. Parent hash validation
    if let Some(bn) = block_number
        && bn >= 2
    {
        match provider
            .get_block_by_number(BlockNumberOrTag::Number(bn))
            .await
        {
            Ok(Some(block)) => {
                let parent_hash = block.header.parent_hash;
                match provider.get_block_by_hash(parent_hash).await {
                    Ok(Some(parent)) if parent.header.hash == parent_hash => {
                        checks.push(Check::pass(
                            "parent hash validation",
                            false,
                            "consistent".into(),
                        ));
                    }
                    Ok(Some(_)) => checks.push(Check::warn(
                        "parent hash validation",
                        "hash mismatch".into(),
                    )),
                    _ => checks.push(Check::warn(
                        "parent hash validation",
                        "could not fetch parent block".into(),
                    )),
                }
            }
            _ => checks.push(Check::warn(
                "parent hash validation",
                "could not fetch block".into(),
            )),
        }
    }

    checks
}
