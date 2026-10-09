//! Real mainnet transactions (fetched from RPC, see tests/fixtures/*.json). Expected values
//! were cross-checked against each tx's receipt (Transfer / WETH Deposit / Withdrawal logs).

use std::time::Instant;

use alloy::primitives::{Address, U256};
use alloy::rpc::types::Transaction;
use dex_copy_trade::decoder::classifier::TxClass;
use dex_copy_trade::decoder::{DecodeOutcome, DecoderRegistry};
use dex_copy_trade::models::{Dex, PoolKind, SwapKind, TradeSide};
use dex_copy_trade::utils::chain_addresses;
use dex_copy_trade::watcher::{TxOrigin, WatchedTx};

const WETH: &str = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2";
const ALL: [Dex; 4] = [
    Dex::UniswapV2,
    Dex::UniswapV3,
    Dex::UniswapUniversalRouter,
    Dex::UniswapV4,
];

struct Trade {
    fixture: &'static str,
    dex: Dex,
    kind: SwapKind,
    pool: PoolKind,
    side: TradeSide,
    token_in: &'static str,
    token_out: &'static str,
    amount_in: &'static str,
    min_out: Option<&'static str>,
    native_in: bool,
    native_out: bool,
    recipient: &'static str,
}

const TRADES: &[Trade] = &[
    Trade {
        fixture: "v2_buy_eth_fot",
        dex: Dex::UniswapV2,

        kind: SwapKind::ExactIn,

        pool: PoolKind::UniswapV2,
        side: TradeSide::Buy,
        token_in: WETH,
        token_out: "0x1a7ffb1810f051088f0bb7b4c51a39278cdcbb85",
        amount_in: "30000000000000000",
        min_out: None,
        native_in: true,
        native_out: false,
        recipient: "0x87052092af549685ce8d65cf14a934a680363859",
    },
    Trade {
        fixture: "v2_sell_eth_fot",
        dex: Dex::UniswapV2,

        kind: SwapKind::ExactIn,

        pool: PoolKind::UniswapV2,
        side: TradeSide::Sell,
        token_in: "0x1a7ffb1810f051088f0bb7b4c51a39278cdcbb85",
        token_out: WETH,
        amount_in: "32345815392377943",
        min_out: None,
        native_in: false,
        native_out: true,
        recipient: "0x21b5ad2fe32e3716607c4eddb08bed3b8c3212be",
    },
    Trade {
        fixture: "v2_exact_tokens_for_tokens",
        dex: Dex::UniswapV2,

        kind: SwapKind::ExactIn,

        pool: PoolKind::UniswapV2,
        side: TradeSide::Sell,
        token_in: "0xb2617246d0c6c0087f18703d576831899ca94f01",
        token_out: WETH,
        amount_in: "4905874266903613000000",
        min_out: Some("116080749410766580"),
        native_in: false,
        native_out: false,
        recipient: "0xb8f937b7bcfe9da01ad616544f149324a9a40a5f",
    },
    Trade {
        fixture: "v3_router_exact_input_single",
        dex: Dex::UniswapV3,

        kind: SwapKind::ExactIn,

        pool: PoolKind::UniswapV3,
        side: TradeSide::Buy,
        token_in: WETH,
        token_out: "0xb6026518463ec80f1cb33e4318e802487c27f995",
        amount_in: "500000000000000000",
        min_out: None,
        native_in: true,
        native_out: false,
        recipient: "0x273ec7b52dcbea08022363d0948b0091d04ec852",
    },
    Trade {
        fixture: "v3_router02_exact_input_single",
        dex: Dex::UniswapV3,

        kind: SwapKind::ExactIn,

        pool: PoolKind::UniswapV3,
        side: TradeSide::Buy,
        token_in: WETH,
        token_out: "0x6112c3509a8a787df576028450febb3786a2274d",
        amount_in: "846600111547415",
        min_out: Some("215047997326800892054"),
        native_in: true,
        native_out: false,
        recipient: "0xfc489022a1a4ad9d289801744aff510f67db2c90",
    },
    Trade {
        fixture: "v3_router02_multicall_buy_eth",
        dex: Dex::UniswapV3,

        kind: SwapKind::ExactIn,

        pool: PoolKind::UniswapV3,
        side: TradeSide::Buy,
        token_in: WETH,
        token_out: "0xdac17f958d2ee523a2206206994597c13d831ec7",
        amount_in: "19040870000000000",
        min_out: Some("48331419"),
        native_in: true,
        native_out: false,
        recipient: "0xc7a41d4e631efd28c2b6af3435f61062172b0e7b",
    },
    Trade {
        fixture: "v3_router02_multicall_sell_unwrap",
        dex: Dex::UniswapV3,

        kind: SwapKind::ExactIn,

        pool: PoolKind::UniswapV3,
        side: TradeSide::Sell,
        token_in: "0xdac17f958d2ee523a2206206994597c13d831ec7",
        token_out: WETH,
        amount_in: "60000000",
        min_out: Some("23142503764241185"),
        native_in: false,
        native_out: true,
        recipient: "0xa693a495c30e3334223b007d44b5261e9e453b79",
    },
    // WRAP_ETH -> TRANSFER (1% WETH fee) -> V2 swap: amount_in is what was actually swapped.
    Trade {
        fixture: "ur_buy_wrap_fee_v2",
        dex: Dex::UniswapUniversalRouter,

        kind: SwapKind::ExactIn,

        pool: PoolKind::UniswapV2,
        side: TradeSide::Buy,
        token_in: WETH,
        token_out: "0x7368563b2395e96fc74282c6987eea844f4f7132",
        amount_in: "9581734337682290",
        min_out: Some("15252984941649765594068"),
        native_in: true,
        native_out: false,
        recipient: "0x982a7d7b89180076b1f886cb01e05ca4c2771877",
    },
    // WRAP_ETH -> V2 swap to router (min 0) -> PAY_PORTION -> SWEEP: bound/recipient from SWEEP.
    Trade {
        fixture: "ur_buy_v2_sweep",
        dex: Dex::UniswapUniversalRouter,

        kind: SwapKind::ExactIn,

        pool: PoolKind::UniswapV2,
        side: TradeSide::Buy,
        token_in: WETH,
        token_out: "0x090993bcc9dd600d15805d5ff070941cac6a506f",
        amount_in: "34100000000000000",
        min_out: Some("151470571132176"),
        native_in: true,
        native_out: false,
        recipient: "0x7198754a0c231733c55dea90fd73fbe4de802d7e",
    },
    Trade {
        fixture: "ur_sell_unwrap",
        dex: Dex::UniswapUniversalRouter,

        kind: SwapKind::ExactIn,

        pool: PoolKind::UniswapV2,
        side: TradeSide::Sell,
        token_in: "0x7368563b2395e96fc74282c6987eea844f4f7132",
        token_out: WETH,
        amount_in: "15213413701817672355808",
        min_out: Some("8394241936667274"),
        native_in: false,
        native_out: true,
        recipient: "0x2d408c719cfe9427cfa9b8ad4ff311b48d0ebed9",
    },
    // --- Uniswap V4 (Universal Router V4_SWAP); native ETH is address(0), shown as WETH.
    // SWAP_EXACT_IN_SINGLE (ETH pool) + SETTLE_ALL(ETH) + TAKE_ALL(token).
    Trade {
        fixture: "v4_buy_native_single",
        dex: Dex::UniswapUniversalRouter,
        kind: SwapKind::ExactIn,
        pool: PoolKind::UniswapV4,
        side: TradeSide::Buy,
        token_in: WETH,
        token_out: "0xc8fb80fcc03f699c70ff0cc08c09106288888888",
        amount_in: "19608391578061711",
        min_out: Some("225340097531658062621"),
        native_in: true,
        native_out: false,
        recipient: "0x459e88822f07e5c458030bdaca0973613982d15a",
    },
    // SWAP_EXACT_IN with a 2-hop path.
    Trade {
        fixture: "v4_exact_in_path",
        dex: Dex::UniswapUniversalRouter,
        kind: SwapKind::ExactIn,
        pool: PoolKind::UniswapV4,
        side: TradeSide::Buy,
        token_in: WETH,
        token_out: "0xf4f986ba1362ed61f9aeb683be20f6219056d2c1",
        amount_in: "39061737075447744",
        min_out: Some("1196621677654933540168449"),
        native_in: true,
        native_out: false,
        recipient: "0x2f2ab3ec2cfa323f26307fb529b1a30c2cc0d998",
    },
    // Exact-output BUY: amount_in = max input, min_out = exact output.
    Trade {
        fixture: "v4_buy_exact_out",
        dex: Dex::UniswapUniversalRouter,
        kind: SwapKind::ExactOut,
        pool: PoolKind::UniswapV4,
        side: TradeSide::Buy,
        token_in: WETH,
        token_out: "0x1b54e762aa34cf6e28e9c082f2848e28e45da6b8",
        amount_in: "87907246760604615",
        min_out: Some("16147120000000000800355"),
        native_in: true,
        native_out: false,
        recipient: "0x6cb5ee0270d405ccee26b9b45629df80148dbe92",
    },
    // SELL into an ETH pool: token pulled via Permit2, TAKE_ALL(ETH) to the sender.
    Trade {
        fixture: "v4_sell_native_single",
        dex: Dex::UniswapUniversalRouter,
        kind: SwapKind::ExactIn,
        pool: PoolKind::UniswapV4,
        side: TradeSide::Sell,
        token_in: "0xd34a99bc0f67ae1bbd63c660e6d0b0dd03e263b7",
        token_out: WETH,
        amount_in: "83250966164051832832",
        min_out: Some("300523959583198848"),
        native_in: false,
        native_out: true,
        recipient: "0x0a6a99fc2f211413db16c8ba0fa4512a526e7afd",
    },
];

