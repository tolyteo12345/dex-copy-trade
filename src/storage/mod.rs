use std::future::Future;
use std::time::{Duration, Instant};

use alloy::primitives::B256;
use dashmap::DashMap;
use dashmap::mapref::entry::Entry;
use parking_lot::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DedupKey {
    pub chain_id: u64,
    pub tx_hash: B256,
}

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("storage backend error: {0}")]
    Backend(String),
}

/// Idempotency store: one source tx is processed at most once.
/// Persistent backends (Redis `SET NX`, Postgres unique insert) implement the same contract.
pub trait DedupStore: Send + Sync {
    /// Atomically records `key`; returns `false` if it was already recorded.
    fn mark_if_new(&self, key: DedupKey)
    -> impl Future<Output = Result<bool, StorageError>> + Send;
}

/// Entries older than this are forgotten. Long enough that a tx re-broadcast or re-mined
/// (reorg) is still recognised; memory stays tiny because only watched wallets get here.
pub const DEDUP_TTL: Duration = Duration::from_secs(24 * 3600);
const SWEEP_EVERY: Duration = Duration::from_secs(60);

/// Process-local store: state is lost on restart (accepted for V1).
pub struct InMemoryDedup {
    seen: DashMap<DedupKey, Instant>,
    ttl: Duration,
    last_sweep: Mutex<Instant>,
}

impl InMemoryDedup {
    pub fn new(ttl: Duration) -> Self {
        Self {
            seen: DashMap::new(),
            ttl,
            last_sweep: Mutex::new(Instant::now()),
        }
    }

    pub fn len(&self) -> usize {
        self.seen.len()
    }

    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }

    fn mark_at(&self, key: DedupKey, now: Instant) -> bool {
        self.sweep_if_due(now);
        match self.seen.entry(key) {
            Entry::Occupied(mut e) => {
                if now.duration_since(*e.get()) < self.ttl {
                    false
                } else {
                    e.insert(now);
                    true
                }
            }
            Entry::Vacant(e) => {
                e.insert(now);
                true
            }
        }
    }

    fn sweep_if_due(&self, now: Instant) {
        {
            let mut last = self.last_sweep.lock();
            if now.duration_since(*last) < SWEEP_EVERY {
                return;
            }
            *last = now;
        }
        self.seen.retain(|_, at| now.duration_since(*at) < self.ttl);
    }
}

impl DedupStore for InMemoryDedup {
    async fn mark_if_new(&self, key: DedupKey) -> Result<bool, StorageError> {
        Ok(self.mark_at(key, Instant::now()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn key(n: u8) -> DedupKey {
        DedupKey {
            chain_id: 1,
            tx_hash: B256::repeat_byte(n),
        }
    }

    #[tokio::test]
    async fn second_mark_is_duplicate() {
        let store = InMemoryDedup::new(DEDUP_TTL);
        assert!(store.mark_if_new(key(1)).await.unwrap());
        assert!(!store.mark_if_new(key(1)).await.unwrap());
        assert!(store.mark_if_new(key(2)).await.unwrap());
    }

    #[test]
    fn chain_id_is_part_of_key() {
        let store = InMemoryDedup::new(DEDUP_TTL);
        let now = Instant::now();
        assert!(store.mark_at(key(1), now));
        assert!(store.mark_at(
            DedupKey {
                chain_id: 10,
                ..key(1)
            },
            now
        ));
    }

    #[test]
    fn entries_expire_after_ttl_and_are_swept() {
        let ttl = Duration::from_secs(10);
        let store = InMemoryDedup::new(ttl);
        let t0 = Instant::now();
        assert!(store.mark_at(key(1), t0));
        assert!(!store.mark_at(key(1), t0 + Duration::from_secs(5)));
        assert!(
            store.mark_at(key(1), t0 + Duration::from_secs(11)),
            "expired => new again"
        );

        assert!(store.mark_at(key(2), t0 + Duration::from_secs(11)));
        // Sweep runs at most every SWEEP_EVERY; both entries are past TTL by then.
        let later = t0 + SWEEP_EVERY + Duration::from_secs(30);
        assert!(store.mark_at(key(3), later));
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn concurrent_marks_admit_exactly_one() {
        let store = Arc::new(InMemoryDedup::new(DEDUP_TTL));
        let now = Instant::now();
        let handles: Vec<_> = (0..16)
            .map(|_| {
                let s = Arc::clone(&store);
                std::thread::spawn(move || s.mark_at(key(7), now))
            })
            .collect();
        let admitted = handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .filter(|&ok| ok)
            .count();
        assert_eq!(admitted, 1);
    }
}
