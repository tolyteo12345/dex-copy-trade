//! Telegram notifications, kept off the trading path.
//!
//! Stages call [`Notifier::notify`], which only does a non-blocking `try_send` into a bounded
//! queue. A background task formats and delivers the messages, so a slow or unreachable
//! Telegram API can never delay a trade. When the queue is full new messages are dropped
//! (and logged) instead of applying backpressure to the pipeline.

use std::fmt;
use std::future::Future;
use std::time::Duration;

use alloy::primitives::utils::format_ether;
use alloy::primitives::{Address, B256, U256};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::models::{Dex, TradeSide};
use crate::utils::Backoff;

/// Messages waiting for delivery. Telegram allows ~1 msg/s per chat, so a burst larger
/// than this is already minutes behind; dropping beats an ever-growing backlog.
const QUEUE_CAPACITY: usize = 256;
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_ATTEMPTS: u32 = 3;
/// Upper bound on a server-requested `retry_after`, so one message cannot stall the queue.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(30);
const EXPLORER: &str = "https://etherscan.io";

#[derive(Debug, thiserror::Error)]
pub enum NotifyError {
    #[error("telegram request failed: {0}")]
    Http(String),
    #[error("telegram rate limited (retry after {0:?})")]
    RateLimited(Duration),
    #[error("telegram rejected the message: {0}")]
    Api(String),
}

/// `TELEGRAM_BOT_TOKEN` grants full control of the bot: `Debug` only shows the chat id.
#[derive(Clone)]
pub struct TelegramConfig {
    pub bot_token: String,
    pub chat_id: String,
}

impl fmt::Debug for TelegramConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TelegramConfig")
            .field("bot_token", &"<redacted>")
            .field("chat_id", &self.chat_id)
            .finish()
    }
}

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

impl Notification {
    /// Telegram HTML. Everything not produced here (revert reasons) goes through `escape`.
    pub fn render(&self, dry_run: bool) -> String {
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
    let full = value.to_string();
    let short = if full.len() > 14 {
        format!("{}…{}", &full[..8], &full[full.len() - 6..])
    } else {
        full.clone()
    };
    format!("<a href=\"{EXPLORER}/{kind}/{full}\">{short}</a>")
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Cheap to clone; a disabled notifier is a no-op.
#[derive(Debug, Clone, Default)]
pub struct Notifier {
    tx: Option<mpsc::Sender<Notification>>,
}

impl Notifier {
    pub fn disabled() -> Self {
        Self::default()
    }

    /// Never awaits: safe to call from the hot path.
    pub fn notify(&self, n: Notification) {
        let Some(tx) = &self.tx else { return };
        if let Err(mpsc::error::TrySendError::Full(_)) = tx.try_send(n) {
            tracing::warn!(event = "notification_dropped", reason = "queue full");
        }
    }
}

pub trait MessageSink: Send + Sync + 'static {
    fn send(&self, text: &str) -> impl Future<Output = Result<(), NotifyError>> + Send;
}

/// The request URL embeds the bot token: errors are built with `without_url()` and the
/// struct does not implement `Debug`.
pub struct TelegramSink {
    client: reqwest::Client,
    url: String,
    chat_id: String,
}

impl TelegramSink {
    pub fn new(cfg: &TelegramConfig) -> Result<Self, NotifyError> {
        let client = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .map_err(|e| NotifyError::Http(e.without_url().to_string()))?;
        Ok(Self {
            client,
            url: format!("https://api.telegram.org/bot{}/sendMessage", cfg.bot_token),
            chat_id: cfg.chat_id.clone(),
        })
    }
}

impl MessageSink for TelegramSink {
    async fn send(&self, text: &str) -> Result<(), NotifyError> {
        let body = serde_json::json!({
            "chat_id": self.chat_id,
            "text": text,
            "parse_mode": "HTML",
            "disable_web_page_preview": true,
        });
        let resp = self
            .client
            .post(&self.url)
            .json(&body)
            .send()
            .await
            .map_err(|e| NotifyError::Http(e.without_url().to_string()))?;
        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        // Error bodies look like {"ok":false,"description":"...","parameters":{"retry_after":5}}.
        let json: serde_json::Value = resp.json().await.unwrap_or_default();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let secs = json["parameters"]["retry_after"].as_u64().unwrap_or(1);
            return Err(NotifyError::RateLimited(Duration::from_secs(secs)));
        }
        let description = json["description"].as_str().unwrap_or("no description");
        if status.is_server_error() {
            return Err(NotifyError::Http(format!("{status}: {description}")));
        }
        Err(NotifyError::Api(format!("{status}: {description}")))
    }
}

