use alloy::primitives::{I256, U256};

/// Chainlink ETH/USD round (8 decimals).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EthUsdPrice {
    pub answer: I256,
    /// Unix seconds of the round update.
    pub updated_at: u64,
}

/// On-chain state read once per trade decision. Optional fields are only fetched when the
/// side/strategy needs them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainSnapshot {
    /// `eth_gasPrice` (base fee + suggested tip), wei.
    pub gas_price: u128,
    pub bot_eth: U256,
    pub token_is_contract: bool,
    /// SELL: bot's balance of the token being sold.
    pub bot_token: Option<U256>,
    /// PROPORTIONAL: source wallet's ETH + WETH right before the source tx.
    pub source_balance: Option<U256>,
    /// FIXED_USD only.
    pub eth_usd: Option<EthUsdPrice>,
    /// Wall clock (unix seconds) at fetch time, for price staleness.
    pub now_unix: u64,
}
