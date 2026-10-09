mod copy_trade;
mod snapshot;
mod trade_intent;

use std::fmt;
use std::str::FromStr;

pub use copy_trade::CopyTrade;
pub use snapshot::{ChainSnapshot, EthUsdPrice};
pub use trade_intent::{
    PoolKind, RouteLeg, SourceCall, SwapKind, TradeIntent, TradeSide, V4Hop, V4Path,
};

#[cfg(test)]
pub(crate) mod test_util {
    use std::time::Instant;

    use alloy::primitives::{Address, B256, U256, address};

    use super::*;

    pub const WETH: Address = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
    pub const TOKEN: Address = address!("1111111111111111111111111111111111111111");
    pub const SOURCE: Address = address!("2222222222222222222222222222222222222222");
    pub const ROUTER: Address = address!("7a250d5630B4cF539739dF2C5dAcb4c659F2488D");

    /// Source intent: BUY spends `amount_in` wei of ETH, SELL sells `amount_in` TOKEN.
    pub fn intent(side: TradeSide, amount_in: u128) -> TradeIntent {
        let (token_in, token_out) = match side {
            TradeSide::Buy => (WETH, TOKEN),
            TradeSide::Sell => (TOKEN, WETH),
        };
        let now = Instant::now();
        TradeIntent {
            source_wallet: SOURCE,
            source_tx_hash: B256::repeat_byte(0xab),
            source_block: None,
            dex: Dex::UniswapV2,
            router: ROUTER,
            side,
            token_in,
            token_out,
            native_in: side == TradeSide::Buy,
            native_out: side == TradeSide::Sell,
            swap_kind: SwapKind::ExactIn,
            amount_in: U256::from(amount_in),
            min_amount_out: None,
            recipient: SOURCE,
            route: vec![RouteLeg {
                pool: PoolKind::UniswapV2,
                tokens: vec![token_in, token_out],
                fees: Vec::new(),
                v4: None,
            }],
            source_call: SourceCall {
                to: ROUTER,
                value: U256::ZERO,
                input: Default::default(),
                gas_limit: 300_000,
                nonce: 7,
            },
            detected_at: now,
            decoded_at: now,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Dex {
    UniswapV2,
    UniswapV3,
    UniswapUniversalRouter,
    /// Uniswap V4 pools (reached through the Universal Router's `V4_SWAP`).
    UniswapV4,
}

impl Dex {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Dex::UniswapV2 => "UNISWAP_V2",
            Dex::UniswapV3 => "UNISWAP_V3",
            Dex::UniswapUniversalRouter => "UNISWAP_UNIVERSAL_ROUTER",
            Dex::UniswapV4 => "UNISWAP_V4",
        }
    }
}

impl fmt::Display for Dex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Dex {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_uppercase().as_str() {
            "UNISWAP_V2" => Ok(Dex::UniswapV2),
            "UNISWAP_V3" => Ok(Dex::UniswapV3),
            "UNISWAP_UNIVERSAL_ROUTER" => Ok(Dex::UniswapUniversalRouter),
            "UNISWAP_V4" => Ok(Dex::UniswapV4),
            other => Err(format!("unsupported dex `{other}`")),
        }
    }
}
