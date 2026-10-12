use std::future::Future;

use alloy::primitives::B256;

pub use crate::common::dedup::DEDUP_TTL;
use crate::common::dedup::TtlSet;

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

/// Process-local store: state is lost on restart (accepted for V1).
pub type InMemoryDedup = TtlSet<DedupKey>;

impl DedupStore for InMemoryDedup {
    async fn mark_if_new(&self, key: DedupKey) -> Result<bool, StorageError> {
        Ok(TtlSet::mark_if_new(self, key))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    fn key(n: u8) -> DedupKey {
        DedupKey {
            chain_id: 1,
            tx_hash: B256::repeat_byte(n),
        }
    }

    #[tokio::test]
    async fn second_mark_is_duplicate() {
        let store = InMemoryDedup::new(DEDUP_TTL);
        assert!(DedupStore::mark_if_new(&store, key(1)).await.unwrap());
        assert!(!DedupStore::mark_if_new(&store, key(1)).await.unwrap());
        assert!(DedupStore::mark_if_new(&store, key(2)).await.unwrap());
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
}
