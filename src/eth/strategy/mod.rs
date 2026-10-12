pub mod copy_strategy;

use alloy::primitives::U256;

use crate::eth::config::{BPS_DENOMINATOR, Config, StrategyKind};
use crate::eth::models::{ChainSnapshot, TradeIntent, TradeSide};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum StrategyError {
    #[error("bot holds none of the token being sold")]
    NothingToSell,
    #[error("computed trade size is zero")]
    ZeroAmount,
    #[error("source wallet balance is zero (cannot size proportionally)")]
    ZeroSourceBalance,
    #[error("ETH/USD price is invalid: {0}")]
    InvalidPrice(&'static str),
    #[error("missing chain data: {0}")]
    MissingData(&'static str),
}

/// On-chain inputs a strategy needs beyond the always-fetched snapshot fields.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DataNeeds {
    pub source_balance: bool,
    pub eth_usd: bool,
}

/// BUY sizing policy. SELL sizing is shared (fraction of the bot's own balance), because
/// the bot's position is unrelated to the source's.
pub trait CopyStrategy: Send + Sync {
    fn name(&self) -> &'static str;

    fn needs(&self) -> DataNeeds {
        DataNeeds::default()
    }

    /// ETH (wei) to spend, before the MAX_TRADE_ETH cap.
    fn buy_amount(
        &self,
        intent: &TradeIntent,
        chain: &ChainSnapshot,
    ) -> Result<U256, StrategyError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sizing {
    pub amount_in: U256,
    pub capped: bool,
}

pub struct Sizer {
    strategy: Box<dyn CopyStrategy>,
    sell_ratio_bps: u32,
    max_trade_wei: U256,
}

impl Sizer {
    pub fn new(strategy: Box<dyn CopyStrategy>, sell_ratio_bps: u32, max_trade_wei: U256) -> Self {
        Self {
            strategy,
            sell_ratio_bps,
            max_trade_wei,
        }
    }

    pub fn from_config(cfg: &Config) -> Self {
        use copy_strategy::*;
        let strategy: Box<dyn CopyStrategy> = match cfg.strategy {
            StrategyKind::FixedEth => Box::new(FixedEth {
                amount_wei: cfg.fixed_trade_wei,
            }),
            StrategyKind::FixedUsd => Box::new(FixedUsd {
                amount_usd_micro: cfg.fixed_trade_usd_micro,
            }),
            StrategyKind::Ratio => Box::new(Ratio {
                ratio_bps: cfg.buy_ratio_bps,
            }),
            StrategyKind::Proportional => Box::new(Proportional),
        };
        Self::new(strategy, cfg.sell_ratio_bps, cfg.max_trade_wei)
    }

    pub fn strategy_name(&self) -> &'static str {
        self.strategy.name()
    }

    pub fn needs(&self) -> DataNeeds {
        self.strategy.needs()
    }

    pub fn size(
        &self,
        intent: &TradeIntent,
        chain: &ChainSnapshot,
    ) -> Result<Sizing, StrategyError> {
        match intent.side {
            TradeSide::Buy => {
                let raw = self.strategy.buy_amount(intent, chain)?;
                if raw.is_zero() {
                    return Err(StrategyError::ZeroAmount);
                }
                Ok(Sizing {
                    amount_in: raw.min(self.max_trade_wei),
                    capped: raw > self.max_trade_wei,
                })
            }
            TradeSide::Sell => {
                let balance = chain
                    .bot_token
                    .ok_or(StrategyError::MissingData("bot token balance"))?;
                if balance.is_zero() {
                    return Err(StrategyError::NothingToSell);
                }
                let amount = mul_bps(balance, self.sell_ratio_bps);
                if amount.is_zero() {
                    return Err(StrategyError::ZeroAmount);
                }
                Ok(Sizing {
                    amount_in: amount,
                    capped: false,
                })
            }
        }
    }
}

/// `value * bps / 10_000`, rounding down. Saturates instead of overflowing (U256 inputs
/// near the max are not meaningful token amounts anyway).
pub fn mul_bps(value: U256, bps: u32) -> U256 {
    value.saturating_mul(U256::from(bps)) / U256::from(BPS_DENOMINATOR)
}

#[cfg(test)]
pub(crate) mod test_util {
    use super::*;

    pub fn snapshot() -> ChainSnapshot {
        ChainSnapshot {
            gas_price: 1_000_000_000,
            bot_eth: U256::from(10u128 * 10u128.pow(18)),
            token_is_contract: true,
            bot_token: None,
            source_balance: None,
            eth_usd: None,
            now_unix: 1_800_000_000,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::copy_strategy::*;
    use super::test_util::snapshot;
    use super::*;
    use crate::eth::models::test_util::intent;

    const ETH: u128 = 1_000_000_000_000_000_000;

    fn sizer(strategy: Box<dyn CopyStrategy>) -> Sizer {
        Sizer::new(strategy, 5_000, U256::from(ETH / 2))
    }

    #[test]
    fn buy_is_capped_at_max_trade() {
        let s = sizer(Box::new(Ratio { ratio_bps: 1_000 }));
        // target buys 10 ETH -> ratio 10% = 1 ETH -> capped to 0.5 ETH
        let sized = s
            .size(&intent(TradeSide::Buy, 10 * ETH), &snapshot())
            .unwrap();
        assert_eq!(sized.amount_in, U256::from(ETH / 2));
        assert!(sized.capped);

        let sized = s.size(&intent(TradeSide::Buy, ETH), &snapshot()).unwrap();
        assert_eq!(sized.amount_in, U256::from(ETH / 10));
        assert!(!sized.capped);
    }

    #[test]
    fn zero_buy_is_rejected() {
        let s = sizer(Box::new(Ratio { ratio_bps: 1 }));
        // 1 wei * 0.01% rounds to zero
        assert_eq!(
            s.size(&intent(TradeSide::Buy, 1), &snapshot()),
            Err(StrategyError::ZeroAmount)
        );
    }

    #[test]
    fn sell_uses_bot_balance_not_source_amount() {
        let s = sizer(Box::new(FixedEth {
            amount_wei: U256::from(ETH / 20),
        }));
        let mut chain = snapshot();
        chain.bot_token = Some(U256::from(300u64));
        // target sells 10_000 tokens; bot holds 300 -> SELL_RATIO 50% -> 150
        let sized = s.size(&intent(TradeSide::Sell, 10_000), &chain).unwrap();
        assert_eq!(sized.amount_in, U256::from(150u64));
        assert!(!sized.capped);
    }

    #[test]
    fn sell_full_balance_with_ratio_one() {
        let s = Sizer::new(
            Box::new(FixedEth {
                amount_wei: U256::from(1u64),
            }),
            10_000,
            U256::from(ETH),
        );
        let mut chain = snapshot();
        chain.bot_token = Some(U256::from(12_345u64));
        let sized = s.size(&intent(TradeSide::Sell, 1), &chain).unwrap();
        assert_eq!(sized.amount_in, U256::from(12_345u64));
    }

    #[test]
    fn sell_with_zero_balance_is_ignored() {
        let s = sizer(Box::new(Ratio { ratio_bps: 1_000 }));
        let mut chain = snapshot();
        chain.bot_token = Some(U256::ZERO);
        assert_eq!(
            s.size(&intent(TradeSide::Sell, 10), &chain),
            Err(StrategyError::NothingToSell)
        );
        chain.bot_token = Some(U256::from(1u64));
        assert_eq!(
            s.size(&intent(TradeSide::Sell, 10), &chain),
            Err(StrategyError::ZeroAmount),
            "50% of 1 wei rounds to 0"
        );
        chain.bot_token = None;
        assert!(matches!(
            s.size(&intent(TradeSide::Sell, 10), &chain),
            Err(StrategyError::MissingData(_))
        ));
    }

    #[test]
    fn mul_bps_rounds_down_and_saturates() {
        assert_eq!(mul_bps(U256::from(999u64), 1_000), U256::from(99u64));
        assert_eq!(
            mul_bps(U256::MAX, 10_000),
            U256::MAX / U256::from(10_000u64)
        );
    }
}
