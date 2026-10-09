use std::time::Instant;

use alloy::primitives::{Address, B256, Bytes, U256};

use super::Dex;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TradeSide {
    /// ETH/WETH -> token
    Buy,
    /// token -> ETH/WETH
    Sell,
}

impl TradeSide {
    /// Token-to-token swaps (neither side WETH) and WETH<->WETH have no side.
    pub fn classify(token_in: Address, token_out: Address, weth: Address) -> Option<Self> {
        match (token_in == weth, token_out == weth) {
            (true, false) => Some(TradeSide::Buy),
            (false, true) => Some(TradeSide::Sell),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapKind {
    ExactIn,
    ExactOut,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolKind {
    UniswapV2,
    UniswapV3,
    UniswapV4,
}

/// One swap hop sequence as executed by the source tx; a hint for building our own route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteLeg {
    pub pool: PoolKind,
    /// Tokens in swap direction: `tokens[0]` in, `tokens[last]` out.
    pub tokens: Vec<Address>,
    /// Fee tier per hop (`tokens.len() - 1` entries); empty for V2.
    pub fees: Vec<u32>,
    /// V4 only: the exact pools (`tokens` above has native ETH normalised to WETH).
    pub v4: Option<V4Path>,
}

/// A Uniswap V4 path with real currencies (`Address::ZERO` = native ETH).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct V4Path {
    pub currency_in: Address,
    pub hops: Vec<V4Hop>,
}

/// One V4 pool hop; together with the previous currency it identifies the `PoolKey`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct V4Hop {
    pub currency_out: Address,
    pub fee: u32,
    pub tick_spacing: i32,
    pub hooks: Address,
    pub hook_data: Bytes,
}

/// DEX-independent description of what the source wallet tried to do.
///
/// ETH/WETH convention: `token_in`/`token_out` are always ERC20 addresses, so native ETH
/// appears as WETH. `native_in`/`native_out` record whether the source paid/received
/// native ETH, which decides whether we build an ETH or a WETH variant of the swap.
#[derive(Debug, Clone)]
pub struct TradeIntent {
    pub source_wallet: Address,
    pub source_tx_hash: B256,
    /// Block that included the source tx; `None` while pending.
    pub source_block: Option<u64>,
    pub dex: Dex,
    pub router: Address,
    pub side: TradeSide,
    pub token_in: Address,
    pub token_out: Address,
    pub native_in: bool,
    pub native_out: bool,
    pub swap_kind: SwapKind,
    /// ExactIn: exact input. ExactOut: max input the source accepted.
    pub amount_in: U256,
    /// ExactIn: min output. ExactOut: exact output. `None` when the source set no bound (0).
    pub min_amount_out: Option<U256>,
    pub recipient: Address,
    pub route: Vec<RouteLeg>,
    pub source_call: SourceCall,
    pub detected_at: Instant,
    pub decoded_at: Instant,
}

/// The source tx's call fields. Used ONLY to simulate the post-source state while the source
/// tx is still pending; it is never signed or broadcast by the bot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceCall {
    pub to: Address,
    pub value: U256,
    pub input: Bytes,
    pub gas_limit: u64,
    pub nonce: u64,
}

impl TradeIntent {
    /// The non-WETH token being bought or sold.
    pub fn token(&self) -> Address {
        match self.side {
            TradeSide::Buy => self.token_out,
            TradeSide::Sell => self.token_in,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;

    const WETH: Address = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
    const TOKEN: Address = address!("1111111111111111111111111111111111111111");
    const OTHER: Address = address!("2222222222222222222222222222222222222222");

    #[test]
    fn side_classification() {
        assert_eq!(TradeSide::classify(WETH, TOKEN, WETH), Some(TradeSide::Buy));
        assert_eq!(
            TradeSide::classify(TOKEN, WETH, WETH),
            Some(TradeSide::Sell)
        );
        assert_eq!(TradeSide::classify(TOKEN, OTHER, WETH), None);
        assert_eq!(TradeSide::classify(WETH, WETH, WETH), None);
    }
}
