use alloy::primitives::{I256, U256};

use super::{CopyStrategy, DataNeeds, StrategyError, mul_bps};
use crate::models::{ChainSnapshot, TradeIntent};

/// Chainlink ETH/USD heartbeat is 1h; allow some slack before calling the feed stale.
pub const MAX_PRICE_AGE_SECS: u64 = 3_600 + 900;

/// Always spend a fixed amount of ETH.
pub struct FixedEth {
    pub amount_wei: U256,
}

impl CopyStrategy for FixedEth {
    fn name(&self) -> &'static str {
        "FIXED_ETH"
    }

    fn buy_amount(&self, _: &TradeIntent, _: &ChainSnapshot) -> Result<U256, StrategyError> {
        Ok(self.amount_wei)
    }
}

/// Spend a fixed USD amount, converted with the Chainlink ETH/USD price.
pub struct FixedUsd {
    /// USD with 6 decimals.
    pub amount_usd_micro: U256,
}

impl CopyStrategy for FixedUsd {
    fn name(&self) -> &'static str {
        "FIXED_USD"
    }

    fn needs(&self) -> DataNeeds {
        DataNeeds {
            eth_usd: true,
            ..DataNeeds::default()
        }
    }

    fn buy_amount(&self, _: &TradeIntent, chain: &ChainSnapshot) -> Result<U256, StrategyError> {
        let price = chain
            .eth_usd
            .ok_or(StrategyError::MissingData("ETH/USD price"))?;
        if price.answer <= I256::ZERO {
            return Err(StrategyError::InvalidPrice("non-positive answer"));
        }
        if chain.now_unix.saturating_sub(price.updated_at) > MAX_PRICE_AGE_SECS {
            return Err(StrategyError::InvalidPrice("stale round"));
        }
        // wei = usd_micro / 1e6 / (answer / 1e8) * 1e18 = usd_micro * 1e20 / answer
        let answer = price.answer.into_raw();
        Ok(self
            .amount_usd_micro
            .saturating_mul(U256::from(10u128.pow(20)))
            / answer)
    }
}

/// Spend a fraction of what the source spent.
pub struct Ratio {
    pub ratio_bps: u32,
}

impl CopyStrategy for Ratio {
    fn name(&self) -> &'static str {
        "RATIO"
    }

    /// For exact-output source swaps `amount_in` is the source's max input, so this
    /// slightly over-sizes; MAX_TRADE_ETH still bounds it.
    fn buy_amount(&self, intent: &TradeIntent, _: &ChainSnapshot) -> Result<U256, StrategyError> {
        Ok(mul_bps(intent.amount_in, self.ratio_bps))
    }
}

/// Spend the same fraction of the bot's ETH as the source spent of its ETH+WETH
/// (balance read right before the source tx).
pub struct Proportional;

impl CopyStrategy for Proportional {
    fn name(&self) -> &'static str {
        "PROPORTIONAL"
    }

    fn needs(&self) -> DataNeeds {
        DataNeeds {
            source_balance: true,
            ..DataNeeds::default()
        }
    }

    fn buy_amount(
        &self,
        intent: &TradeIntent,
        chain: &ChainSnapshot,
    ) -> Result<U256, StrategyError> {
        let source = chain
            .source_balance
            .ok_or(StrategyError::MissingData("source wallet balance"))?;
        if source.is_zero() {
            return Err(StrategyError::ZeroSourceBalance);
        }
        // The source cannot spend more than it had; clamp so the fraction stays <= 1.
        let spent = intent.amount_in.min(source);
        Ok(spent.saturating_mul(chain.bot_eth) / source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::test_util::intent;
    use crate::models::{EthUsdPrice, TradeSide};
    use crate::strategy::test_util::snapshot;

    const ETH: u128 = 1_000_000_000_000_000_000;

    #[test]
    fn fixed_eth_ignores_source_size() {
        let s = FixedEth {
            amount_wei: U256::from(ETH / 20),
        };
        let amount = s
            .buy_amount(&intent(TradeSide::Buy, 10 * ETH), &snapshot())
            .unwrap();
        assert_eq!(
            amount,
            U256::from(ETH / 20),
            "target 10 ETH -> bot 0.05 ETH"
        );
    }

    #[test]
    fn ratio_scales_source_amount() {
        let s = Ratio { ratio_bps: 1_000 };
        let amount = s
            .buy_amount(&intent(TradeSide::Buy, 10 * ETH), &snapshot())
            .unwrap();
        assert_eq!(
            amount,
            U256::from(ETH),
            "10 ETH * 0.1 = 1 ETH (cap applied by Sizer)"
        );
    }

    fn with_price(answer: i64, age: u64) -> ChainSnapshot {
        let mut chain = snapshot();
        chain.eth_usd = Some(EthUsdPrice {
            answer: I256::try_from(answer).unwrap(),
            updated_at: chain.now_unix - age,
        });
        chain
    }

    #[test]
    fn fixed_usd_converts_with_chainlink_price() {
        let s = FixedUsd {
            amount_usd_micro: U256::from(100_000_000u64), // $100
        };
        // $2500.00000000 / ETH -> $100 = 0.04 ETH
        let amount = s
            .buy_amount(
                &intent(TradeSide::Buy, ETH),
                &with_price(250_000_000_000, 60),
            )
            .unwrap();
        assert_eq!(amount, U256::from(ETH / 25));
    }

    #[test]
    fn fixed_usd_rejects_bad_prices() {
        let s = FixedUsd {
            amount_usd_micro: U256::from(1u64),
        };
        let buy = intent(TradeSide::Buy, ETH);
        assert_eq!(
            s.buy_amount(&buy, &with_price(0, 0)),
            Err(StrategyError::InvalidPrice("non-positive answer"))
        );
        assert_eq!(
            s.buy_amount(&buy, &with_price(-5, 0)),
            Err(StrategyError::InvalidPrice("non-positive answer"))
        );
        assert_eq!(
            s.buy_amount(&buy, &with_price(1, MAX_PRICE_AGE_SECS + 1)),
            Err(StrategyError::InvalidPrice("stale round"))
        );
        assert!(matches!(
            s.buy_amount(&buy, &snapshot()),
            Err(StrategyError::MissingData(_))
        ));
    }

    #[test]
    fn proportional_mirrors_fraction_of_holdings() {
        let mut chain = snapshot(); // bot holds 10 ETH
        chain.source_balance = Some(U256::from(100 * ETH));
        // source spends 20 of its 100 ETH (20%) -> bot spends 20% of 10 = 2 ETH
        let amount = Proportional
            .buy_amount(&intent(TradeSide::Buy, 20 * ETH), &chain)
            .unwrap();
        assert_eq!(amount, U256::from(2 * ETH));

        // inconsistent data (spent > balance) is clamped to 100%
        chain.source_balance = Some(U256::from(ETH));
        let amount = Proportional
            .buy_amount(&intent(TradeSide::Buy, 5 * ETH), &chain)
            .unwrap();
        assert_eq!(amount, chain.bot_eth);

        chain.source_balance = Some(U256::ZERO);
        assert_eq!(
            Proportional.buy_amount(&intent(TradeSide::Buy, ETH), &chain),
            Err(StrategyError::ZeroSourceBalance)
        );
    }
}
