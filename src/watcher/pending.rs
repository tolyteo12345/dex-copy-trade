use std::time::Instant;

use alloy::providers::Provider;
use futures::StreamExt;

use super::{STALL_TIMEOUT, SessionEnd, TxOrigin, WatchError, Watcher};
use crate::ethereum::provider;
use crate::utils::Backoff;

/// Subscribes to full pending tx bodies (`newPendingTransactions` with `true`).
/// Hash-only subscription + per-hash fetch is deliberately not used: it costs one RPC
/// round-trip per mempool tx (hundreds/s on mainnet) and adds latency to every match.
pub(super) async fn run_session(
    watcher: &mut Watcher,
    established_once: &mut bool,
    backoff: &mut Backoff,
) -> Result<SessionEnd, WatchError> {
    let provider = provider::connect_ws(&watcher.ws_url).await?;
    let sub = provider.subscribe_full_pending_transactions().await?;
    let mut stream = sub.into_stream();
    *established_once = true;
    backoff.reset();
    tracing::info!(event = "watcher_subscribed", mode = "pending");

    loop {
        tokio::select! {
            biased;
            _ = watcher.shutdown.changed() => return Ok(SessionEnd::Shutdown),
            item = tokio::time::timeout(STALL_TIMEOUT, stream.next()) => match item {
                Err(_) => return Ok(SessionEnd::Stalled),
                Ok(None) => return Ok(SessionEnd::StreamClosed),
                Ok(Some(tx)) => {
                    if !watcher.emit(tx, TxOrigin::Pending, Instant::now()) {
                        return Ok(SessionEnd::Shutdown);
                    }
                }
            },
        }
    }
}
