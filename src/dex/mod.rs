//! Execution-side DEX adapters: build quote calls and swap txs for the bot's own trade.
//! Pure (no RPC): the executor runs the calls inside `eth_simulateV1` bundles.

pub mod uniswap_v2;
pub mod uniswap_v3;
pub mod uniswap_v4;

use alloy::primitives::{Address, U256};
use alloy::rpc::types::TransactionRequest;

use crate::models::{PoolKind, RouteLeg, TradeSide};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DexError {
    #[error("invalid route: {0}")]
    InvalidRoute(&'static str),
    #[error("cannot decode quote: {0}")]
    BadQuote(String),
}

/// The bot always pays native ETH on BUY and receives native ETH on SELL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwapParams {
    pub side: TradeSide,
    pub amount_in: U256,
    pub min_out: U256,
    pub recipient: Address,
    /// Unix seconds.
    pub deadline: U256,
}

pub trait DexAdapter: Send + Sync {
    fn pool(&self) -> PoolKind;
    /// Contract pulling the input token on SELL (approval target).
    fn spender(&self) -> Address;
    /// When `spender()` is Permit2: the contract Permit2 must authorise (the router).
    fn permit2_spender(&self) -> Option<Address> {
        None
    }
    /// Exact-input quote for `route`, ignoring transfer taxes.
    fn quote_call(&self, route: &RouteLeg, amount_in: U256)
    -> Result<TransactionRequest, DexError>;
    fn decode_quote(&self, return_data: &[u8]) -> Result<U256, DexError>;
    /// Swap tx without `from`/gas/nonce (filled by the executor / gas strategy).
    fn swap_tx(
        &self,
        route: &RouteLeg,
        params: &SwapParams,
    ) -> Result<TransactionRequest, DexError>;
}

/// Checks the route is a single path from `token_in` to `token_out` for `pool`.
pub(crate) fn check_route(
    route: &RouteLeg,
    pool: PoolKind,
    side: TradeSide,
    weth: Address,
) -> Result<(), DexError> {
    if route.pool != pool {
        return Err(DexError::InvalidRoute(
            "route pool kind does not match adapter",
        ));
    }
    let (Some(first), Some(last)) = (route.tokens.first(), route.tokens.last()) else {
        return Err(DexError::InvalidRoute("empty path"));
    };
    if route.tokens.len() < 2 {
        return Err(DexError::InvalidRoute("path shorter than 2"));
    }
    let ok = match side {
        TradeSide::Buy => *first == weth && *last != weth,
        TradeSide::Sell => *first != weth && *last == weth,
    };
    if !ok {
        return Err(DexError::InvalidRoute(
            "path endpoints do not match trade side",
        ));
    }
    Ok(())
}
