//! Real fixtures -> decoder -> DecisionStage with live mainnet reads (no signing).
//! `ETH_HTTP_URL=https://... cargo test --test decision_live -- --ignored`

use std::collections::HashSet;
use std::time::{Duration, Instant};

use alloy::primitives::{Address, U256, address};
use alloy::rpc::types::Transaction;
use dex_copy_trade::decision::{DecisionStage, Rejection};
use dex_copy_trade::decoder::{DecodeOutcome, DecoderRegistry};
use dex_copy_trade::ethereum::chain_state::RpcChainReader;
use dex_copy_trade::ethereum::provider;
use dex_copy_trade::models::{Dex, TradeIntent};
use dex_copy_trade::risk::{RiskConfig, RiskEngine, RiskError};
use dex_copy_trade::storage::{DEDUP_TTL, InMemoryDedup};
use dex_copy_trade::strategy::copy_strategy::{FixedEth, FixedUsd};
use dex_copy_trade::strategy::{CopyStrategy, Sizer, StrategyError};
use dex_copy_trade::utils::chain_addresses;
use dex_copy_trade::watcher::{TxOrigin, WatchedTx};

const ETH: u128 = 1_000_000_000_000_000_000;
/// Exchange hot wallet with a large ETH balance; only its balances are read.
const RICH: Address = address!("28C6c06298d514Db089934071355E5743bf21d60");
const ALL: [Dex; 3] = [Dex::UniswapV2, Dex::UniswapV3, Dex::UniswapUniversalRouter];

fn intent(fixture: &str) -> TradeIntent {
    let path = format!(
        "{}/tests/fixtures/{fixture}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let mut tx: Transaction =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    // Treat as pending so reads use latest state (fixture blocks may be pruned on full nodes).
    tx.block_number = None;
    let item = WatchedTx {
        tx,
        origin: TxOrigin::Pending,
        detected_at: Instant::now(),
    };
    let addrs = chain_addresses(1).unwrap();
    match DecoderRegistry::new(&addrs, &ALL).process(&item) {
        DecodeOutcome::Trade(i) => *i,
        other => panic!("{fixture}: {other:?}"),
    }
}

fn stage(strategy: Box<dyn CopyStrategy>) -> DecisionStage<RpcChainReader, InMemoryDedup> {
    let url = std::env::var("ETH_HTTP_URL").expect("set ETH_HTTP_URL to a mainnet RPC");
    let addrs = chain_addresses(1).unwrap();
    let reader = RpcChainReader::new(provider::connect_http(&url).unwrap(), addrs.eth_usd_feed);
    let risk = RiskEngine::new(
        RiskConfig {
            chain_id: 1,
            supported_dexes: ALL.to_vec(),
            token_blacklist: HashSet::new(),
            max_trade_wei: U256::from(ETH / 2),
            max_gas_wei: 100_000_000_000,
            enforce_gas_cap: true,
            cooldown: Duration::from_secs(30),
        },
        InMemoryDedup::new(DEDUP_TTL),
    );
    DecisionStage::new(
        reader,
        Sizer::new(strategy, 10_000, U256::from(ETH / 2)),
        risk,
        RICH,
        addrs.weth,
    )
}

#[tokio::test]
#[ignore]
async fn fixed_eth_buy_approved_then_duplicate_and_cooldown() {
    let mut s = stage(Box::new(FixedEth {
        amount_wei: U256::from(ETH / 20),
    }));
    let buy = intent("v2_buy_eth_fot");
    let trade = s.decide(buy.clone()).await.expect("buy approved");
    assert_eq!(trade.amount_in, U256::from(ETH / 20));

    assert!(matches!(
        s.decide(buy).await,
        Err(Rejection::Risk(RiskError::Duplicate))
    ));

    // Same token bought by another source tx within the cooldown.
    let mut again = intent("v2_buy_eth_fot");
    again.source_tx_hash = alloy::primitives::B256::repeat_byte(1);
    assert!(matches!(
        s.decide(again).await,
        Err(Rejection::Risk(RiskError::Cooldown { .. }))
    ));
}

#[tokio::test]
#[ignore]
async fn sell_of_token_bot_does_not_hold_is_ignored() {
    let mut s = stage(Box::new(FixedEth {
        amount_wei: U256::from(ETH / 20),
    }));
    assert!(matches!(
        s.decide(intent("v2_sell_eth_fot")).await,
        Err(Rejection::Strategy(StrategyError::NothingToSell))
    ));
}

#[tokio::test]
#[ignore]
async fn fixed_usd_uses_live_chainlink_price() {
    let mut s = stage(Box::new(FixedUsd {
        amount_usd_micro: U256::from(100_000_000u64),
    }));
    let trade = s
        .decide(intent("ur_buy_v2_sweep"))
        .await
        .expect("buy approved");
    // $100 at any plausible ETH price ($500..$20k) is 0.005..0.2 ETH.
    assert!(
        trade.amount_in > U256::from(ETH / 200) && trade.amount_in < U256::from(ETH / 5),
        "amount {}",
        trade.amount_in
    );
}
