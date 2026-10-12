//! Solana copy-trading pipeline for pump.fun (bonding curve + PumpSwap), binary
//! `sol-copy-trade`. Runs as its own process; shares only `crate::common` with ETH.
//!
//! ```text
//! watcher   Yellowstone gRPC (processed, successful txs mentioning COPY wallets) → SourceTx
//! decoder   pump events (TradeEvent / BuyEvent / SellEvent) → SolTradeIntent
//! decision  precheck (no RPC) → 1× getMultipleAccounts → sizing + risk → SolCopyTrade
//! executor  build tx → simulateTransaction (measure received) → [live] sign + send → track
//! ```

pub mod config;
pub mod decision;
pub mod decoder;
pub mod executor;
pub mod models;
pub mod notify;
pub mod pump;
pub mod rpc;
pub mod signer;
pub mod watcher;
