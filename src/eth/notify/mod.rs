//! ETH notification messages. Delivery (queue, retries, Telegram client) lives in
//! [`crate::common::telegram`].

use std::fmt;

use alloy::primitives::utils::format_ether;
use alloy::primitives::{Address, B256, U256};

use crate::common::telegram::{self, Render};
pub use crate::common::telegram::{
    MessageSink, NotifyError, TelegramConfig, TelegramSink, escape, spawn,
};
use crate::eth::models::{Dex, TradeSide};

const EXPLORER: &str = "https://etherscan.io";

pub type Notifier = telegram::Notifier<Notification>;

#[derive(Debug, Clone)]
pub enum Notification {
    /// Decision stage approved copying a source trade.
    Signal {
        source_wallet: Address,
        source_tx: B256,
        dex: Dex,
        side: TradeSide,
        token: Address,
        amount_in: U256,
        capped: bool,
    },
    /// DRY_RUN only: the copy simulated fine and would have been sent.
    Simulated {
        source_tx: B256,
        side: TradeSide,
        token: Address,
        amount_in: U256,
        min_out: U256,
    },
    Broadcasted {
        source_tx: B256,
        side: TradeSide,
        token: Address,
        amount_in: U256,
        min_out: U256,
        /// In nonce order; the swap is last.
        txs: Vec<(&'static str, B256)>,
    },
    Confirmed {
        source_tx: B256,
        side: TradeSide,
        token: Address,
        tx: Option<B256>,
        block: Option<u64>,
    },
    Failed {
        source_tx: B256,
        side: TradeSide,
        token: Address,
        stage: &'static str,
        reason: String,
        tx: Option<B256>,
    },
}

impl Render for Notification {
    /// Telegram HTML. Everything not produced here (revert reasons) goes through `escape`.
    fn render(&self, dry_run: bool) -> String {
        let mut out = String::new();
        if dry_run {
            out.push_str("🧪 <b>[DRY RUN]</b> ");
        }
        match self {
            Notification::Signal {
                source_wallet,
                source_tx,
                dex,
                side,
                token,
                amount_in,
                capped,
            } => {
                out.push_str(&format!("📡 <b>Copy signal</b> · {}\n", side_str(*side)));
                out.push_str(&format!("Wallet: {}\n", link("address", source_wallet)));
                out.push_str(&format!("Source tx: {}\n", link("tx", source_tx)));
                out.push_str(&format!("DEX: {dex}\n"));
                out.push_str(&format!("Token: {}\n", link("token", token)));
                out.push_str(&format!(
                    "Size: {}{}",
                    amount(*side, *amount_in, true),
                    if *capped {
                        " (capped to MAX_TRADE_ETH)"
                    } else {
                        ""
                    }
                ));
            }
            Notification::Simulated {
                source_tx,
                side,
                token,
                amount_in,
                min_out,
            } => {
                out.push_str(&format!("✅ <b>Copy simulated</b> · {}\n", side_str(*side)));
                out.push_str(&format!("Source tx: {}\n", link("tx", source_tx)));
                out.push_str(&format!("Token: {}\n", link("token", token)));
                out.push_str(&format!("In: {}\n", amount(*side, *amount_in, true)));
                out.push_str(&format!("Min out: {}", amount(*side, *min_out, false)));
            }
            Notification::Broadcasted {
                source_tx,
                side,
                token,
                amount_in,
                min_out,
                txs,
            } => {
                out.push_str(&format!("🚀 <b>Copy sent</b> · {}\n", side_str(*side)));
                out.push_str(&format!("Source tx: {}\n", link("tx", source_tx)));
                out.push_str(&format!("Token: {}\n", link("token", token)));
                out.push_str(&format!("In: {}\n", amount(*side, *amount_in, true)));
                out.push_str(&format!("Min out: {}", amount(*side, *min_out, false)));
                for (step, hash) in txs {
                    out.push_str(&format!("\n{}: {}", escape(step), link("tx", hash)));
                }
            }
            Notification::Confirmed {
                source_tx,
                side,
                token,
                tx,
                block,
            } => {
                out.push_str(&format!("🎯 <b>Copy confirmed</b> · {}\n", side_str(*side)));
                out.push_str(&format!("Source tx: {}\n", link("tx", source_tx)));
                out.push_str(&format!("Token: {}\n", link("token", token)));
                if let Some(tx) = tx {
                    out.push_str(&format!("Copy tx: {}\n", link("tx", tx)));
                }
                out.push_str(&format!(
                    "Block: {}",
                    block.map_or_else(|| "?".to_string(), |b| b.to_string())
                ));
            }
            Notification::Failed {
                source_tx,
                side,
                token,
                stage,
                reason,
                tx,
            } => {
                out.push_str(&format!("❌ <b>Copy failed</b> · {}\n", side_str(*side)));
                out.push_str(&format!("Source tx: {}\n", link("tx", source_tx)));
                out.push_str(&format!("Token: {}\n", link("token", token)));
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

/// The ETH leg is shown in ETH; the token leg in raw units, because token decimals are
/// not fetched (it would cost an RPC call on the trading path for a cosmetic gain).
fn amount(side: TradeSide, value: U256, is_input: bool) -> String {
    let eth_leg = (side == TradeSide::Buy) == is_input;
    if eth_leg {
        format!("{} ETH", format_ether(value))
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

    fn failed(reason: &str) -> Notification {
        Notification::Failed {
            source_tx: B256::repeat_byte(0xaa),
            side: TradeSide::Sell,
            token: Address::repeat_byte(0x11),
            stage: "plan",
            reason: reason.to_string(),
            tx: None,
        }
    }

    #[test]
    fn render_links_and_escapes() {
        let text = failed("revert <Foo> & bar").render(true);
        assert!(text.starts_with("🧪 <b>[DRY RUN]</b>"));
        assert!(text.contains(&format!("{EXPLORER}/tx/0x{}", "aa".repeat(32))));
        assert!(text.contains("revert &lt;Foo&gt; &amp; bar"));
        assert!(!text.contains("<Foo>"));
    }

    #[test]
    fn render_amount_units_follow_side() {
        let n = Notification::Broadcasted {
            source_tx: B256::ZERO,
            side: TradeSide::Buy,
            token: Address::repeat_byte(0x11),
            amount_in: U256::from(50_000_000_000_000_000u64),
            min_out: U256::from(123u64),
            txs: vec![("swap", B256::repeat_byte(0xbb))],
        };
        let text = n.render(false);
        assert!(text.contains("In: 0.050000000000000000 ETH"));
        assert!(text.contains("Min out: 123 (raw token units)"));
        assert!(text.contains(&format!(
            "swap: <a href=\"{EXPLORER}/tx/0x{}",
            "bb".repeat(32)
        )));
    }
}
