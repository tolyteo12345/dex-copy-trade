//! EIP-1559 fees for the bot's own txs. The source tx's gas settings are never copied.

use std::future::Future;
use std::time::Duration;

use alloy::eips::BlockNumberOrTag;
use alloy::providers::{DynProvider, Provider};
use tokio::time::Instant;

use crate::eth::config::GasCapMode;
use crate::eth::ethereum::provider::ProviderError;

/// Re-check interval in wait mode (~1 mainnet block).
pub const WAIT_POLL: Duration = Duration::from_secs(12);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Eip1559Fees {
    pub max_fee_per_gas: u128,
    pub max_priority_fee_per_gas: u128,
}

#[derive(Debug, thiserror::Error)]
pub enum GasError {
    #[error("base fee + tip = {needed} wei exceeds MAX_GAS_GWEI ({cap} wei)")]
    OverCap { needed: u128, cap: u128 },
    #[error("gas still above MAX_GAS_GWEI after waiting {waited_s}s ({needed} > {cap} wei)")]
    WaitExpired {
        needed: u128,
        cap: u128,
        waited_s: u64,
    },
    #[error("fee data unavailable: {0}")]
    Provider(#[from] ProviderError),
}

pub trait GasStrategy: Send + Sync {
    fn fees(&self) -> impl Future<Output = Result<Eip1559Fees, GasError>> + Send;
}

/// Current base fee and suggested priority fee (wei).
pub trait FeeSource: Send + Sync {
    fn base_fee_and_tip(&self) -> impl Future<Output = Result<(u128, u128), ProviderError>> + Send;
}

pub struct RpcFeeSource {
    provider: DynProvider,
}

impl RpcFeeSource {
    pub fn new(provider: DynProvider) -> Self {
        Self { provider }
    }
}

impl FeeSource for RpcFeeSource {
    async fn base_fee_and_tip(&self) -> Result<(u128, u128), ProviderError> {
        let (block, tip) = tokio::try_join!(
            async {
                Ok::<_, ProviderError>(
                    self.provider
                        .get_block_by_number(BlockNumberOrTag::Latest)
                        .await?,
                )
            },
            async { Ok(self.provider.get_max_priority_fee_per_gas().await?) },
        )?;
        let base = block
            .and_then(|b| b.header.base_fee_per_gas)
            .ok_or_else(|| ProviderError::BadResponse("latest block has no base fee".into()))?;
        Ok((u128::from(base), tip))
    }
}

/// `maxFee = min(2 * base + tip, cap)`: 2x base survives ~6 full blocks of base-fee growth,
/// and the cap bounds the worst case. Rejects when even `base + tip` is above the cap.
pub fn capped_fees(base_fee: u128, tip: u128, cap: u128) -> Result<Eip1559Fees, GasError> {
    let needed = base_fee.saturating_add(tip);
    if needed > cap {
        return Err(GasError::OverCap { needed, cap });
    }
    Ok(Eip1559Fees {
        max_fee_per_gas: base_fee.saturating_mul(2).saturating_add(tip).min(cap),
        max_priority_fee_per_gas: tip,
    })
}

pub struct CappedEip1559<F: FeeSource> {
    source: F,
    cap: u128,
    mode: GasCapMode,
    poll: Duration,
}

impl<F: FeeSource> CappedEip1559<F> {
    pub fn new(source: F, cap: u128, mode: GasCapMode) -> Self {
        Self {
            source,
            cap,
            mode,
            poll: WAIT_POLL,
        }
    }
}

impl<F: FeeSource> GasStrategy for CappedEip1559<F> {
    async fn fees(&self) -> Result<Eip1559Fees, GasError> {
        let started = Instant::now();
        loop {
            let (base, tip) = self.source.base_fee_and_tip().await?;
            match (capped_fees(base, tip, self.cap), self.mode) {
                (Ok(fees), _) => return Ok(fees),
                (Err(GasError::OverCap { needed, cap }), GasCapMode::Wait { max_wait }) => {
                    if started.elapsed() + self.poll > max_wait {
                        return Err(GasError::WaitExpired {
                            needed,
                            cap,
                            waited_s: started.elapsed().as_secs(),
                        });
                    }
                    tracing::info!(event = "gas_wait", needed, cap, "gas above cap; waiting");
                    tokio::time::sleep(self.poll).await;
                }
                (Err(e), _) => return Err(e),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use std::collections::VecDeque;

    const GWEI: u128 = 1_000_000_000;

    #[test]
    fn fee_math() {
        let f = capped_fees(10 * GWEI, GWEI, 100 * GWEI).unwrap();
        assert_eq!(f.max_fee_per_gas, 21 * GWEI);
        assert_eq!(f.max_priority_fee_per_gas, GWEI);
        // 2x headroom is clamped by the cap
        let f = capped_fees(60 * GWEI, GWEI, 100 * GWEI).unwrap();
        assert_eq!(f.max_fee_per_gas, 100 * GWEI);
        assert!(matches!(
            capped_fees(100 * GWEI, 1, 100 * GWEI),
            Err(GasError::OverCap { .. })
        ));
    }

    struct Scripted(Mutex<VecDeque<u128>>);

    impl FeeSource for Scripted {
        async fn base_fee_and_tip(&self) -> Result<(u128, u128), ProviderError> {
            let mut q = self.0.lock();
            let base = if q.len() > 1 {
                q.pop_front().unwrap()
            } else {
                q[0]
            };
            Ok((base, GWEI))
        }
    }

    fn strategy(bases: &[u128], mode: GasCapMode) -> CappedEip1559<Scripted> {
        CappedEip1559::new(
            Scripted(Mutex::new(bases.iter().copied().collect())),
            50 * GWEI,
            mode,
        )
    }

    #[tokio::test]
    async fn skip_mode_rejects_immediately() {
        let s = strategy(&[80 * GWEI, 10 * GWEI], GasCapMode::Skip);
        assert!(matches!(s.fees().await, Err(GasError::OverCap { .. })));
    }

    #[tokio::test(start_paused = true)]
    async fn wait_mode_waits_for_gas_to_drop() {
        let wait = GasCapMode::Wait {
            max_wait: Duration::from_secs(60),
        };
        let s = strategy(&[80 * GWEI, 70 * GWEI, 10 * GWEI], wait);
        let t0 = tokio::time::Instant::now();
        let fees = s.fees().await.unwrap();
        assert_eq!(fees.max_fee_per_gas, 21 * GWEI);
        assert_eq!(t0.elapsed(), 2 * WAIT_POLL);
    }

    #[tokio::test(start_paused = true)]
    async fn wait_mode_gives_up_after_max_wait() {
        let wait = GasCapMode::Wait {
            max_wait: Duration::from_secs(30),
        };
        let s = strategy(&[80 * GWEI], wait);
        assert!(matches!(s.fees().await, Err(GasError::WaitExpired { .. })));
    }
}
