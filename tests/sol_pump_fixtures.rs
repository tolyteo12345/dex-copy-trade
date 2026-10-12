//! Real mainnet pump.fun / PumpSwap transactions (RPC `getTransaction`, see
//! tests/fixtures/sol/*.json). Expected amounts were cross-checked against each tx's
//! token balance changes.
#![cfg(feature = "sol")]

use std::str::FromStr;

use solana_pubkey::Pubkey;

use dex_copy_trade::sol::decoder::{Skip, SolDecoder, source_tx_from_rpc_json};
use dex_copy_trade::sol::models::{SolTradeIntent, SourceTx, TradeSide, Venue};
use dex_copy_trade::sol::pump::{
    self, BUYBACK_FEE_RECIPIENTS, Market, PUMP_AMM_PROGRAM, PUMP_PROGRAM, PoolVaults,
    TOKEN_2022_PROGRAM, TOKEN_PROGRAM, WSOL_MINT,
};

fn load(name: &str) -> SourceTx {
    let path = format!(
        "{}/tests/fixtures/sol/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    source_tx_from_rpc_json(&serde_json::from_str(&raw).unwrap()).expect("fixture parses")
}

fn pk(s: &str) -> Pubkey {
    Pubkey::from_str(s).unwrap()
}

/// Decodes as if the tx's fee payer were the watched wallet.
fn decode(name: &str) -> Result<SolTradeIntent, Skip> {
    let tx = load(name);
    SolDecoder::new([tx.account_keys[0]]).decode(&tx)
}

fn expect(name: &str, venue: Venue, side: TradeSide, mint: &str, sol: u64, tokens: u64) {
    let i = decode(name).unwrap_or_else(|e| panic!("{name}: {e}"));
    assert_eq!(i.venue, venue, "{name}");
    assert_eq!(i.side, side, "{name}");
    assert_eq!(i.mint, pk(mint), "{name}");
    assert_eq!((i.source_sol, i.source_tokens), (sol, tokens), "{name}");
    assert_eq!(i.source_wallet, load(name).account_keys[0], "{name}");
}

#[test]
fn bonding_curve_trades_direct_and_routed() {
    use TradeSide::*;
    let curve = Venue::BondingCurve;
    expect(
        "curve_buy_direct",
        curve,
        Buy,
        "6UTxqoZzuLDRZFN63GvfFaF6PHh5ncmsu3DBk972pump",
        10_000_001,
        127_452_963_724,
    );
    expect(
        "curve_buy_exact_sol_in",
        curve,
        Buy,
        "HLjZsvQPTfdJKWycdNSMuuY5AgcFjcMaR2aqunHUpump",
        296_296_296,
        3_676_438_191_728,
    );
    // v1 transaction, buy_v3 called through a router program.
    expect(
        "curve_buy_v3_router",
        curve,
        Buy,
        "68eGdrUBX87sMvJ7SV7rJvA4P53F2ntf45WT1C4Dpump",
        469_135_416,
        6_371_583_105_909,
    );
    expect(
        "curve_sell_direct",
        curve,
        Sell,
        "HLjZsvQPTfdJKWycdNSMuuY5AgcFjcMaR2aqunHUpump",
        258_714_379,
        3_320_796_314_672,
    );
    expect(
        "curve_sell_v3_router",
        curve,
        Sell,
        "91fBkXU5mBCekf2JvEGK4Km9xzLQvJZ5sR2roBV1pump",
        803_264_292,
        16_237_076_016_017,
    );
    expect(
        "curve_sell_v2_jupiter",
        curve,
        Sell,
        "3Gx9tH6yEcPvMocccUH24BCfq5NwkGSbgBejbnWYpump",
        1_026_166_693,
        26_835_197_723_102,
    );
}

#[test]
fn pumpswap_trades() {
    expect(
        "amm_buy_legacy",
        Venue::PumpSwap {
            pool: pk("9axwUn5sPyFfZSENYcWg7SRJjoSfpbdXXBXRdzVVTR2c"),
        },
        TradeSide::Buy,
        "7KKUmUG4p8kta1sBbMHnRUNoBfXX4zYbUFHspdHwpump",
        1_576_801_943,
        715_551_917_281,
    );
    expect(
        "amm_sell_legacy",
        Venue::PumpSwap {
            pool: pk("H7SopFMt5nyi9g264QxJpQjGoSJGt13mpVDCKVoe4ShM"),
        },
        TradeSide::Sell,
        "5fePPYbUE2yQHD4AfKetsZzVLuz1Hu8K2mSuCBu5pump",
        354_015,
        1_346_623,
    );
}

#[test]
fn unsupported_shapes_are_skipped() {
    // Pool with WSOL as base: the "sell" is really a buy of the quote coin.
    assert_eq!(decode("amm_inverted_pool").unwrap_err(), Skip::InvertedPool);
    // Buy and sell of the same coin in one tx (volume/MEV bundle).
    assert_eq!(
        decode("curve_buy_and_sell_same_tx").unwrap_err(),
        Skip::Complex(2)
    );
    // SOL -> coin A -> coin B through multi_hop_swap.
    assert_eq!(decode("multi_hop").unwrap_err(), Skip::Complex(3));
    // A program PDA traded; the signer's own balances did not move.
    assert_eq!(decode("curve_router_user_pda").unwrap_err(), Skip::NoTrade);
    assert_eq!(decode("amm_sell_v2_router_pda").unwrap_err(), Skip::NoTrade);
}

#[test]
fn only_signing_watched_wallets_count() {
    let tx = load("curve_buy_direct");
    let stranger = Pubkey::new_unique();
    assert_eq!(
        SolDecoder::new([stranger]).decode(&tx).unwrap_err(),
        Skip::NotSigner
    );
    // Non-signer accounts of the tx (e.g. the mint) never make it a watched trade.
    let mint = pk("6UTxqoZzuLDRZFN63GvfFaF6PHh5ncmsu3DBk972pump");
    assert_eq!(
        SolDecoder::new([mint]).decode(&tx).unwrap_err(),
        Skip::NotSigner
    );
}

/// Account keys of the first instruction to `program` whose data starts with `disc`.
fn real_accounts(tx: &SourceTx, program: &Pubkey, disc: &[u8]) -> Vec<Pubkey> {
    let ix = tx
        .instructions
        .iter()
        .find(|ix| tx.account_keys[ix.program_id_index] == *program && ix.data.starts_with(disc))
        .expect("instruction present");
    ix.accounts
        .iter()
        .map(|a| tx.account_keys[usize::from(*a)])
        .collect()
}

fn built_trade_accounts(user: &Pubkey, side: TradeSide, m: &Market) -> Vec<Pubkey> {
    let program = if m.pool.is_some() {
        PUMP_AMM_PROGRAM
    } else {
        PUMP_PROGRAM
    };
    let ixs = pump::swap_instructions(user, side, m, 1, 1);
    let trade = ixs.iter().find(|ix| ix.program_id == program).unwrap();
    trade.accounts.iter().map(|a| a.pubkey).collect()
}

/// The bot's v3 curve accounts, derived from scratch, equal what pump's own clients sent
/// (the buyback recipient may be any of the 8).
#[test]
fn curve_v3_accounts_match_real_buy_and_sell() {
    for (name, disc, side, mint) in [
        (
            "curve_buy_v3_router",
            [7u8, 5, 29, 196, 245, 23, 101, 80],
            TradeSide::Buy,
            "68eGdrUBX87sMvJ7SV7rJvA4P53F2ntf45WT1C4Dpump",
        ),
        (
            "curve_sell_v3_router",
            [28, 146, 222, 119, 38, 196, 105, 213],
            TradeSide::Sell,
            "91fBkXU5mBCekf2JvEGK4Km9xzLQvJZ5sR2roBV1pump",
        ),
    ] {
        let tx = load(name);
        let real = real_accounts(&tx, &PUMP_PROGRAM, &disc);
        let market = Market {
            mint: pk(mint),
            base_token_program: TOKEN_2022_PROGRAM,
            pool: None,
        };
        let built = built_trade_accounts(&tx.account_keys[0], side, &market);
        assert_eq!(real.len(), 17, "{name}");
        assert_eq!(built.len(), 17, "{name}");
        for i in (0..17).filter(|&i| i != 13) {
            assert_eq!(built[i], real[i], "{name}: account #{i}");
        }
        assert!(BUYBACK_FEE_RECIPIENTS.contains(&real[13]));
        assert!(BUYBACK_FEE_RECIPIENTS.contains(&built[13]));
    }
}

/// PumpSwap v2: the source traded through a router PDA, so its user-owned accounts
/// (#1, #5, #6, #12) differ by design; everything protocol-side must match.
#[test]
fn amm_v2_accounts_match_real_sell() {
    let tx = load("amm_sell_v2_router_pda");
    let real = real_accounts(
        &tx,
        &PUMP_AMM_PROGRAM,
        &[93, 246, 130, 60, 231, 233, 64, 178],
    );
    let market = Market {
        mint: real[3],
        base_token_program: real[9],
        pool: Some(PoolVaults {
            pool: real[0],
            base_vault: real[7],
            quote_vault: real[8],
        }),
    };
    let user = real[1];
    let built = built_trade_accounts(&user, TradeSide::Sell, &market);
    assert_eq!((real.len(), built.len()), (17, 17));
    for i in [0, 1, 2, 3, 4, 7, 8, 9, 10, 11, 12, 13, 15, 16] {
        assert_eq!(built[i], real[i], "account #{i}");
    }
    assert_eq!(real[4], WSOL_MINT);
    assert_eq!(real[10], TOKEN_PROGRAM);
    // Buyback recipient is a buyback wallet's WSOL ATA (any of the 8).
    let is_buyback_ata = |k: &Pubkey| {
        BUYBACK_FEE_RECIPIENTS
            .iter()
            .any(|w| pump::ata(w, &WSOL_MINT, &TOKEN_PROGRAM) == *k)
    };
    assert!(is_buyback_ata(&real[14]) && is_buyback_ata(&built[14]));
}
