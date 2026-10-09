//! Strategy + risk stage: TradeIntent -> CopyTrade (or a logged rejection).

use std::time::{Instant, SystemTime, UNIX_EPOCH};

use alloy::eips::BlockId;
use alloy::primitives::Address;
use alloy::primitives::utils::format_ether;
use tokio::sync::mpsc;

use crate::ethereum::chain_state::ChainReader;
use crate::ethereum::provider::ProviderError;
use crate::models::{ChainSnapshot, CopyTrade, TradeIntent, TradeSide};
use crate::risk::{RiskEngine, RiskError};
use crate::storage::DedupStore;
use crate::strategy::{Sizer, StrategyError};

#[derive(Debug, thiserror::Error)]
pub enum Rejection {
    #[error(transparent)]
    Risk(#[from] RiskError),
    #[error(transparent)]
    Strategy(#[from] StrategyError),
    #[error("chain data unavailable: {0}")]
    ChainData(#[from] ProviderError),
}

pub struct DecisionStage<R: ChainReader, D: DedupStore> {
    reader: R,
    sizer: Sizer,
    risk: RiskEngine<D>,
    bot: Address,
    weth: Address,
}

impl<R: ChainReader, D: DedupStore> DecisionStage<R, D> {
    pub fn new(reader: R, sizer: Sizer, risk: RiskEngine<D>, bot: Address, weth: Address) -> Self {
        Self {
            reader,
            sizer,
            risk,
            bot,
            weth,
        }
    }

    /// The source tx is marked processed in `precheck`, so a transient RPC failure later
    /// drops the trade rather than retrying it (a retried copy would be stale anyway).
    pub async fn decide(&mut self, intent: TradeIntent) -> Result<CopyTrade, Rejection> {
        let now = Instant::now();
        self.risk.precheck(&intent, now).await?;
        let chain = self.snapshot(&intent).await?;
        let sizing = self.sizer.size(&intent, &chain)?;
        let trade = CopyTrade {
            intent,
            amount_in: sizing.amount_in,
            capped: sizing.capped,
            decision_at: Instant::now(),
        };
        self.risk.check(&trade, &chain)?;
        self.risk.record_approved(&trade, now);
        Ok(trade)
    }

    /// All reads run concurrently: one RPC round-trip of latency per decision.
    async fn snapshot(&self, intent: &TradeIntent) -> Result<ChainSnapshot, ProviderError> {
        let needs = self.sizer.needs();
        let token = intent.token();
        let latest = BlockId::latest();
        let r = &self.reader;

        let bot_token = async {
            match intent.side {
                TradeSide::Sell => r.token_balance(token, self.bot, latest).await.map(Some),
                TradeSide::Buy => Ok(None),
            }
        };
        let source_balance = async {
            if !needs.source_balance {
                return Ok(None);
            }
            // Balance *before* the source tx: parent block when mined; when pending,
            // latest state (the tx may get mined meanwhile — accepted approximation).
            let at = intent
                .source_block
                .map_or(latest, |n| BlockId::number(n.saturating_sub(1)));
            let (eth, weth) = tokio::try_join!(
                r.eth_balance(intent.source_wallet, at),
                r.token_balance(self.weth, intent.source_wallet, at),
            )?;
            Ok(Some(eth.saturating_add(weth)))
        };
        let eth_usd = async {
            if needs.eth_usd {
                r.eth_usd().await.map(Some)
            } else {
                Ok(None)
            }
        };

        let (gas_price, bot_eth, token_is_contract, bot_token, source_balance, eth_usd) = tokio::try_join!(
            r.gas_price(),
            r.eth_balance(self.bot, latest),
            r.is_contract(token),
            bot_token,
            source_balance,
            eth_usd,
        )?;
        Ok(ChainSnapshot {
            gas_price,
            bot_eth,
            token_is_contract,
            bot_token,
            source_balance,
            eth_usd,
            now_unix: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs()),
        })
    }
}

/// Processes intents one at a time on purpose: cooldown and balance checks would race if
/// two decisions ran concurrently. Each decision costs ~1 RPC round-trip, so bursts queue
/// briefly; parallelise per token if watched wallets ever trade that often.
pub async fn run_stage<R: ChainReader, D: DedupStore>(
    mut stage: DecisionStage<R, D>,
    mut rx: mpsc::Receiver<TradeIntent>,
    out: mpsc::Sender<CopyTrade>,
) {
    while let Some(intent) = rx.recv().await {
        let source_tx = intent.source_tx_hash;
        let source_wallet = intent.source_wallet;
        let side = intent.side;
        let token = intent.token();
        let dex = intent.dex;
        let decoded_at = intent.decoded_at;
        match stage.decide(intent).await {
            Ok(trade) => {
                tracing::info!(
                    event = "trade_approved",
                    %source_wallet,
                    %source_tx,
                    %dex,
                    side = ?side,
                    %token,
                    amount_in = %trade.amount_in,
                    amount_in_eth = (side == TradeSide::Buy).then(|| format_ether(trade.amount_in)),
                    capped = trade.capped,
                    strategy = stage.sizer.strategy_name(),
                    decode_to_decision_ms = (trade.decision_at - decoded_at).as_secs_f64() * 1000.0,
                );
                if out.send(trade).await.is_err() {
                    break;
                }
            }
            Err(Rejection::Risk(RiskError::Duplicate)) => {
                tracing::debug!(event = "trade_rejected", %source_tx, reason = "duplicate");
            }
            Err(reason) => tracing::info!(
                event = "trade_rejected",
                %source_wallet,
                %source_tx,
                %dex,
                side = ?side,
                %token,
                reason = %reason,
                decode_to_decision_ms = decoded_at.elapsed().as_secs_f64() * 1000.0,
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use alloy::primitives::{U256, address};

    use super::*;
    use crate::models::Dex;
    use crate::models::test_util::{SOURCE, TOKEN, WETH, intent};
    use crate::risk::RiskConfig;
    use crate::storage::{DEDUP_TTL, InMemoryDedup};
    use crate::strategy::copy_strategy::{FixedEth, Proportional};

    const ETH: u128 = 1_000_000_000_000_000_000;
    const BOT: Address = address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266");

    #[derive(Default)]
    struct MockReader {
        calls: AtomicUsize,
        bot_eth: U256,
        bot_token: U256,
        source_eth: U256,
        source_weth: U256,
        /// (block id seen by eth_balance for the source wallet)
        source_block_seen: parking_lot::Mutex<Option<BlockId>>,
    }

    impl ChainReader for MockReader {
        async fn gas_price(&self) -> Result<u128, ProviderError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(1_000_000_000)
        }
        async fn eth_balance(&self, who: Address, at: BlockId) -> Result<U256, ProviderError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if who == SOURCE {
                *self.source_block_seen.lock() = Some(at);
                return Ok(self.source_eth);
            }
            Ok(self.bot_eth)
        }
        async fn token_balance(
            &self,
            token: Address,
            who: Address,
            _: BlockId,
        ) -> Result<U256, ProviderError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match (token, who) {
                (WETH, SOURCE) => Ok(self.source_weth),
                (TOKEN, BOT) => Ok(self.bot_token),
                _ => Err(ProviderError::BadResponse("unexpected balanceOf".into())),
            }
        }
        async fn is_contract(&self, _: Address) -> Result<bool, ProviderError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(true)
        }
        async fn eth_usd(&self) -> Result<crate::models::EthUsdPrice, ProviderError> {
            Err(ProviderError::BadResponse("not used".into()))
        }
        async fn allowance(
            &self,
            _: Address,
            _: Address,
            _: Address,
        ) -> Result<U256, ProviderError> {
            Err(ProviderError::BadResponse("not used".into()))
        }
        async fn permit2_allowance(
            &self,
            _: Address,
            _: Address,
            _: Address,
            _: Address,
        ) -> Result<(U256, u64), ProviderError> {
            Err(ProviderError::BadResponse("not used".into()))
        }
        async fn mined_nonce(&self, _: Address) -> Result<u64, ProviderError> {
            Err(ProviderError::BadResponse("not used".into()))
        }
    }

    fn stage(
        reader: MockReader,
        strategy: Box<dyn crate::strategy::CopyStrategy>,
    ) -> DecisionStage<MockReader, InMemoryDedup> {
        let risk = RiskEngine::new(
            RiskConfig {
                chain_id: 1,
                supported_dexes: vec![Dex::UniswapV2],
                token_blacklist: HashSet::new(),
                max_trade_wei: U256::from(ETH / 2),
                max_gas_wei: 100_000_000_000,
                enforce_gas_cap: true,
                cooldown: Duration::from_secs(30),
            },
            InMemoryDedup::new(DEDUP_TTL),
        );
        let sizer = Sizer::new(strategy, 10_000, U256::from(ETH / 2));
        DecisionStage::new(reader, sizer, risk, BOT, WETH)
    }

    fn fixed() -> Box<dyn crate::strategy::CopyStrategy> {
        Box::new(FixedEth {
            amount_wei: U256::from(ETH / 20),
        })
    }

    #[tokio::test]
    async fn buy_is_sized_and_approved_once() {
        let mut s = stage(
            MockReader {
                bot_eth: U256::from(ETH),
                ..Default::default()
            },
            fixed(),
        );
        let trade = s.decide(intent(TradeSide::Buy, 10 * ETH)).await.unwrap();
        assert_eq!(trade.amount_in, U256::from(ETH / 20));

        let again = s.decide(intent(TradeSide::Buy, 10 * ETH)).await;
        assert!(matches!(again, Err(Rejection::Risk(RiskError::Duplicate))));
    }

    #[tokio::test]
    async fn precheck_rejection_costs_no_rpc() {
        let mut s = stage(MockReader::default(), fixed());
        let mut i = intent(TradeSide::Buy, ETH);
        i.dex = Dex::UniswapV3;
        assert!(matches!(
            s.decide(i).await,
            Err(Rejection::Risk(RiskError::DexNotAllowed(_)))
        ));
        assert_eq!(s.reader.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn sell_with_empty_bag_is_ignored() {
        let mut s = stage(
            MockReader {
                bot_eth: U256::from(ETH),
                ..Default::default()
            },
            fixed(),
        );
        assert!(matches!(
            s.decide(intent(TradeSide::Sell, 10_000)).await,
            Err(Rejection::Strategy(StrategyError::NothingToSell))
        ));
    }

    #[tokio::test]
    async fn sell_sells_bot_balance() {
        let mut s = stage(
            MockReader {
                bot_eth: U256::from(ETH),
                bot_token: U256::from(777u64),
                ..Default::default()
            },
            fixed(),
        );
        let trade = s.decide(intent(TradeSide::Sell, 10_000)).await.unwrap();
        assert_eq!(
            trade.amount_in,
            U256::from(777u64),
            "SELL_RATIO=1.0 sells whole bag"
        );
    }

    #[tokio::test]
    async fn proportional_reads_source_balance_before_the_tx() {
        let mut s = stage(
            MockReader {
                bot_eth: U256::from(ETH),
                source_eth: U256::from(6 * ETH),
                source_weth: U256::from(4 * ETH),
                ..Default::default()
            },
            Box::new(Proportional),
        );
        let mut i = intent(TradeSide::Buy, ETH); // source spends 1 of 10 ETH = 10%
        i.source_block = Some(500);
        let trade = s.decide(i).await.unwrap();
        assert_eq!(trade.amount_in, U256::from(ETH / 10));
        assert_eq!(
            *s.reader.source_block_seen.lock(),
            Some(BlockId::number(499))
        );
    }

    #[tokio::test]
    async fn insufficient_eth_rejects_buy() {
        let mut s = stage(
            MockReader {
                bot_eth: U256::from(ETH / 20),
                ..Default::default()
            },
            fixed(),
        );
        assert!(matches!(
            s.decide(intent(TradeSide::Buy, ETH)).await,
            Err(Rejection::Risk(RiskError::InsufficientBalance { .. }))
        ));
    }
}