fn load(name: &str) -> WatchedTx {
    let path = format!("{}/tests/fixtures/{name}.json", env!("CARGO_MANIFEST_DIR"));
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let tx: Transaction = serde_json::from_str(&raw).unwrap();
    WatchedTx {
        tx,
        origin: TxOrigin::Pending,
        detected_at: Instant::now(),
    }
}

fn registry(dexes: &[Dex]) -> DecoderRegistry {
    DecoderRegistry::new(&chain_addresses(1).unwrap(), dexes)
}

fn addr(s: &str) -> Address {
    s.parse().unwrap()
}

fn amount(s: &str) -> U256 {
    s.parse().unwrap()
}

#[test]
fn real_swaps_decode_to_expected_intents() {
    let reg = registry(&ALL);
    for t in TRADES {
        let item = load(t.fixture);
        let DecodeOutcome::Trade(i) = reg.process(&item) else {
            panic!(
                "{}: expected a trade, got {:?}",
                t.fixture,
                reg.process(&item)
            );
        };
        let f = t.fixture;
        assert_eq!(i.dex, t.dex, "{f}");
        assert_eq!(i.side, t.side, "{f}");
        assert_eq!(i.token_in, addr(t.token_in), "{f}");
        assert_eq!(i.token_out, addr(t.token_out), "{f}");
        assert_eq!(i.amount_in, amount(t.amount_in), "{f}");
        assert_eq!(i.min_amount_out, t.min_out.map(amount), "{f}");
        assert_eq!(
            (i.native_in, i.native_out),
            (t.native_in, t.native_out),
            "{f}"
        );
        assert_eq!(i.recipient, addr(t.recipient), "{f}");
        assert_eq!(i.swap_kind, t.kind, "{f}");
        assert_eq!(i.route[0].pool, t.pool, "{f}");
        assert_eq!(
            i.route[0].v4.is_some(),
            t.pool == PoolKind::UniswapV4,
            "{f}"
        );
        assert_eq!(i.source_tx_hash, item.tx.inner.tx_hash().to_owned(), "{f}");
        assert_eq!(i.route.len(), 1, "{f}");
    }
}

