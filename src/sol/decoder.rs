//! SourceTx -> SolTradeIntent, from the events pump programs emit for every trade.
//!
//! Events (not instructions) are the source of truth: every instruction variant (legacy, v2,
//! v3, exact-in/out) and every router (Jupiter, trading bots) ends in the same `TradeEvent` /
//! `BuyEvent` / `SellEvent`, so new pump instruction versions need no decoder change.

use std::collections::HashSet;
use std::str::FromStr;
use std::time::Instant;

use solana_pubkey::Pubkey;
use solana_signature::Signature;
use tokio::sync::mpsc;

use crate::sol::models::{Ix, SolTradeIntent, SourceTx, TokenBalance, TradeSide, Venue};
use crate::sol::pump::{self, PUMP_AMM_PROGRAM, PumpEvent, WSOL_MINT};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Skip {
    #[error("no watched wallet signed the tx")]
    NotSigner,
    #[error("no pump trade by the watched wallet")]
    NoTrade,
    #[error("several pump trades in one tx ({0}); only single-token buys/sells are copied")]
    Complex(usize),
    #[error("cannot resolve the traded mint")]
    UnknownMint,
    #[error("PumpSwap pool with SOL as base (inverted pool) is not supported")]
    InvertedPool,
    #[error("quote is not SOL (token-to-token route)")]
    NonSolQuote,
}

/// One pump trade found in the tx.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Leg {
    venue: Venue,
    side: TradeSide,
    mint: Option<Pubkey>,
    sol: u64,
    tokens: u64,
}

pub struct SolDecoder {
    wallets: HashSet<Pubkey>,
}

impl SolDecoder {
    pub fn new(wallets: impl IntoIterator<Item = Pubkey>) -> Self {
        Self {
            wallets: wallets.into_iter().collect(),
        }
    }

    pub fn decode(&self, tx: &SourceTx) -> Result<SolTradeIntent, Skip> {
        // Only trades the wallet signed itself; incoming transfers etc. also match the
        // subscription filter but are not trades of that wallet.
        let trader = *tx
            .account_keys
            .iter()
            .take(tx.num_signers)
            .find(|k| self.wallets.contains(*k))
            .ok_or(Skip::NotSigner)?;

        let mut legs: Vec<Leg> = Vec::new();
        for ix in &tx.instructions {
            let Some(program) = tx.account_keys.get(ix.program_id_index) else {
                continue;
            };
            let Some(event) = pump::parse_event(program, &ix.data) else {
                continue;
            };
            let (leg, user) = match event {
                PumpEvent::Curve {
                    mint,
                    sol_amount,
                    token_amount,
                    is_buy,
                    user,
                } => (
                    Leg {
                        venue: Venue::BondingCurve,
                        side: if is_buy {
                            TradeSide::Buy
                        } else {
                            TradeSide::Sell
                        },
                        mint: Some(mint),
                        sol: sol_amount,
                        tokens: token_amount,
                    },
                    user,
                ),
                PumpEvent::Amm {
                    side,
                    pool,
                    user,
                    user_base_token_account,
                    base_amount,
                    quote_amount,
                } => (
                    Leg {
                        venue: Venue::PumpSwap { pool },
                        side,
                        mint: amm_base_mint(tx, &pool)
                            .or_else(|| token_account_mint(tx, &user_base_token_account)),
                        sol: quote_amount,
                        tokens: base_amount,
                    },
                    user,
                ),
            };
            if user == trader || moved_as_expected(tx, &trader, &leg) {
                legs.push(leg);
            }
        }

        let first = *legs.first().ok_or(Skip::NoTrade)?;
        if legs
            .iter()
            .any(|l| (l.venue, l.side, l.mint) != (first.venue, first.side, first.mint))
        {
            return Err(Skip::Complex(legs.len()));
        }
        let mint = first.mint.ok_or(Skip::UnknownMint)?;
        if mint == WSOL_MINT {
            return Err(Skip::InvertedPool);
        }
        // Split fills of the same trade (e.g. a router hitting the curve twice) are summed.
        let sol = legs.iter().fold(0u64, |acc, l| acc.saturating_add(l.sol));
        let tokens = legs
            .iter()
            .fold(0u64, |acc, l| acc.saturating_add(l.tokens));
        if sol == 0 {
            // The curve reports 0 SOL when its quote is another pump coin.
            return Err(Skip::NonSolQuote);
        }
        Ok(SolTradeIntent {
            signature: tx.signature,
            slot: tx.slot,
            source_wallet: trader,
            venue: first.venue,
            side: first.side,
            mint,
            source_sol: sol,
            source_tokens: tokens,
            seen_at: tx.seen_at,
        })
    }
}

