//! Real signing + eth_sendRawTransaction against mainnet with a freshly generated, unfunded
//! key: the node must decode and verify the tx, then reject it for insufficient funds.
//! Nothing can be spent. `ETH_HTTP_URL=https://... cargo test --test broadcast_live -- --ignored`

use std::time::{Instant, SystemTime, UNIX_EPOCH};

use alloy::primitives::{U256, keccak256};
use alloy::rpc::types::{Transaction, TransactionRequest};
use dex_copy_trade::eth::config::GasCapMode;
use dex_copy_trade::eth::decoder::{DecodeOutcome, DecoderRegistry};
use dex_copy_trade::eth::ethereum::nonce::NonceManager;
use dex_copy_trade::eth::ethereum::provider;
use dex_copy_trade::eth::ethereum::signer::BotSigner;
use dex_copy_trade::eth::execution::broadcast::{BroadcastError, Broadcaster, RpcSender};
use dex_copy_trade::eth::execution::executor::{ExecutionPlan, PlannedTx};
use dex_copy_trade::eth::execution::gas::{CappedEip1559, GasStrategy, RpcFeeSource};
use dex_copy_trade::eth::models::{CopyTrade, Dex};
use dex_copy_trade::eth::utils::chain_addresses;
use dex_copy_trade::eth::watcher::{TxOrigin, WatchedTx};

fn fresh_signer() -> BotSigner {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let key = keccak256(format!("broadcast-live-{nanos}-{}", std::process::id()));
    BotSigner::from_hex(&key.to_string()).unwrap()
}

#[tokio::test]
#[ignore]
async fn node_accepts_encoding_and_rejects_for_funds() {
    let url = std::env::var("ETH_HTTP_URL").expect("set ETH_HTTP_URL to a mainnet RPC");
    let http = provider::connect_http(&url).unwrap();
    let signer = fresh_signer();
    let me = signer.address();

    let gas = CappedEip1559::new(
        RpcFeeSource::new(http.clone()),
        200_000_000_000,
        GasCapMode::Skip,
    );
    let fees = gas.fees().await.expect("live fees under 200 gwei");
    assert!(fees.max_fee_per_gas >= fees.max_priority_fee_per_gas);

    // Any decoded intent works as context; only `txs` is sent: 0-value self transfer.
    let path = format!(
        "{}/tests/fixtures/eth/v2_buy_eth_fot.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let tx: Transaction = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let item = WatchedTx {
        tx,
        origin: TxOrigin::Pending,
        detected_at: Instant::now(),
    };
    let addrs = chain_addresses(1).unwrap();
    let DecodeOutcome::Trade(intent) =
        DecoderRegistry::new(&addrs, &[Dex::UniswapV2]).process(&item)
    else {
        panic!("fixture should decode");
    };
    let route = intent.route[0].clone();
    let now = Instant::now();
    let plan = ExecutionPlan {
        trade: CopyTrade {
            intent: *intent,
            amount_in: U256::ZERO,
            capped: false,
            decision_at: now,
        },
        route,
        amount_in: U256::ZERO,
        sell_capped: false,
        quoted_out: U256::ZERO,
        expected_out: U256::ZERO,
        transfer_tax_bps: 0,
        min_out: U256::ZERO,
        price_impact_bps: 0,
        after_source: false,
        txs: vec![PlannedTx {
            step: "swap",
            request: TransactionRequest::default().to(me).value(U256::ZERO),
            gas_used: 21_000,
            peak_gas: 21_000,
        }],
        simulated_at: now,
    };

    let b = Broadcaster::new(RpcSender::new(http), signer, NonceManager::new(), 1);
    let err = b
        .send(&plan, fees)
        .await
        .expect_err("unfunded account must be rejected");
    let BroadcastError::Rejected {
        step: "swap",
        accepted: 0,
        error,
    } = err
    else {
        panic!("expected node rejection, got {err:?}");
    };
    let msg = error.to_string().to_lowercase();
    assert!(msg.contains("insufficient funds"), "node said: {msg}");
}
