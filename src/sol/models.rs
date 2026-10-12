use std::fmt;
use std::str::FromStr;
use std::time::Instant;

use solana_pubkey::Pubkey;
use solana_signature::Signature;

use crate::sol::pump::Market;

/// The parts of an executed transaction the decoder needs. Built from a Yellowstone update
/// (or from RPC JSON in tests), so the decoder does not depend on either format.
#[derive(Debug, Clone)]
pub struct SourceTx {
    pub signature: Signature,
    pub slot: u64,
    /// Static keys, then address-table-loaded writable, then readonly keys (runtime order,
    /// which is what instruction account indexes refer to).
    pub account_keys: Vec<Pubkey>,
    pub num_signers: usize,
    /// Top-level instructions followed by all inner (CPI) instructions, flattened. pump
    /// programs emit their events as inner self-invocations (Anchor `emit_cpi!`).
    pub instructions: Vec<Ix>,
    pub pre_token_balances: Vec<TokenBalance>,
    pub post_token_balances: Vec<TokenBalance>,
    pub seen_at: Instant,
}

#[derive(Debug, Clone)]
pub struct Ix {
    pub program_id_index: usize,
    /// Indexes into `SourceTx::account_keys`.
    pub accounts: Vec<u8>,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct TokenBalance {
    pub account_index: usize,
    pub mint: Pubkey,
    pub owner: Option<Pubkey>,
    pub amount: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TradeSide {
    /// SOL -> token.
    Buy,
    /// Token -> SOL.
    Sell,
}

/// Configurable venue (SOL_VENUES).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VenueKind {
    /// pump.fun bonding curve (before migration).
    PumpFun,
    /// PumpSwap AMM (after migration).
    PumpSwap,
}

impl fmt::Display for VenueKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            VenueKind::PumpFun => "PUMP_FUN",
            VenueKind::PumpSwap => "PUMP_SWAP",
        })
    }
}

impl FromStr for VenueKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_uppercase().as_str() {
            "PUMP_FUN" => Ok(VenueKind::PumpFun),
            "PUMP_SWAP" => Ok(VenueKind::PumpSwap),
            other => Err(format!("unknown venue `{other}`")),
        }
    }
}

/// Where the source traded. The bot trades on the same venue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Venue {
    BondingCurve,
    PumpSwap { pool: Pubkey },
}

impl Venue {
    pub fn kind(&self) -> VenueKind {
        match self {
            Venue::BondingCurve => VenueKind::PumpFun,
            Venue::PumpSwap { .. } => VenueKind::PumpSwap,
        }
    }
}

/// A watched wallet's pump trade, decoded from the program's own events.
#[derive(Debug, Clone)]
pub struct SolTradeIntent {
    pub signature: Signature,
    pub slot: u64,
    pub source_wallet: Pubkey,
    pub venue: Venue,
    pub side: TradeSide,
    pub mint: Pubkey,
    /// SOL leg in lamports as reported by the program (sizing input for RATIO only).
    pub source_sol: u64,
    pub source_tokens: u64,
    pub seen_at: Instant,
}

/// Balances read by the decision stage; the executor compares simulated post-state
/// against them to measure what the swap really pays out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreBalances {
    /// Bot wallet lamports + its WSOL ATA lamports (wrap/unwrap only moves SOL between
    /// the two, so their sum changes by exactly proceeds − fees − rent).
    pub sol_lamports: u64,
    pub base_tokens: u64,
}

#[derive(Debug, Clone)]
pub struct SolCopyTrade {
    pub intent: SolTradeIntent,
    /// BUY: lamports to spend. SELL: base token units to sell.
    pub amount_in: u64,
    pub capped: bool,
    pub market: Market,
    pub pre: PreBalances,
    pub decision_at: Instant,
}