/// PumpSwap events carry no mint. Every PumpSwap trade instruction version starts with
/// `pool, user, global_config, base_mint`, so the base mint is read from the instruction
/// that traded on `pool`.
fn amm_base_mint(tx: &SourceTx, pool: &Pubkey) -> Option<Pubkey> {
    tx.instructions.iter().find_map(|ix| {
        let key = |i: usize| {
            ix.accounts
                .get(i)
                .and_then(|a| tx.account_keys.get(usize::from(*a)))
        };
        (tx.account_keys.get(ix.program_id_index) == Some(&PUMP_AMM_PROGRAM)
            && key(0) == Some(pool))
        .then(|| key(3).copied())
        .flatten()
    })
}

fn token_account_mint(tx: &SourceTx, account: &Pubkey) -> Option<Pubkey> {
    let idx = tx.account_keys.iter().position(|k| k == account)?;
    tx.post_token_balances
        .iter()
        .chain(&tx.pre_token_balances)
        .find(|b| b.account_index == idx)
        .map(|b| b.mint)
}

/// When a router trades through its own PDA, the event's `user` is the PDA; the trade
/// still counts if the signer's own balance of the mint moved in the trade's direction.
fn moved_as_expected(tx: &SourceTx, owner: &Pubkey, leg: &Leg) -> bool {
    let Some(mint) = leg.mint else { return false };
    let total = |balances: &[TokenBalance]| -> i128 {
        balances
            .iter()
            .filter(|b| b.mint == mint && b.owner.as_ref() == Some(owner))
            .map(|b| i128::from(b.amount))
            .sum()
    };
    let delta = total(&tx.post_token_balances) - total(&tx.pre_token_balances);
    match leg.side {
        TradeSide::Buy => delta > 0,
        TradeSide::Sell => delta < 0,
    }
}

pub async fn run_stage(
    decoder: SolDecoder,
    mut rx: mpsc::Receiver<SourceTx>,
    out: mpsc::Sender<SolTradeIntent>,
) {
    while let Some(tx) = rx.recv().await {
        match decoder.decode(&tx) {
            Ok(intent) => {
                tracing::info!(
                    event = "trade_decoded",
                    signature = %intent.signature,
                    slot = intent.slot,
                    source_wallet = %intent.source_wallet,
                    venue = %intent.venue.kind(),
                    side = ?intent.side,
                    mint = %intent.mint,
                    source_sol = intent.source_sol,
                    source_tokens = intent.source_tokens,
                    seen_to_decode_us = intent.seen_at.elapsed().as_micros() as u64,
                );
                if out.send(intent).await.is_err() {
                    break;
                }
            }
            Err(skip @ (Skip::NotSigner | Skip::NoTrade)) => {
                tracing::debug!(event = "tx_skipped", signature = %tx.signature, reason = %skip);
            }
            Err(skip) => {
                tracing::info!(event = "tx_skipped", signature = %tx.signature, reason = %skip);
            }
        }
    }
}

