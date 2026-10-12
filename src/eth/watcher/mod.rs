mod blocks;
mod pending;

use std::collections::HashSet;
use std::time::{Duration, Instant};

use alloy::network::TransactionResponse;
use alloy::primitives::Address;
use alloy::rpc::types::Transaction;
use tokio::sync::{mpsc, watch};

use crate::common::backoff::Backoff;
use crate::eth::config::CopyMode;
use crate::eth::ethereum::provider::{self, ProviderError};

pub use blocks::BlockCursor;

const BACKOFF_INITIAL: Duration = Duration::from_millis(500);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// No item on the subscription for this long => treat the socket as dead and reconnect.
/// Mainnet produces a block every ~12s and pending txs continuously, so 60s is a safe floor.
const STALL_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxOrigin {
    Pending,
    Block { number: u64 },
}

/// A transaction sent by one of the watched wallets, as first seen by the bot.
#[derive(Debug, Clone)]
pub struct WatchedTx {
    pub tx: Transaction,
    pub origin: TxOrigin,
    pub detected_at: Instant,
}

#[derive(Debug, Clone)]
pub struct WalletFilter {
    wallets: HashSet<Address>,
}

impl WalletFilter {
    pub fn new(wallets: impl IntoIterator<Item = Address>) -> Self {
        Self {
            wallets: wallets.into_iter().collect(),
        }
    }

    pub fn matches(&self, from: &Address) -> bool {
        self.wallets.contains(from)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum WatchError {
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error("block {0} not returned by node")]
    MissingBlock(u64),
}

impl From<alloy::transports::TransportError> for WatchError {
    fn from(err: alloy::transports::TransportError) -> Self {
        WatchError::Provider(err.into())
    }
}

/// Why a single WS session ended without an error.
#[derive(Debug, PartialEq, Eq)]
enum SessionEnd {
    Shutdown,
    StreamClosed,
    Stalled,
}

pub struct Watcher {
    mode: CopyMode,
    ws_url: String,
    filter: WalletFilter,
    out: mpsc::Sender<WatchedTx>,
    shutdown: watch::Receiver<bool>,
}

impl Watcher {
    pub fn new(
        mode: CopyMode,
        ws_url: String,
        filter: WalletFilter,
        out: mpsc::Sender<WatchedTx>,
        shutdown: watch::Receiver<bool>,
    ) -> Self {
        Self {
            mode,
            ws_url,
            filter,
            out,
            shutdown,
        }
    }

    /// Runs until shutdown. A failure before the first subscription is established is
    /// returned (misconfiguration, e.g. RPC without pending-tx support); later failures
    /// are retried with exponential backoff.
    pub async fn run(mut self) -> Result<(), WatchError> {
        let mut backoff = Backoff::new(BACKOFF_INITIAL, BACKOFF_MAX);
        let mut established_once = false;
        // Lives across reconnects so confirmed mode can backfill blocks missed while down.
        let mut cursor = BlockCursor::default();
        let endpoint = provider::redact_url(&self.ws_url);

        loop {
            if *self.shutdown.borrow() {
                return Ok(());
            }
            let result = match self.mode {
                CopyMode::Pending => {
                    pending::run_session(&mut self, &mut established_once, &mut backoff).await
                }
                CopyMode::Confirmed => {
                    blocks::run_session(&mut self, &mut cursor, &mut established_once, &mut backoff)
                        .await
                }
            };
            match result {
                Ok(SessionEnd::Shutdown) => return Ok(()),
                Ok(end) => tracing::warn!(event = "watcher_disconnected", reason = ?end, %endpoint),
                Err(err) if !established_once => return Err(err),
                Err(err) => tracing::warn!(event = "watcher_disconnected", error = %err, %endpoint),
            }

            let delay = backoff.next_delay();
            tracing::info!(
                event = "watcher_reconnecting",
                delay_ms = delay.as_millis() as u64
            );
            tokio::select! {
                _ = self.shutdown.changed() => return Ok(()),
                _ = tokio::time::sleep(delay) => {}
            }
        }
    }

    /// Forwards a matching tx downstream without ever blocking the watcher.
    /// Returns `false` when the downstream channel is closed (pipeline shutting down).
    fn emit(&self, tx: Transaction, origin: TxOrigin, detected_at: Instant) -> bool {
        let from = tx.from();
        if !self.filter.matches(&from) {
            return true;
        }
        let tx_hash = tx.tx_hash();
        tracing::info!(event = "wallet_tx_detected", source_wallet = %from, source_tx = %tx_hash, ?origin);
        match self.out.try_send(WatchedTx {
            tx,
            origin,
            detected_at,
        }) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                tracing::error!(
                    event = "wallet_tx_dropped",
                    source_wallet = %from,
                    source_tx = %tx_hash,
                    reason = "downstream channel full"
                );
                true
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wallet_filter_matches_only_configured() {
        let a: Address = "0x1111111111111111111111111111111111111111"
            .parse()
            .unwrap();
        let b: Address = "0x2222222222222222222222222222222222222222"
            .parse()
            .unwrap();
        let filter = WalletFilter::new([a]);
        assert!(filter.matches(&a));
        assert!(!filter.matches(&b));
    }
}
