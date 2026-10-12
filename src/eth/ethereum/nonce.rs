//! Local nonce manager for the bot wallet.
//!
//! A lease holds the manager's lock from nonce reservation until the txs are handed to the
//! node, so concurrent executions can never share a nonce *or* leave a gap (a gap would
//! block every later tx). Signing is local (µs), so only the `eth_sendRawTransaction`
//! round-trip is serialised; quoting/simulation stay concurrent.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;

use alloy::primitives::B256;
use tokio::sync::{Mutex, OwnedMutexGuard};

#[derive(Debug, Default)]
struct State {
    /// Next nonce to hand out; `None` => read the chain's pending count on next lease.
    next: Option<u64>,
    /// Sent by us, not yet seen confirmed.
    pending: BTreeMap<u64, B256>,
}

#[derive(Debug, Clone, Default)]
pub struct NonceManager {
    state: Arc<Mutex<State>>,
}

/// Exclusive right to nonces `base..base + count` until `finish` (or drop).
pub struct NonceLease {
    guard: OwnedMutexGuard<State>,
    base: u64,
    count: u64,
}

impl NonceManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Reserves `count` consecutive nonces. `chain_pending_nonce` is only awaited when the
    /// manager is unsynced (startup, or after a send error / receipt timeout).
    pub async fn lease<F, Fut, E>(
        &self,
        count: u64,
        chain_pending_nonce: F,
    ) -> Result<NonceLease, E>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<u64, E>>,
    {
        let mut guard = Arc::clone(&self.state).lock_owned().await;
        let base = match guard.next {
            Some(n) => n,
            None => {
                let n = chain_pending_nonce().await?;
                guard.next = Some(n);
                n
            }
        };
        Ok(NonceLease { guard, base, count })
    }

    /// The tx with `nonce` was mined (any status): it and all lower nonces are final.
    pub async fn confirmed(&self, nonce: u64) {
        let mut s = self.state.lock().await;
        s.pending.retain(|n, _| *n > nonce);
    }

    /// Forces a re-read from the chain on the next lease (e.g. a tx may be stuck/dropped).
    pub async fn resync(&self) {
        self.state.lock().await.next = None;
    }

    pub async fn pending_count(&self) -> usize {
        self.state.lock().await.pending.len()
    }
}

impl NonceLease {
    pub fn nonce(&self, index: u64) -> u64 {
        debug_assert!(index < self.count, "nonce index out of lease");
        self.base + index
    }

    /// Records the txs the node accepted, in nonce order (a prefix of the lease). Unused
    /// nonces are returned. `send_failed` resyncs from chain next time, because a failed
    /// send may still have reached the mempool (e.g. timeout after acceptance).
    pub fn finish(mut self, accepted: &[B256], send_failed: bool) {
        for (i, hash) in accepted.iter().enumerate() {
            self.guard.pending.insert(self.base + i as u64, *hash);
        }
        self.guard.next = if send_failed {
            None
        } else {
            Some(self.base + accepted.len() as u64)
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::convert::Infallible;

    async fn chain(n: u64) -> Result<u64, Infallible> {
        Ok(n)
    }

    #[tokio::test]
    async fn first_lease_syncs_from_chain_then_counts_locally() {
        let m = NonceManager::new();
        let lease = m.lease(2, || chain(10)).await.unwrap();
        assert_eq!((lease.nonce(0), lease.nonce(1)), (10, 11));
        lease.finish(&[B256::repeat_byte(1), B256::repeat_byte(2)], false);
        // Chain value is not consulted again while synced.
        let lease = m.lease(1, || chain(999)).await.unwrap();
        assert_eq!(lease.nonce(0), 12);
        drop(lease);
        assert_eq!(m.pending_count().await, 2);
    }

    #[tokio::test]
    async fn dropped_lease_consumes_nothing() {
        let m = NonceManager::new();
        drop(m.lease(2, || chain(5)).await.unwrap());
        let lease = m.lease(1, || chain(999)).await.unwrap();
        assert_eq!(lease.nonce(0), 5);
    }

    #[tokio::test]
    async fn partial_send_keeps_accepted_and_resyncs() {
        let m = NonceManager::new();
        let lease = m.lease(2, || chain(5)).await.unwrap();
        // approve (nonce 5) accepted, swap (6) rejected
        lease.finish(&[B256::repeat_byte(1)], true);
        assert_eq!(m.pending_count().await, 1);
        let lease = m.lease(1, || chain(6)).await.unwrap();
        assert_eq!(lease.nonce(0), 6, "resynced from chain pending count");
    }

    #[tokio::test]
    async fn confirmation_clears_pending_and_resync_rereads_chain() {
        let m = NonceManager::new();
        let lease = m.lease(3, || chain(0)).await.unwrap();
        lease.finish(&[B256::ZERO, B256::ZERO, B256::ZERO], false);
        m.confirmed(1).await;
        assert_eq!(m.pending_count().await, 1);
        m.resync().await;
        let lease = m.lease(1, || chain(42)).await.unwrap();
        assert_eq!(lease.nonce(0), 42);
    }

    #[tokio::test]
    async fn sync_errors_propagate_and_leave_manager_unsynced() {
        let m = NonceManager::new();
        let err = m.lease(1, || async { Err::<u64, _>("rpc down") }).await;
        assert!(err.is_err());
        assert_eq!(m.lease(1, || chain(3)).await.unwrap().nonce(0), 3);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn concurrent_leases_never_share_or_skip_nonces() {
        let m = NonceManager::new();
        let mut tasks = Vec::new();
        for i in 0..64u64 {
            let m = m.clone();
            tasks.push(tokio::spawn(async move {
                let count = 1 + i % 2; // mix of single swaps and approve+swap pairs
                let lease = m.lease(count, || chain(100)).await.unwrap();
                let nonces: Vec<u64> = (0..count).map(|j| lease.nonce(j)).collect();
                tokio::task::yield_now().await;
                lease.finish(&vec![B256::ZERO; count as usize], false);
                nonces
            }));
        }
        let mut all = Vec::new();
        for t in tasks {
            all.extend(t.await.unwrap());
        }
        let unique: HashSet<_> = all.iter().copied().collect();
        assert_eq!(unique.len(), all.len(), "no nonce handed out twice");
        let max = *all.iter().max().unwrap();
        assert_eq!(
            max - 100 + 1,
            all.len() as u64,
            "contiguous from 100: no gaps"
        );
    }
}
