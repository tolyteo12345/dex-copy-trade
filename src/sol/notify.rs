//! SOL notification messages. Delivery (queue, retries, Telegram client) lives in
//! [`crate::common::telegram`].

use std::fmt;

use solana_pubkey::Pubkey;
use solana_signature::Signature;

use crate::common::telegram::{self, Render, escape};
use crate::sol::config::LAMPORTS_PER_SOL;
use crate::sol::models::{TradeSide, VenueKind};

const EXPLORER: &str = "https://solscan.io";

pub type Notifier = telegram::Notifier<SolNotification>;

#[derive(Debug, Clone)]
pub enum SolNotification {
    /// Decision stage approved copying a source trade.
    Signal {
        source_wallet: Pubkey,
        signature: Signature,
        venue: VenueKind,
        side: TradeSide,
        mint: Pubkey,
        amount_in: u64,
        capped: bool,
    },
    /// SOL_DRY_RUN only: the copy simulated fine and would have been sent.
    Simulated {
        signature: Signature,
        side: TradeSide,
        mint: Pubkey,
        amount_in: u64,
        min_out: u64,
    },
    Broadcasted {
        signature: Signature,
        side: TradeSide,
        mint: Pubkey,
        amount_in: u64,
        min_out: u64,
        tx: Signature,
    },
    Confirmed {
        signature: Signature,
        side: TradeSide,
        mint: Pubkey,
        tx: Signature,
        slot: u64,
    },
    Failed {
        signature: Signature,
        side: TradeSide,
        mint: Pubkey,
        stage: &'static str,
        reason: String,
        tx: Option<Signature>,
    },
}

impl Render for SolNotification {
    /// Telegram HTML. Everything not produced here (error texts) goes through `escape`.
    fn render(&self, dry_run: bool) -> String {
        let mut out = String::new();
        if dry_run {
            out.push_str("🧪 <b>[DRY RUN]</b> ");
        }
        let header =
            |out: &mut String, title: &str, side: TradeSide, sig: &Signature, mint: &Pubkey| {
                out.push_str(&format!("{title} · SOL {}\n", side_str(side)));
                out.push_str(&format!("Source tx: {}\n", link("tx", sig)));
                out.push_str(&format!("Token: {}\n", link("token", mint)));
            };
        match self {
            SolNotification::Signal {
                source_wallet,
                signature,
                venue,
                side,
                mint,
                amount_in,
                capped,
            } => {
                header(&mut out, "📡 <b>Copy signal</b>", *side, signature, mint);
                out.push_str(&format!("Wallet: {}\n", link("account", source_wallet)));
                out.push_str(&format!("Venue: {venue}\n"));
                out.push_str(&format!(
                    "Size: {}{}",
                    amount(*side, *amount_in, true),
                    if *capped {
                        " (capped to SOL_MAX_TRADE_SOL)"
                    } else {
                        ""
                    }
                ));
            }
            SolNotification::Simulated {
                signature,
                side,
                mint,
                amount_in,
                min_out,
            } => {
                header(&mut out, "✅ <b>Copy simulated</b>", *side, signature, mint);
                out.push_str(&format!("In: {}\n", amount(*side, *amount_in, true)));
                out.push_str(&format!("Min out: {}", amount(*side, *min_out, false)));
            }
            SolNotification::Broadcasted {
                signature,
                side,
                mint,
                amount_in,
                min_out,
                tx,
            } => {
                header(&mut out, "🚀 <b>Copy sent</b>", *side, signature, mint);
                out.push_str(&format!("In: {}\n", amount(*side, *amount_in, true)));
                out.push_str(&format!("Min out: {}\n", amount(*side, *min_out, false)));
                out.push_str(&format!("Copy tx: {}", link("tx", tx)));
            }
            SolNotification::Confirmed {
                signature,
                side,
                mint,
                tx,
                slot,
            } => {
                header(&mut out, "🎯 <b>Copy confirmed</b>", *side, signature, mint);
                out.push_str(&format!("Copy tx: {}\n", link("tx", tx)));
                out.push_str(&format!("Slot: {slot}"));
            }
            SolNotification::Failed {
                signature,
                side,
                mint,
                stage,
                reason,
                tx,
            } => {
                header(&mut out, "❌ <b>Copy failed</b>", *side, signature, mint);
                if let Some(tx) = tx {
                    out.push_str(&format!("Copy tx: {}\n", link("tx", tx)));
                }
                out.push_str(&format!("Stage: {}\n", escape(stage)));
                out.push_str(&format!("Reason: <code>{}</code>", escape(reason)));
            }
        }
        out
    }
}

fn side_str(side: TradeSide) -> &'static str {
    match side {
        TradeSide::Buy => "BUY",
        TradeSide::Sell => "SELL",
    }
}

/// The SOL leg in SOL; the token leg in raw units (decimals are not fetched on the
/// trading path for a cosmetic gain).
fn amount(side: TradeSide, value: u64, is_input: bool) -> String {
    if (side == TradeSide::Buy) == is_input {
        format!(
            "{}.{:09} SOL",
            value / LAMPORTS_PER_SOL,
            value % LAMPORTS_PER_SOL
        )
    } else {
        format!("{value} (raw token units)")
    }
}

fn link(kind: &str, value: impl fmt::Display) -> String {
    telegram::link(EXPLORER, kind, value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_sol_amounts_solscan_links_and_escapes() {
        let mint = Pubkey::new_unique();
        let sig = Signature::from([1u8; 64]);
        let text = SolNotification::Simulated {
            signature: sig,
            side: TradeSide::Buy,
            mint,
            amount_in: 100_000_000,
            min_out: 42,
        }
        .render(true);
        assert!(text.starts_with("🧪 <b>[DRY RUN]</b>"));
        assert!(text.contains("In: 0.100000000 SOL"));
        assert!(text.contains("Min out: 42 (raw token units)"));
        assert!(text.contains(&format!("{EXPLORER}/tx/{sig}")));
        assert!(text.contains(&format!("{EXPLORER}/token/{mint}")));

        let failed = SolNotification::Failed {
            signature: sig,
            side: TradeSide::Sell,
            mint,
            stage: "simulate",
            reason: "custom <6003> & more".into(),
            tx: None,
        }
        .render(false);
        assert!(failed.contains("custom &lt;6003&gt; &amp; more"));
    }
}
