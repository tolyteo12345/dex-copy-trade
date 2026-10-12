//! Executor against live mainnet state via eth_simulateV1 (nothing is signed or sent).
//! `ETH_HTTP_URL=https://... cargo test --test executor_live -- --ignored`

use std::time::Instant;

use alloy::primitives::{Address, Bytes, U256, address};
use alloy::rpc::types::Transaction;
use dex_copy_trade::eth::decoder::{DecodeOutcome, DecoderRegistry};
use dex_copy_trade::eth::ethereum::chain_state::RpcChainReader;
use dex_copy_trade::eth::ethereum::provider;
use dex_copy_trade::eth::execution::ExecutionError;
use dex_copy_trade::eth::execution::executor::{ExecutionPlan, Executor, ExecutorConfig};
use dex_copy_trade::eth::execution::simulator::RpcSimulator;
use dex_copy_trade::eth::models::{
    CopyTrade, Dex, PoolKind, RouteLeg, TradeIntent, TradeSide, V4Hop, V4Path,
};
use dex_copy_trade::eth::utils::chain_addresses;
use dex_copy_trade::eth::watcher::{TxOrigin, WatchedTx};

const ETH: u128 = 1_000_000_000_000_000_000;
/// Exchange hot wallet holding ETH and USDT; used only as the simulated sender.
const RICH: Address = address!("28C6c06298d514Db089934071355E5743bf21d60");
const USDT: Address = address!("dAC17F958D2ee523a2206206994597C13D831ec7");
const USDC: Address = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");

fn intent(fixture: &str) -> TradeIntent {
    let path = format!(
        "{}/tests/fixtures/eth/{fixture}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let tx: Transaction = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let item = WatchedTx {
        tx,
        origin: TxOrigin::Pending,
        detected_at: Instant::now(),
    };
    let addrs = chain_addresses(1).unwrap();
    let all = [
        Dex::UniswapV2,
        Dex::UniswapV3,
        Dex::UniswapUniversalRouter,
        Dex::UniswapV4,
    ];
    match DecoderRegistry::new(&addrs, &all).process(&item) {
        // Fixture txs are mined: plan against latest state, without the source tx.
        DecodeOutcome::Trade(i) => TradeIntent {
            source_block: Some(1),
            ..*i
        },
        other => panic!("{fixture}: {other:?}"),
    }
}

fn executor() -> Executor<RpcChainReader, RpcSimulator> {
    let url = std::env::var("ETH_HTTP_URL").expect("set ETH_HTTP_URL to a mainnet RPC");
    let http = provider::connect_http(&url).unwrap();
    let addrs = chain_addresses(1).unwrap();
    Executor::new(
        RpcChainReader::new(http.clone(), addrs.eth_usd_feed),
        RpcSimulator::new(http),
        &addrs,
        ExecutorConfig {
            bot: RICH,
            max_slippage_bps: 300,
            max_trade_wei: U256::from(ETH / 2),
        },
    )
}

async fn plan(intent: TradeIntent, amount_in: U256) -> Result<ExecutionPlan, ExecutionError> {
    let trade = CopyTrade {
        intent,
        amount_in,
        capped: false,
        decision_at: Instant::now(),
    };
    executor().plan(trade).await
}

fn assert_sane(p: &ExecutionPlan) {
    assert!(p.expected_out > U256::ZERO);
    assert!(p.min_out < p.expected_out);
    assert!(p.price_impact_bps <= 300, "impact {}", p.price_impact_bps);
    let swap = p.txs.last().unwrap();
    assert_eq!(swap.step, "swap");
    assert!(
        swap.gas_used > 50_000 && swap.gas_used < 500_000,
        "gas {}",
        swap.gas_used
    );
}

#[tokio::test]
#[ignore]
async fn v3_buy_usdt_with_eth() {
    let i = intent("v3_router02_multicall_buy_eth");
    assert_eq!(i.token_out, USDT);
    let p = plan(i, U256::from(ETH / 20)).await.unwrap();
    assert_sane(&p);
    assert_eq!(p.route.pool, PoolKind::UniswapV3);
    assert_eq!(p.txs.len(), 1, "no approval needed for ETH input");
    // 0.05 ETH is worth between $25 and $1000 of USDT (6 decimals).
    assert!(
        p.expected_out > U256::from(25_000_000u64) && p.expected_out < U256::from(1_000_000_000u64)
    );
}

#[tokio::test]
#[ignore]
async fn v2_buy_usdc_with_eth() {
    let mut i = intent("v2_buy_eth_fot");
    i.token_out = USDC;
    i.route[0].tokens = vec![i.token_in, USDC];
    let p = plan(i, U256::from(ETH / 20)).await.unwrap();
    assert_sane(&p);
    assert_eq!(p.route.pool, PoolKind::UniswapV2);
}

#[tokio::test]
#[ignore]
async fn v3_sell_usdt_for_eth_with_approval() {
    let i = intent("v3_router02_multicall_sell_unwrap");
    assert_eq!((i.side, i.token_in), (TradeSide::Sell, USDT));
    let p = plan(i, U256::from(60_000_000u64)).await.unwrap(); // 60 USDT
    assert_sane(&p);
    let steps: Vec<_> = p.txs.iter().map(|t| t.step).collect();
    assert!(steps.ends_with(&["approve", "swap"]), "{steps:?}");
}

fn v4_route(currency_in: Address, currency_out: Address) -> RouteLeg {
    let norm = |a: Address| {
        if a.is_zero() {
            chain_addresses(1).unwrap().weth
        } else {
            a
        }
    };
    RouteLeg {
        pool: PoolKind::UniswapV4,
        tokens: vec![norm(currency_in), norm(currency_out)],
        fees: vec![500],
        v4: Some(V4Path {
            currency_in,
            hops: vec![V4Hop {
                currency_out,
                fee: 500,
                tick_spacing: 10,
                hooks: Address::ZERO,
                hook_data: Bytes::new(),
            }],
        }),
    }
}

/// Any decoded fixture intent, re-pointed at the ETH/USDC V4 pool (fee 0.05%).
fn v4_usdc_intent(side: TradeSide) -> TradeIntent {
    let mut i = intent("v4_buy_native_single");
    let weth = chain_addresses(1).unwrap().weth;
    i.side = side;
    (i.token_in, i.token_out, i.route) = match side {
        TradeSide::Buy => (weth, USDC, vec![v4_route(Address::ZERO, USDC)]),
        TradeSide::Sell => (USDC, weth, vec![v4_route(USDC, Address::ZERO)]),
    };
    i
}

#[tokio::test]
#[ignore]
async fn v4_buy_usdc_with_native_eth() {
    let p = plan(v4_usdc_intent(TradeSide::Buy), U256::from(ETH / 20))
        .await
        .unwrap();
    assert_sane(&p);
    assert_eq!(p.route.pool, PoolKind::UniswapV4);
    assert_eq!(p.txs.len(), 1);
    assert!(p.expected_out > U256::from(25_000_000u64));
}

#[tokio::test]
#[ignore]
async fn v4_sell_usdc_for_native_eth_via_permit2() {
    let p = plan(v4_usdc_intent(TradeSide::Sell), U256::from(60_000_000u64))
        .await
        .unwrap();
    assert_sane(&p);
    let steps: Vec<_> = p.txs.iter().map(|t| t.step).collect();
    assert_eq!(steps.last(), Some(&"swap"));
    assert!(
        steps.contains(&"permit2_approve"),
        "router needs a Permit2 allowance: {steps:?}"
    );
    // 60 USDC is worth well above 0.005 ETH at any plausible price.
    assert!(p.expected_out > U256::from(ETH / 200));
}
