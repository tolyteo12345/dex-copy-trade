pub mod allowance;
pub mod broadcast;
pub mod executor;
pub mod gas;
pub mod simulator;

use alloy::primitives::U256;

use crate::dex::DexError;
use crate::ethereum::provider::ProviderError;

#[derive(Debug, thiserror::Error)]
pub enum SimulationError {
    #[error("simulation rpc failed: {0}")]
    Rpc(#[from] ProviderError),
    #[error("simulation returned {got} results for {expected} calls")]
    ResultCount { expected: usize, got: usize },
    #[error("{step} reverted: {reason}")]
    Reverted { step: &'static str, reason: String },
}

#[derive(Debug, thiserror::Error)]
pub enum ExecutionError {
    #[error("no executable route: {0}")]
    NoRoute(&'static str),
    #[error(transparent)]
    Dex(#[from] DexError),
    #[error("chain data unavailable: {0}")]
    Chain(ProviderError),
    #[error(transparent)]
    Simulation(#[from] SimulationError),
    #[error("source tx reverts in simulation (would not copy a failing trade): {0}")]
    SourceReverts(String),
    #[error("no route could be quoted: {0}")]
    NoQuote(String),
    #[error("price impact {impact_bps} bps exceeds MAX_SLIPPAGE_BPS {max_bps}")]
    PriceImpact { impact_bps: u32, max_bps: u32 },
    #[error("cannot measure price impact (probe quote is zero)")]
    ImpactUnmeasurable,
    #[error("min output {min_out} is zero after slippage")]
    ZeroQuote { min_out: U256 },
}
