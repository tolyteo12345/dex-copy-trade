//! Sign + send a simulated `ExecutionPlan`, then follow the swap's receipt.

use std::future::Future;
use std::time::{Duration, Instant};

use alloy::eips::BlockId;
use alloy::eips::eip2718::Encodable2718;
use alloy::network::{Ethereum, NetworkTransactionBuilder, TransactionBuilder};
use alloy::primitives::{Address, B256, Bytes};
use alloy::providers::{
    DynProvider, PendingTransactionBuilder, PendingTransactionError, Provider, WatchTxError,
};

use super::executor::ExecutionPlan;
use super::gas::Eip1559Fees;
use crate::eth::ethereum::nonce::NonceManager;
use crate::eth::ethereum::provider::{self, ProviderError};
use crate::eth::ethereum::signer::BotSigner;

pub const RECEIPT_TIMEOUT: Duration = Duration::from_secs(180);

/// Peak simulated gas (pre-refund) + 30%: covers the 1/64 kept back on calls (EIP-150),
/// state drift between simulation and inclusion, and FoT tokens. Unused gas isn't charged.
/// Sizing from post-refund `gasUsed` instead runs out of gas on sells that zero a balance.
pub fn gas_limit(simulated_peak_gas: u64) -> u64 {
    (simulated_peak_gas.saturating_mul(13) / 10).max(21_000)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReceiptSummary {
    pub success: bool,
    pub block_number: Option<u64>,
    pub gas_used: u64,
    pub effective_gas_price: u128,
}

#[derive(Debug, thiserror::Error)]
pub enum BroadcastError {
    #[error("nonce unavailable: {0}")]
    Nonce(ProviderError),
    #[error("signing {step} failed: {reason}")]
    Sign { step: &'static str, reason: String },
    #[error("{step} rejected by node ({accepted} earlier tx(s) accepted): {error}")]
    Rejected {
        step: &'static str,
        accepted: usize,
        error: ProviderError,
    },
    #[error("no receipt within {0}s (tx may still be pending)")]
    ReceiptTimeout(u64),
    #[error("receipt unavailable: {0}")]
    Receipt(String),
}

pub trait TxSender: Send + Sync {
    /// `eth_getTransactionCount(who, "pending")`.
    fn pending_nonce(
        &self,
        who: Address,
    ) -> impl Future<Output = Result<u64, ProviderError>> + Send;
    fn send_raw(&self, raw: Bytes) -> impl Future<Output = Result<B256, ProviderError>> + Send;
    fn wait_receipt(
        &self,
        hash: B256,
        timeout: Duration,
    ) -> impl Future<Output = Result<ReceiptSummary, BroadcastError>> + Send;
}

pub struct RpcSender {
    provider: DynProvider,
}

impl RpcSender {
    pub fn new(provider: DynProvider) -> Self {
        Self { provider }
    }
}

impl TxSender for RpcSender {
    async fn pending_nonce(&self, who: Address) -> Result<u64, ProviderError> {
        Ok(self
            .provider
            .get_transaction_count(who)
            .block_id(BlockId::pending())
            .await?)
    }

    async fn send_raw(&self, raw: Bytes) -> Result<B256, ProviderError> {
        Ok(*self.provider.send_raw_transaction(&raw).await?.tx_hash())
    }

    async fn wait_receipt(
        &self,
        hash: B256,
        timeout: Duration,
    ) -> Result<ReceiptSummary, BroadcastError> {
        let pending = PendingTransactionBuilder::new(self.provider.root().clone(), hash)
            .with_timeout(Some(timeout));
        match pending.get_receipt().await {
            Ok(r) => Ok(ReceiptSummary {
                success: r.status(),
                block_number: r.block_number,
                gas_used: r.gas_used,
                effective_gas_price: r.effective_gas_price,
            }),
            Err(PendingTransactionError::TxWatcher(WatchTxError::Timeout)) => {
                Err(BroadcastError::ReceiptTimeout(timeout.as_secs()))
            }
            Err(e) => Err(BroadcastError::Receipt(provider::redact_urls_in(
                &e.to_string(),
            ))),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SentTx {
    pub step: &'static str,
    pub hash: B256,
    pub nonce: u64,
}

#[derive(Debug, Clone)]
pub struct SentTrade {
    /// In nonce order; the swap is last.
    pub txs: Vec<SentTx>,
    pub fees: Eip1559Fees,
    pub signed_at: Instant,
    pub broadcast_at: Instant,
}

pub struct Broadcaster<T: TxSender> {
    sender: T,
    signer: BotSigner,
    nonces: NonceManager,
    chain_id: u64,
}

impl<T: TxSender> Broadcaster<T> {
    pub fn new(sender: T, signer: BotSigner, nonces: NonceManager, chain_id: u64) -> Self {
        Self {
            sender,
            signer,
            nonces,
            chain_id,
        }
    }

    /// Signs all plan txs with consecutive nonces and sends them in order. Holds the nonce
    /// lease for the whole send, so another trade can't interleave or reuse nonces.
    pub async fn send(
        &self,
        plan: &ExecutionPlan,
        fees: Eip1559Fees,
    ) -> Result<SentTrade, BroadcastError> {
        let bot = self.signer.address();
        let lease = self
            .nonces
            .lease(plan.txs.len() as u64, || self.sender.pending_nonce(bot))
            .await
            .map_err(BroadcastError::Nonce)?;

        let wallet = self.signer.wallet();
        let mut signed = Vec::with_capacity(plan.txs.len());
        for (i, tx) in plan.txs.iter().enumerate() {
            let nonce = lease.nonce(i as u64);
            let request = tx
                .request
                .clone()
                .with_from(bot)
                .with_nonce(nonce)
                .with_chain_id(self.chain_id)
                .with_gas_limit(gas_limit(tx.peak_gas))
                .with_max_fee_per_gas(fees.max_fee_per_gas)
                .with_max_priority_fee_per_gas(fees.max_priority_fee_per_gas);
            // Dropping the lease on error consumes no nonce.
            let envelope = NetworkTransactionBuilder::<Ethereum>::build(request, &wallet)
                .await
                .map_err(|e| BroadcastError::Sign {
                    step: tx.step,
                    reason: e.to_string(),
                })?;
            let sent = SentTx {
                step: tx.step,
                hash: *envelope.tx_hash(),
                nonce,
            };
            signed.push((sent, Bytes::from(envelope.encoded_2718())));
        }
        let signed_at = Instant::now();

        let mut accepted: Vec<B256> = Vec::with_capacity(signed.len());
        let mut txs = Vec::with_capacity(signed.len());
        for (sent, raw) in signed {
            if let Err(error) = self.sender.send_raw(raw).await {
                let n = accepted.len();
                lease.finish(&accepted, true);
                return Err(BroadcastError::Rejected {
                    step: sent.step,
                    accepted: n,
                    error,
                });
            }
            accepted.push(sent.hash);
            txs.push(sent);
        }
        lease.finish(&accepted, false);
        Ok(SentTrade {
            txs,
            fees,
            signed_at,
            broadcast_at: Instant::now(),
        })
    }

    #[cfg(test)]
    pub(crate) fn sender_for_tests(&self) -> &T {
        &self.sender
    }

    /// Waits for the swap (last tx). A timeout forces a nonce resync: the tx may be stuck.
    pub async fn track(&self, sent: &SentTrade) -> Result<ReceiptSummary, BroadcastError> {
        let Some(swap) = sent.txs.last() else {
            return Err(BroadcastError::Receipt("nothing was sent".into()));
        };
        match self.sender.wait_receipt(swap.hash, RECEIPT_TIMEOUT).await {
            Ok(receipt) => {
                self.nonces.confirmed(swap.nonce).await;
                Ok(receipt)
            }
            Err(e) => {
                self.nonces.resync().await;
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use alloy::consensus::transaction::SignerRecoverable;
    use alloy::consensus::{Transaction as _, TxEnvelope};
    use alloy::eips::eip2718::Decodable2718;
    use alloy::primitives::{U256, address};
    use alloy::rpc::types::TransactionRequest;
    use parking_lot::Mutex;

    use super::*;
    use crate::eth::execution::executor::PlannedTx;
    use crate::eth::models::test_util::{TOKEN, intent};
    use crate::eth::models::{CopyTrade, PoolKind, RouteLeg, TradeSide};

    // Public Anvil dev key #0 — test only.
    const KEY: &str = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
    const BOT: Address = address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
    const ROUTER: Address = address!("7a250d5630B4cF539739dF2C5dAcb4c659F2488D");

    #[derive(Default)]
    struct MockSender {
        chain_nonce: u64,
        /// 0-based index of the send call that fails.
        fail_at: Option<usize>,
        sends: AtomicUsize,
        raws: Mutex<Vec<Bytes>>,
        receipt: Option<ReceiptSummary>,
    }

    impl TxSender for MockSender {
        async fn pending_nonce(&self, who: Address) -> Result<u64, ProviderError> {
            assert_eq!(who, BOT);
            Ok(self.chain_nonce)
        }
        async fn send_raw(&self, raw: Bytes) -> Result<B256, ProviderError> {
            let i = self.sends.fetch_add(1, Ordering::SeqCst);
            if self.fail_at == Some(i) {
                return Err(ProviderError::BadResponse("nonce too low".into()));
            }
            let hash = alloy::primitives::keccak256(&raw);
            self.raws.lock().push(raw);
            Ok(hash)
        }
        async fn wait_receipt(
            &self,
            _: B256,
            t: Duration,
        ) -> Result<ReceiptSummary, BroadcastError> {
            self.receipt
                .ok_or(BroadcastError::ReceiptTimeout(t.as_secs()))
        }
    }

    fn plan(steps: &[&'static str]) -> ExecutionPlan {
        let txs = steps
            .iter()
            .map(|step| PlannedTx {
                step,
                request: TransactionRequest::default()
                    .to(if *step == "swap" { ROUTER } else { TOKEN })
                    .value(U256::from(5))
                    .input(vec![1, 2, 3].into()),
                gas_used: 100_000,
                peak_gas: 150_000, // refunds make post-refund gas_used lower than the need
            })
            .collect();
        let now = Instant::now();
        ExecutionPlan {
            trade: CopyTrade {
                intent: intent(TradeSide::Buy, 1),
                amount_in: U256::from(5),
                capped: false,
                decision_at: now,
            },
            route: RouteLeg {
                pool: PoolKind::UniswapV2,
                tokens: vec![],
                fees: vec![],
                v4: None,
            },
            amount_in: U256::from(5),
            sell_capped: false,
            quoted_out: U256::from(1),
            expected_out: U256::from(1),
            transfer_tax_bps: 0,
            min_out: U256::from(1),
            price_impact_bps: 0,
            after_source: false,
            txs,
            simulated_at: now,
        }
    }

    const FEES: Eip1559Fees = Eip1559Fees {
        max_fee_per_gas: 30_000_000_000,
        max_priority_fee_per_gas: 1_000_000_000,
    };

    fn broadcaster(sender: MockSender) -> Broadcaster<MockSender> {
        Broadcaster::new(
            sender,
            BotSigner::from_hex(KEY).unwrap(),
            NonceManager::new(),
            1,
        )
    }

    #[tokio::test]
    async fn signs_eip1559_with_consecutive_nonces_and_bot_key() {
        let b = broadcaster(MockSender {
            chain_nonce: 9,
            ..Default::default()
        });
        let sent = b.send(&plan(&["approve", "swap"]), FEES).await.unwrap();
        assert_eq!(
            sent.txs
                .iter()
                .map(|t| (t.step, t.nonce))
                .collect::<Vec<_>>(),
            vec![("approve", 9), ("swap", 10)]
        );
        let raws = b.sender.raws.lock().clone(); // don't hold the lock across the next send
        for (raw, expected_nonce) in raws.iter().zip([9, 10]) {
            let env = TxEnvelope::decode_2718(&mut raw.as_ref()).unwrap();
            assert!(matches!(env, TxEnvelope::Eip1559(_)), "expected EIP-1559");
            assert_eq!(env.recover_signer().unwrap(), BOT);
            assert_eq!(env.nonce(), expected_nonce);
            assert_eq!(env.chain_id(), Some(1));
            assert_eq!(
                env.gas_limit(),
                195_000,
                "limit from peak gas, not post-refund"
            );
            assert_eq!(env.max_fee_per_gas(), FEES.max_fee_per_gas);
            assert_eq!(
                env.max_priority_fee_per_gas(),
                Some(FEES.max_priority_fee_per_gas)
            );
            assert_eq!(env.value(), U256::from(5));
        }
        // Next trade continues locally after the two sent txs.
        let sent = b.send(&plan(&["swap"]), FEES).await.unwrap();
        assert_eq!(sent.txs[0].nonce, 11);
    }

    #[tokio::test]
    async fn rejected_swap_keeps_sent_approve_and_resyncs() {
        let b = broadcaster(MockSender {
            chain_nonce: 4,
            fail_at: Some(1),
            ..Default::default()
        });
        let err = b.send(&plan(&["approve", "swap"]), FEES).await.unwrap_err();
        assert!(matches!(
            err,
            BroadcastError::Rejected {
                step: "swap",
                accepted: 1,
                ..
            }
        ));
        assert_eq!(b.nonces.pending_count().await, 1);
    }

    #[tokio::test]
    async fn receipt_confirms_or_resyncs() {
        let ok = ReceiptSummary {
            success: true,
            block_number: Some(1),
            gas_used: 90_000,
            effective_gas_price: 1,
        };
        let b = broadcaster(MockSender {
            receipt: Some(ok),
            ..Default::default()
        });
        let sent = b.send(&plan(&["approve", "swap"]), FEES).await.unwrap();
        assert_eq!(b.track(&sent).await.unwrap(), ok);
        assert_eq!(
            b.nonces.pending_count().await,
            0,
            "swap mined => approve too"
        );

        let b = broadcaster(MockSender::default());
        let sent = b.send(&plan(&["swap"]), FEES).await.unwrap();
        assert!(matches!(
            b.track(&sent).await,
            Err(BroadcastError::ReceiptTimeout(180))
        ));
    }

    #[test]
    fn gas_limit_margin() {
        assert_eq!(gas_limit(150_000), 195_000);
        assert_eq!(gas_limit(0), 21_000);
    }
}