/// RPC `getTransaction` result (`encoding: "json"`, `maxSupportedTransactionVersion: 1`) ->
/// SourceTx. Used by the fixture and live tests; the bot itself gets txs from gRPC.
pub fn source_tx_from_rpc_json(v: &serde_json::Value) -> Option<SourceTx> {
    let tx = &v["transaction"];
    let msg = &tx["message"];
    let meta = &v["meta"];
    let keys = |arr: &serde_json::Value| -> Option<Vec<Pubkey>> {
        arr.as_array()
            .map(|a| {
                a.iter()
                    .map(|k| k.as_str().and_then(|s| Pubkey::from_str(s).ok()))
                    .collect::<Option<Vec<_>>>()
            })
            .unwrap_or(Some(Vec::new()))
    };
    let mut account_keys = keys(&msg["accountKeys"])?;
    account_keys.extend(keys(&meta["loadedAddresses"]["writable"])?);
    account_keys.extend(keys(&meta["loadedAddresses"]["readonly"])?);

    let parse_ix = |ix: &serde_json::Value| -> Option<Ix> {
        Some(Ix {
            program_id_index: usize::try_from(ix["programIdIndex"].as_u64()?).ok()?,
            accounts: ix["accounts"]
                .as_array()?
                .iter()
                .map(|a| a.as_u64().and_then(|n| u8::try_from(n).ok()))
                .collect::<Option<Vec<_>>>()?,
            data: bs58::decode(ix["data"].as_str()?).into_vec().ok()?,
        })
    };
    let mut instructions = msg["instructions"]
        .as_array()?
        .iter()
        .map(parse_ix)
        .collect::<Option<Vec<_>>>()?;
    for group in meta["innerInstructions"].as_array()? {
        for ix in group["instructions"].as_array()? {
            instructions.push(parse_ix(ix)?);
        }
    }
    let balances = |arr: &serde_json::Value| -> Option<Vec<TokenBalance>> {
        arr.as_array()?
            .iter()
            .map(|b| {
                Some(TokenBalance {
                    account_index: usize::try_from(b["accountIndex"].as_u64()?).ok()?,
                    mint: Pubkey::from_str(b["mint"].as_str()?).ok()?,
                    owner: b["owner"].as_str().and_then(|s| Pubkey::from_str(s).ok()),
                    amount: b["uiTokenAmount"]["amount"].as_str()?.parse().ok()?,
                })
            })
            .collect()
    };
    let signature = bs58::decode(tx["signatures"][0].as_str()?)
        .into_vec()
        .ok()
        .and_then(|b| Signature::try_from(b.as_slice()).ok())?;
    Some(SourceTx {
        signature,
        slot: v["slot"].as_u64()?,
        account_keys,
        num_signers: usize::try_from(msg["header"]["numRequiredSignatures"].as_u64()?).ok()?,
        instructions,
        pre_token_balances: balances(&meta["preTokenBalances"])?,
        post_token_balances: balances(&meta["postTokenBalances"])?,
        seen_at: Instant::now(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tx_with(instructions: Vec<Ix>, keys: Vec<Pubkey>) -> SourceTx {
        SourceTx {
            signature: Signature::default(),
            slot: 1,
            account_keys: keys,
            num_signers: 1,
            instructions,
            pre_token_balances: vec![],
            post_token_balances: vec![],
            seen_at: Instant::now(),
        }
    }

    fn curve_event(mint: &Pubkey, user: &Pubkey, is_buy: bool, sol: u64) -> Vec<u8> {
        let mut d = vec![0xe4, 0x45, 0xa5, 0x2e, 0x51, 0xcb, 0x9a, 0x1d];
        d.extend_from_slice(&[189, 219, 127, 211, 78, 230, 97, 238]);
        d.extend_from_slice(mint.as_ref());
        d.extend_from_slice(&sol.to_le_bytes());
        d.extend_from_slice(&500u64.to_le_bytes());
        d.push(u8::from(is_buy));
        d.extend_from_slice(user.as_ref());
        d
    }

    #[test]
    fn unwatched_or_non_signer_wallets_are_skipped() {
        let (wallet, mint) = (Pubkey::new_unique(), Pubkey::new_unique());
        let ev = Ix {
            program_id_index: 1,
            accounts: vec![],
            data: curve_event(&mint, &wallet, true, 10),
        };
        let other = Pubkey::new_unique();
        // Watched wallet present but not a signer (e.g. it received a transfer).
        let tx = tx_with(vec![ev.clone()], vec![other, pump::PUMP_PROGRAM, wallet]);
        assert_eq!(
            SolDecoder::new([wallet]).decode(&tx).unwrap_err(),
            Skip::NotSigner
        );
        let tx = tx_with(vec![ev], vec![wallet, pump::PUMP_PROGRAM]);
        let intent = SolDecoder::new([wallet]).decode(&tx).unwrap();
        assert_eq!(
            (intent.side, intent.mint, intent.source_sol),
            (TradeSide::Buy, mint, 10)
        );
    }

    #[test]
    fn event_from_another_program_id_is_ignored() {
        let (wallet, mint) = (Pubkey::new_unique(), Pubkey::new_unique());
        let fake = Pubkey::new_unique();
        let tx = tx_with(
            vec![Ix {
                program_id_index: 1,
                accounts: vec![],
                data: curve_event(&mint, &wallet, true, 10),
            }],
            vec![wallet, fake],
        );
        assert_eq!(
            SolDecoder::new([wallet]).decode(&tx).unwrap_err(),
            Skip::NoTrade
        );
    }

    #[test]
    fn split_fills_are_summed_and_zero_sol_is_rejected() {
        let (wallet, mint) = (Pubkey::new_unique(), Pubkey::new_unique());
        let ev = |sol| Ix {
            program_id_index: 1,
            accounts: vec![],
            data: curve_event(&mint, &wallet, false, sol),
        };
        let keys = vec![wallet, pump::PUMP_PROGRAM];
        let d = SolDecoder::new([wallet]);
        let intent = d
            .decode(&tx_with(vec![ev(3), ev(4)], keys.clone()))
            .unwrap();
        assert_eq!(
            (intent.side, intent.source_sol, intent.source_tokens),
            (TradeSide::Sell, 7, 1_000)
        );
        assert_eq!(
            d.decode(&tx_with(vec![ev(0)], keys)).unwrap_err(),
            Skip::NonSolQuote
        );
    }
}