/// Starts the delivery task. It exits once every `Notifier` clone is dropped and the
/// queue is drained.
pub fn spawn<S: MessageSink>(sink: S, dry_run: bool) -> (Notifier, JoinHandle<()>) {
    let (tx, rx) = mpsc::channel(QUEUE_CAPACITY);
    let handle = tokio::spawn(run_worker(sink, rx, dry_run));
    (Notifier { tx: Some(tx) }, handle)
}

/// Delivers one message at a time on purpose: keeps messages in order and stays under
/// Telegram's per-chat rate limit without extra pacing logic.
async fn run_worker<S: MessageSink>(sink: S, mut rx: mpsc::Receiver<Notification>, dry_run: bool) {
    while let Some(n) = rx.recv().await {
        deliver(&sink, &n.render(dry_run)).await;
    }
}

async fn deliver<S: MessageSink>(sink: &S, text: &str) {
    let mut backoff = Backoff::new(Duration::from_secs(1), Duration::from_secs(8));
    for attempt in 1..=MAX_ATTEMPTS {
        let delay = match sink.send(text).await {
            Ok(()) => return,
            Err(NotifyError::RateLimited(after)) => after.min(MAX_RETRY_AFTER),
            Err(NotifyError::Http(_)) => backoff.next_delay(),
            Err(e @ NotifyError::Api(_)) => {
                tracing::warn!(event = "notification_failed", error = %e);
                return;
            }
        };
        if attempt == MAX_ATTEMPTS {
            tracing::warn!(
                event = "notification_failed",
                attempts = attempt,
                "giving up"
            );
            return;
        }
        tokio::time::sleep(delay).await;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use parking_lot::Mutex;

    use super::*;

    #[derive(Clone, Default)]
    struct MockSink {
        sent: Arc<Mutex<Vec<String>>>,
        /// Errors returned (in order) before succeeding.
        errors: Arc<Mutex<Vec<NotifyError>>>,
        calls: Arc<Mutex<u32>>,
    }

    impl MessageSink for MockSink {
        async fn send(&self, text: &str) -> Result<(), NotifyError> {
            *self.calls.lock() += 1;
            let next = {
                let mut errs = self.errors.lock();
                (!errs.is_empty()).then(|| errs.remove(0))
            };
            if let Some(e) = next {
                return Err(e);
            }
            self.sent.lock().push(text.to_string());
            Ok(())
        }
    }

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

    #[test]
    fn telegram_config_debug_hides_token() {
        let cfg = TelegramConfig {
            bot_token: "123456:SECRET-TOKEN".into(),
            chat_id: "-100".into(),
        };
        assert!(!format!("{cfg:?}").contains("SECRET"));
    }

    #[tokio::test]
    async fn disabled_notifier_is_noop() {
        Notifier::disabled().notify(failed("x"));
    }

    #[tokio::test]
    async fn full_queue_drops_without_blocking() {
        let (tx, mut rx) = mpsc::channel(1);
        let n = Notifier { tx: Some(tx) };
        n.notify(failed("first"));
        n.notify(failed("dropped"));
        drop(n);
        assert!(rx.recv().await.is_some());
        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn worker_delivers_in_order_and_exits_when_senders_drop() {
        let sink = MockSink::default();
        let (n, handle) = spawn(sink.clone(), false);
        n.notify(failed("one"));
        n.notify(failed("two"));
        drop(n);
        handle.await.unwrap();
        let sent = sink.sent.lock();
        assert_eq!(sent.len(), 2);
        assert!(sent[0].contains("one") && sent[1].contains("two"));
    }

    #[tokio::test(start_paused = true)]
    async fn retries_rate_limit_and_transport_errors() {
        let sink = MockSink::default();
        sink.errors.lock().extend([
            NotifyError::RateLimited(Duration::from_secs(5)),
            NotifyError::Http("timeout".into()),
        ]);
        deliver(&sink, "hi").await;
        assert_eq!(*sink.calls.lock(), 3);
        assert_eq!(sink.sent.lock().as_slice(), ["hi"]);
    }

    #[tokio::test(start_paused = true)]
    async fn api_rejection_is_not_retried() {
        let sink = MockSink::default();
        sink.errors
            .lock()
            .push(NotifyError::Api("400: chat not found".into()));
        deliver(&sink, "hi").await;
        assert_eq!(*sink.calls.lock(), 1);
        assert!(sink.sent.lock().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn gives_up_after_max_attempts() {
        let sink = MockSink::default();
        sink.errors
            .lock()
            .extend((0..5).map(|_| NotifyError::Http("down".into())));
        deliver(&sink, "hi").await;
        assert_eq!(*sink.calls.lock(), MAX_ATTEMPTS);
    }
}