#[test]
fn v4_path_keeps_every_hop() {
    let out = registry(&ALL).process(&load("v4_exact_in_path"));
    let DecodeOutcome::Trade(i) = out else {
        panic!("{out:?}");
    };
    let v4 = i.route[0].v4.as_ref().unwrap();
    assert_eq!(v4.hops.len(), 2);
    assert_eq!(v4.currency_in, Address::ZERO, "native ETH input");
    assert_eq!(i.route[0].tokens.len(), 3);
}

#[test]
fn v4_ignored_when_disabled() {
    let dexes = [Dex::UniswapUniversalRouter];
    let out = registry(&dexes).process(&load("v4_buy_native_single"));
    let DecodeOutcome::Ignored { dex, reason } = out else {
        panic!("expected Ignored, got {out:?}");
    };
    assert_eq!(dex, Dex::UniswapUniversalRouter);
    assert!(reason.to_string().contains("UNISWAP_V4"));
    let t2t = registry(&ALL).process(&load("v4_token_to_token"));
    assert!(
        matches!(t2t, DecodeOutcome::Ignored { ref reason, .. } if reason.to_string().contains("token-to-token")),
        "{t2t:?}"
    );
}

#[test]
fn token_to_token_is_ignored() {
    let out = registry(&ALL).process(&load("v3_router_exact_output_single_t2t"));
    assert!(
        matches!(out, DecodeOutcome::Ignored { dex: Dex::UniswapV3, ref reason } if reason.to_string().contains("token-to-token")),
        "{out:?}"
    );
}

#[test]
fn disabled_dex_is_not_decoded() {
    let out = registry(&[Dex::UniswapV2]).process(&load("ur_buy_v2_sweep"));
    assert!(
        matches!(out, DecodeOutcome::NotSwap(TxClass::ContractCall)),
        "{out:?}"
    );
}
