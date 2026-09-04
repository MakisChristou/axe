use std::time::Duration;

use alloy::primitives::{Bytes, TxHash};
use alloy::providers::ProviderBuilder;
use alloy::rpc::{client::RpcClient, types::TransactionRequest};
use alloy::transports::mock::Asserter;

use super::*;

fn sender(asserter: Asserter, gas_budget: u64) -> PipelinedSender {
    PipelinedSender::new(
        EvmEndpoints {
            urls: vec!["http://127.0.0.1:1".to_owned()],
            providers: vec![
                ProviderBuilder::new()
                    .connect_client(RpcClient::mocked(asserter))
                    .erased(),
            ],
        },
        PrivateKeySigner::random(),
        PipelineLimits {
            native_reserve: U256::from(100),
            gas_budget: U256::from(gas_budget),
            receipt_timeout: Duration::ZERO,
        },
    )
}

fn transaction(sender: &PipelinedSender) -> PipelineTransaction {
    PipelineTransaction {
        request: TransactionRequest {
            chain_id: Some(1),
            ..Default::default()
        }
        .from(sender.signer.address())
        .to(Address::from([1; 20]))
        .value(U256::ZERO)
        .gas_limit(21_000)
        .gas_price(1),
        token: Address::from([2; 20]),
        amount: U256::from(10),
        deadline: Instant::now() + Duration::from_secs(60),
    }
}

fn balances(asserter: &Asserter) {
    asserter.push_success(&U256::from(1_000_000));
    asserter.push_success(&Bytes::copy_from_slice(
        &U256::from(1_000).to_be_bytes::<32>(),
    ));
}

#[tokio::test]
async fn sends_consecutive_nonces_without_waiting_for_any_receipts() {
    let asserter = Asserter::new();
    asserter.push_success(&"0x5");
    balances(&asserter);
    asserter.push_success(&TxHash::ZERO);
    balances(&asserter);
    asserter.push_success(&TxHash::ZERO);
    let sender = sender(asserter.clone(), 100_000);

    let first = sender.broadcast(transaction(&sender)).await.unwrap();
    assert_eq!(sender.state.lock().await.next, Some(6));
    let second = sender.broadcast(transaction(&sender)).await.unwrap();
    let state = sender.state.lock().await;
    assert_ne!(first.hash, second.hash);
    assert_eq!(state.next, Some(7));
    assert_eq!(state.gas_spent, U256::ZERO);
    assert_eq!(state.gas_reserved, U256::from(42_000));
    assert_eq!(sender.broadcasts.load(Ordering::Relaxed), 2);
    assert!(asserter.read_q().is_empty());
}

#[tokio::test]
async fn pending_deposits_reserve_gas_and_rejected_work_does_not_consume_a_nonce() {
    let asserter = Asserter::new();
    asserter.push_success(&"0x5");
    balances(&asserter);
    asserter.push_success(&TxHash::ZERO);
    balances(&asserter);
    let sender = sender(asserter.clone(), 40_000);
    let first = sender.broadcast(transaction(&sender)).await.unwrap();
    assert!(matches!(
        sender.broadcast(transaction(&sender)).await,
        Err(PipelineError::NotSent(_))
    ));
    let mut state = sender.state.lock().await;
    assert_eq!(state.next, Some(6));
    assert_eq!(state.gas_reserved, U256::from(21_000));
    state.release(&first, U256::from(10_000));
    assert_eq!(state.gas_reserved, U256::ZERO);
    assert_eq!(state.gas_spent, U256::from(10_000));
    assert_eq!(state.tokens_reserved[&first.token], U256::ZERO);
    assert!(!sender.stopped());
    assert!(asserter.read_q().is_empty());
}

#[tokio::test]
async fn an_uncertain_broadcast_stops_the_chain_and_retains_its_reservations() {
    let asserter = Asserter::new();
    asserter.push_success(&"0x5");
    balances(&asserter);
    asserter.push_failure_msg("execution reverted");
    asserter.push_success(&Option::<u64>::None);
    let sender = sender(asserter.clone(), 100_000);
    assert!(matches!(
        sender.broadcast(transaction(&sender)).await,
        Err(PipelineError::Uncertain(_))
    ));
    assert!(sender.stopped());
    assert!(matches!(
        sender.broadcast(transaction(&sender)).await,
        Err(PipelineError::NotSent(_))
    ));
    let state = sender.state.lock().await;
    assert_eq!(state.gas_reserved, U256::from(21_000));
    assert_eq!(state.next, Some(5));
    assert!(asserter.read_q().is_empty());
}

#[test]
fn pending_inputs_and_native_reserve_cannot_be_double_spent() {
    let mut state = NonceState::default();
    let pending = PendingDeposit {
        hash: TxHash::ZERO,
        gas_reserved: U256::from(20),
        token: Address::ZERO,
        amount: U256::from(10),
    };
    let limits = PipelineLimits {
        native_reserve: U256::from(100),
        gas_budget: U256::from(100),
        receipt_timeout: Duration::ZERO,
    };
    state.reserve(&pending);
    assert!(
        state
            .check_funding(
                &limits,
                U256::from(139),
                U256::from(100),
                Address::ZERO,
                U256::from(10),
                U256::from(20)
            )
            .is_err()
    );
    assert!(
        state
            .check_funding(
                &limits,
                U256::from(140),
                U256::from(19),
                Address::ZERO,
                U256::from(10),
                U256::from(20)
            )
            .is_err()
    );
    assert!(
        state
            .check_funding(
                &limits,
                U256::from(140),
                U256::from(20),
                Address::ZERO,
                U256::from(10),
                U256::from(20)
            )
            .is_ok()
    );
}
