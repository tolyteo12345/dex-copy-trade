use std::time::Instant;

use alloy::primitives::{Address, U256};

use super::{TradeIntent, TradeSide};

/// A sized copy of a source trade, ready for risk checks and execution.
#[derive(Debug, Clone)]
pub struct CopyTrade {
    pub intent: TradeIntent,
    /// BUY: ETH (wei) the bot spends. SELL: token units the bot sells.
    pub amount_in: U256,
    /// The strategy's raw size exceeded MAX_TRADE_ETH and was reduced to it.
    pub capped: bool,
    pub decision_at: Instant,
}

impl CopyTrade {
    pub fn side(&self) -> TradeSide {
        self.intent.side
    }

    pub fn token(&self) -> Address {
        self.intent.token()
    }
}
