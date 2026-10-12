use std::hash::Hash;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use dashmap::mapref::entry::Entry;
use parking_lot::Mutex;

/// Entries older than this are forgotten. Long enough that a tx re-broadcast or re-mined
/// (reorg) is still recognised; memory stays tiny because only watched wallets get here.
pub const DEDUP_TTL: Duration = Duration::from_secs(24 * 3600);
const SWEEP_EVERY: Duration = Duration::from_secs(60);

/// Process-local "seen" set with expiry: state is lost on restart (accepted for V1).
/// Keyed by whatever identifies a source tx on the chain (ETH: chain id + hash, SOL:
/// signature).
pub struct TtlSet<K> {
    seen: DashMap<K, Instant>,
    ttl: Duration,
    last_sweep: Mutex<Instant>,
}

impl<K: Eq + Hash> TtlSet<K> {
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

    /// Atomically records `key`; returns `false` if it was already recorded (and not expired).
    pub fn mark_if_new(&self, key: K) -> bool {
        self.mark_at(key, Instant::now())
    }

    pub(crate) fn mark_at(&self, key: K, now: Instant) -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn second_mark_is_duplicate() {
        let store = TtlSet::new(DEDUP_TTL);
        assert!(store.mark_if_new(1u8));
        assert!(!store.mark_if_new(1u8));
        assert!(store.mark_if_new(2u8));
    }

    #[test]
    fn entries_expire_after_ttl_and_are_swept() {
        let ttl = Duration::from_secs(10);
        let store = TtlSet::new(ttl);
        let t0 = Instant::now();
        assert!(store.mark_at(1u8, t0));
        assert!(!store.mark_at(1, t0 + Duration::from_secs(5)));
        assert!(
            store.mark_at(1, t0 + Duration::from_secs(11)),
            "expired => new again"
        );

        assert!(store.mark_at(2, t0 + Duration::from_secs(11)));
        // Sweep runs at most every SWEEP_EVERY; both entries are past TTL by then.
        let later = t0 + SWEEP_EVERY + Duration::from_secs(30);
        assert!(store.mark_at(3, later));
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn concurrent_marks_admit_exactly_one() {
        let store = Arc::new(TtlSet::new(DEDUP_TTL));
        let now = Instant::now();
        let handles: Vec<_> = (0..16)
            .map(|_| {
                let s = Arc::clone(&store);
                std::thread::spawn(move || s.mark_at(7u8, now))
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
