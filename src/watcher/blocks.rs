use std::ops::RangeInclusive;
use std::time::{Duration, Instant};

use alloy::eips::BlockNumberOrTag;
use alloy::providers::{DynProvider, Provider};
use futures::StreamExt;

use super::{STALL_TIMEOUT, SessionEnd, TxOrigin, WatchError, Watcher};
use crate::ethereum::provider;
use crate::utils::Backoff;

/// Max blocks re-scanned after a reconnect. Older gaps are skipped with a warning:
/// copying trades that are minutes old is worse than missing them.
pub const MAX_BACKFILL_BLOCKS: u64 = 32;

const BLOCK_FETCH_ATTEMPTS: u32 = 5;
const BLOCK_FETCH_RETRY_DELAY: Duration = Duration::from_millis(250);

/// Tracks the last fully processed block so a reconnect can backfill the gap.
#[derive(Debug, Default)]
pub struct BlockCursor {
    last_processed: Option<u64>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ScanPlan {
    pub range: RangeInclusive<u64>,
    pub skipped: u64,
}

impl BlockCursor {
    /// Blocks to scan when `head` arrives. A head at or below the cursor (reorg) rescans
    /// just that height; duplicate emissions are absorbed by downstream dedup.
    pub fn plan(&self, head: u64) -> ScanPlan {
        match self.last_processed {
            Some(last) if head > last => {
                let wanted_from = last + 1;
                let from = wanted_from.max(head.saturating_sub(MAX_BACKFILL_BLOCKS - 1));
                ScanPlan {
                    range: from..=head,
                    skipped: from - wanted_from,
                }
            }
            _ => ScanPlan {
                range: head..=head,
                skipped: 0,
            },
        }
    }

    pub fn mark_processed(&mut self, number: u64) {
        self.last_processed = Some(number);
    }
}

pub(super) async fn run_session(
    watcher: &mut Watcher,
    cursor: &mut BlockCursor,
    established_once: &mut bool,
    backoff: &mut Backoff,
) -> Result<SessionEnd, WatchError> {
    let provider = provider::connect_ws(&watcher.ws_url).await?;
    let sub = provider.subscribe_blocks().await?;
    let mut stream = sub.into_stream();
    *established_once = true;
    backoff.reset();
    tracing::info!(event = "watcher_subscribed", mode = "confirmed");

    loop {
        tokio::select! {
            biased;
            _ = watcher.shutdown.changed() => return Ok(SessionEnd::Shutdown),
            item = tokio::time::timeout(STALL_TIMEOUT, stream.next()) => match item {
                Err(_) => return Ok(SessionEnd::Stalled),
                Ok(None) => return Ok(SessionEnd::StreamClosed),
                Ok(Some(header)) => {
                    if !scan_up_to(watcher, &provider, cursor, header.number).await? {
                        return Ok(SessionEnd::Shutdown);
                    }
                }
            },
        }
    }
}

/// On error the cursor is left at the last fully scanned block, so the next session
/// retries from there.
async fn scan_up_to(
    watcher: &Watcher,
    provider: &DynProvider,
    cursor: &mut BlockCursor,
    head: u64,
) -> Result<bool, WatchError> {
    let plan = cursor.plan(head);
    if plan.skipped > 0 {
        tracing::warn!(
            event = "blocks_skipped",
            skipped = plan.skipped,
            resume_from = *plan.range.start(),
            "gap larger than MAX_BACKFILL_BLOCKS"
        );
    }
    for number in plan.range {
        let block = fetch_block(provider, number).await?;
        let detected_at = Instant::now();
        for tx in block.transactions.into_transactions() {
            if !watcher.emit(tx, TxOrigin::Block { number }, detected_at) {
                return Ok(false);
            }
        }
        cursor.mark_processed(number);
    }
    Ok(true)
}

/// Load-balanced RPCs can announce a head before the backend serving the follow-up
/// request has it (observed on publicnode), so `None` is retried briefly before the
/// session is torn down.
async fn fetch_block(
    provider: &DynProvider,
    number: u64,
) -> Result<alloy::rpc::types::Block, WatchError> {
    for _ in 0..BLOCK_FETCH_ATTEMPTS {
        if let Some(block) = provider
            .get_block_by_number(BlockNumberOrTag::Number(number))
            .full()
            .await?
        {
            return Ok(block);
        }
        tokio::time::sleep(BLOCK_FETCH_RETRY_DELAY).await;
    }
    Err(WatchError::MissingBlock(number))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_head_scans_only_head() {
        let c = BlockCursor::default();
        assert_eq!(
            c.plan(100),
            ScanPlan {
                range: 100..=100,
                skipped: 0
            }
        );
    }

    #[test]
    fn contiguous_and_small_gap_are_fully_scanned() {
        let mut c = BlockCursor::default();
        c.mark_processed(100);
        assert_eq!(
            c.plan(101),
            ScanPlan {
                range: 101..=101,
                skipped: 0
            }
        );
        assert_eq!(
            c.plan(105),
            ScanPlan {
                range: 101..=105,
                skipped: 0
            }
        );
    }

    #[test]
    fn large_gap_is_capped() {
        let mut c = BlockCursor::default();
        c.mark_processed(100);
        let plan = c.plan(200);
        assert_eq!(plan.range, (200 - MAX_BACKFILL_BLOCKS + 1)..=200);
        assert_eq!(plan.skipped, 200 - MAX_BACKFILL_BLOCKS - 100);
        assert_eq!(plan.range.clone().count() as u64, MAX_BACKFILL_BLOCKS);
    }

    #[test]
    fn reorg_rescans_head_only() {
        let mut c = BlockCursor::default();
        c.mark_processed(100);
        assert_eq!(
            c.plan(100),
            ScanPlan {
                range: 100..=100,
                skipped: 0
            }
        );
        assert_eq!(
            c.plan(99),
            ScanPlan {
                range: 99..=99,
                skipped: 0
            }
        );
    }
}
