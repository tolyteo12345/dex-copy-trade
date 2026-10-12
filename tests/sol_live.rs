//! Live mainnet checks (read + simulate only, never sends). Run with:
//! `SOL_RPC_URL=https://solana-rpc.publicnode.com cargo test --test sol_live -- --ignored --nocapture`
//!
//! Picks a wallet that just bought on pump (so it holds SOL and the token), runs the real
//! decoder + DecisionStage for it, then simulates the swap instructions the bot would build
//! with that wallet as `user`. `sigVerify` is off in simulation, so no key is needed.
#![cfg(feature = "sol")]

use std::time::Duration;

use serde_json::{Value, json};
use solana_message::{VersionedMessage, v0};
use solana_pubkey::Pubkey;
use solana_signature::Signature;
use solana_transaction::versioned::VersionedTransaction;

use dex_copy_trade::common::dedup::{DEDUP_TTL, TtlSet};
use dex_copy_trade::sol::config::SolStrategy;
use dex_copy_trade::sol::decision::{DecisionConfig, DecisionStage};
use dex_copy_trade::sol::decoder::{SolDecoder, source_tx_from_rpc_json};
use dex_copy_trade::sol::models::{SolCopyTrade, SolTradeIntent, TradeSide, VenueKind};
use dex_copy_trade::sol::pump::{self, PUMP_AMM_PROGRAM, PUMP_PROGRAM, TOKEN_PROGRAM, WSOL_MINT};
use dex_copy_trade::sol::rpc::{HttpRpc, SolRpc};

fn rpc_url() -> String {
    std::env::var("SOL_RPC_URL").unwrap_or_else(|_| "https://solana-rpc.publicnode.com".into())
}

async fn call(method: &str, params: Value) -> Value {
    let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
    let v: Value = reqwest::Client::new()
        .post(rpc_url())
        .json(&body)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    v["result"].clone()
}

/// Most recent successful BUY on `program` by a wallet that signed it itself.
async fn recent_buy(program: &Pubkey) -> SolTradeIntent {
    let sigs = call(
        "getSignaturesForAddress",
        json!([program.to_string(), { "limit": 200, "commitment": "confirmed" }]),
    )
    .await;
    for s in sigs.as_array().unwrap() {
        if !s["err"].is_null() {
            continue;
        }
        let raw = call(
            "getTransaction",
            json!([s["signature"], { "encoding": "json", "maxSupportedTransactionVersion": 1, "commitment": "confirmed" }]),
        )
        .await;
        let Some(tx) = source_tx_from_rpc_json(&raw) else {
            continue;
        };
        if let Ok(i) = SolDecoder::new([tx.account_keys[0]]).decode(&tx) {
            if i.side == TradeSide::Buy {
                return i;
            }
        }
    }
    panic!("no recent buy found on {program}");
}

async fn decide(intent: SolTradeIntent) -> SolCopyTrade {
    let cfg = DecisionConfig {
        bot: intent.source_wallet,
        strategy: SolStrategy::FixedSol {
            lamports: 1_000_000,
        },
        sell_ratio_bps: 100,
        max_trade_lamports: 1_000_000,
        fee_reserve_lamports: 0,
        venues: vec![VenueKind::PumpFun, VenueKind::PumpSwap],
        token_blacklist: Default::default(),
        cooldown: Duration::ZERO,
    };
    let mut stage = DecisionStage::new(
        HttpRpc::new(&rpc_url()).unwrap(),
        cfg,
        TtlSet::new(DEDUP_TTL),
    );
    stage
        .decide(intent)
        .await
        .expect("decision approves the copy")
}

/// Simulates the bot's swap with `trade.intent.source_wallet` as user; returns the amount
/// received as the executor would measure it.
async fn simulate(trade: &SolCopyTrade, side: TradeSide, amount_in: u64) -> u64 {
    let rpc = HttpRpc::new(&rpc_url()).unwrap();
    let user = trade.intent.source_wallet;
    let mut ixs = vec![
        pump::compute_unit_limit(400_000),
        pump::compute_unit_price(1),
    ];
    ixs.extend(pump::swap_instructions(
        &user,
        side,
        &trade.market,
        amount_in,
        1,
    ));
    let blockhash = rpc.latest_blockhash().await.unwrap();
    let msg = v0::Message::try_compile(&user, &ixs, &[], blockhash).unwrap();
    let tx = VersionedTransaction {
        signatures: vec![Signature::default()],
        message: VersionedMessage::V0(msg),
    };
    let watch = [
        user,
        pump::ata(&user, &WSOL_MINT, &TOKEN_PROGRAM),
        pump::ata(&user, &trade.market.mint, &trade.market.base_token_program),
    ];
    let sim = rpc.simulate(&tx, &watch).await.unwrap();
    assert!(
        sim.err.is_none(),
        "{side:?} simulation failed: {:?}\n{}",
        sim.err,
        sim.logs.join("\n")
    );
    println!("{side:?} ok, units {:?}", sim.units_consumed);
    let post = |i: usize| sim.accounts[i].as_ref();
    match side {
        TradeSide::Buy => post(2)
            .and_then(|a| pump::token_account_amount(&a.data))
            .unwrap_or(0)
            .saturating_sub(trade.pre.base_tokens),
        TradeSide::Sell => {
            let l = |i: usize| post(i).map_or(0, |a| a.lamports);
            l(0).saturating_add(l(1))
                .saturating_sub(trade.pre.sol_lamports)
        }
    }
}

async fn round_trip(program: Pubkey) {
    let intent = recent_buy(&program).await;
    println!(
        "source {} {} mint {} wallet {}",
        intent.signature,
        intent.venue.kind(),
        intent.mint,
        intent.source_wallet
    );
    let trade = decide(intent).await;
    let tokens = simulate(&trade, TradeSide::Buy, trade.amount_in).await;
    assert!(tokens > 0, "buy pays out tokens");
    println!("buy 0.001 SOL -> {tokens} tokens");
    let sell = (trade.pre.base_tokens / 100).max(1);
    if trade.pre.base_tokens > 0 {
        // Net of the simulated tx fee; tiny sells can legitimately net ~0.
        let lamports = simulate(&trade, TradeSide::Sell, sell).await;
        println!("sell {sell} tokens -> {lamports} lamports (net of fees)");
    }
}

#[tokio::test]
#[ignore = "needs mainnet RPC"]
async fn bonding_curve_buy_and_sell_simulate_on_mainnet() {
    round_trip(PUMP_PROGRAM).await;
}

#[tokio::test]
#[ignore = "needs mainnet RPC"]
async fn pumpswap_buy_and_sell_simulate_on_mainnet() {
    round_trip(PUMP_AMM_PROGRAM).await;
}
