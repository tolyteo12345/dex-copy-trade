use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use alloy::primitives::{Address, U256};

use crate::eth::config::{Config, GasCapMode};
use crate::eth::models::{ChainSnapshot, CopyTrade, Dex, TradeIntent, TradeSide};
use crate::eth::storage::{DedupKey, DedupStore, StorageError};

/// Rough gas units used to reserve ETH for fees before real estimates exist (Phase 4/5
/// replace them with `eth_estimateGas`). Deliberately generous: a Uniswap swap is ~120-250k.
pub const SWAP_GAS_UNITS: u128 = 300_000;
pub const APPROVE_GAS_UNITS: u128 = 60_000;

#[derive(Debug, thiserror::Error)]
pub enum RiskError {
    #[error("source tx already processed (duplicate)")]
    Duplicate,
    #[error("dedup store unavailable: {0}")]
    Storage(#[from] StorageError),
    #[error("dex {0} is not in SUPPORTED_DEXES")]
    DexNotAllowed(Dex),
    #[error("zero address in {0}")]
    ZeroAddress(&'static str),
    #[error("token {0} is blacklisted")]
    Blacklisted(Address),
    #[error("invalid token {0}: {1}")]
    InvalidToken(Address, &'static str),
    #[error("cooldown: same token/side copied {elapsed_s}s ago (cooldown {cooldown_s}s)")]
    Cooldown { elapsed_s: u64, cooldown_s: u64 },
    #[error("trade size {amount} wei exceeds MAX_TRADE_ETH {max} wei")]
    MaxTradeExceeded { amount: U256, max: U256 },
    #[error("gas price {current} wei exceeds MAX_GAS_GWEI ({max} wei)")]
    GasTooHigh { current: u128, max: u128 },
    #[error("insufficient ETH: have {have} wei, need {need} wei (trade + gas reserve)")]
    InsufficientBalance { have: U256, need: U256 },
    #[error("insufficient ETH for gas: have {have} wei, need {need} wei")]
    InsufficientGas { have: U256, need: U256 },
}

#[derive(Debug, Clone)]
pub struct RiskConfig {
    pub chain_id: u64,
    pub supported_dexes: Vec<Dex>,
    pub token_blacklist: HashSet<Address>,
    pub max_trade_wei: U256,
    pub max_gas_wei: u128,
    /// Off in GAS_CAP_MODE=wait: the executor waits for gas instead of rejecting here.
    pub enforce_gas_cap: bool,
    pub cooldown: Duration,
}

impl RiskConfig {
    pub fn from_config(cfg: &Config) -> Self {
        Self {
            chain_id: cfg.chain_id,
            supported_dexes: cfg.supported_dexes.clone(),
            token_blacklist: cfg.token_blacklist.iter().copied().collect(),
            max_trade_wei: cfg.max_trade_wei,
            max_gas_wei: cfg.max_gas_wei,
            enforce_gas_cap: cfg.gas_cap_mode == GasCapMode::Skip,
            cooldown: cfg.trade_cooldown,
        }
    }
}

/// Pre-execution gate. `precheck` needs no chain data and runs first; `check` runs on the
/// sized trade plus a fresh `ChainSnapshot`.
///
/// Rules that need a quote (MAX_SLIPPAGE_BPS / price impact, MAX_TRADE_ETH for sells whose
/// ETH value is only known after quoting) run at the executor's post-quote checkpoint.
/// "Unsupported route" is enforced by the decoder (it never emits such intents).
pub struct RiskEngine<D: DedupStore> {
    cfg: RiskConfig,
    dedup: D,
    last_copied: HashMap<(Address, TradeSide), Instant>,
}

impl<D: DedupStore> RiskEngine<D> {
    pub fn new(cfg: RiskConfig, dedup: D) -> Self {
        Self {
            cfg,
            dedup,
            last_copied: HashMap::new(),
        }
    }

    /// Marks the source tx as processed: a tx is decided once, whatever the outcome.
    pub async fn precheck(&self, intent: &TradeIntent, now: Instant) -> Result<(), RiskError> {
        let key = DedupKey {
            chain_id: self.cfg.chain_id,
            tx_hash: intent.source_tx_hash,
        };
        if !self.dedup.mark_if_new(key).await? {
            return Err(RiskError::Duplicate);
        }
        if !self.cfg.supported_dexes.contains(&intent.dex) {
            return Err(RiskError::DexNotAllowed(intent.dex));
        }
        for (what, addr) in [
            ("token_in", intent.token_in),
            ("token_out", intent.token_out),
            ("source_wallet", intent.source_wallet),
        ] {
            if addr.is_zero() {
                return Err(RiskError::ZeroAddress(what));
            }
        }
        let token = intent.token();
        if self.cfg.token_blacklist.contains(&token) {
            return Err(RiskError::Blacklisted(token));
        }
        if let Some(at) = self.last_copied.get(&(token, intent.side)) {
            let elapsed = now.saturating_duration_since(*at);
            if elapsed < self.cfg.cooldown {
                return Err(RiskError::Cooldown {
                    elapsed_s: elapsed.as_secs(),
                    cooldown_s: self.cfg.cooldown.as_secs(),
                });
            }
        }
        Ok(())
    }

    pub fn check(&self, trade: &CopyTrade, chain: &ChainSnapshot) -> Result<(), RiskError> {
        let token = trade.token();
        if !chain.token_is_contract {
            return Err(RiskError::InvalidToken(token, "no contract code"));
        }
        if self.cfg.enforce_gas_cap && chain.gas_price > self.cfg.max_gas_wei {
            return Err(RiskError::GasTooHigh {
                current: chain.gas_price,
                max: self.cfg.max_gas_wei,
            });
        }
        // Phase 5 clamps maxFeePerGas to MAX_GAS_GWEI, so reserving 2x current price
        // (bounded by that cap) covers a base-fee jump without over-reserving.
        let reserve_price = U256::from(chain.gas_price.saturating_mul(2).min(self.cfg.max_gas_wei));
        match trade.side() {
            TradeSide::Buy => {
                if trade.amount_in > self.cfg.max_trade_wei {
                    return Err(RiskError::MaxTradeExceeded {
                        amount: trade.amount_in,
                        max: self.cfg.max_trade_wei,
                    });
                }
                let need = trade
                    .amount_in
                    .saturating_add(reserve_price * U256::from(SWAP_GAS_UNITS));
                if chain.bot_eth < need {
                    return Err(RiskError::InsufficientBalance {
                        have: chain.bot_eth,
                        need,
                    });
                }
            }
            TradeSide::Sell => {
                let need = reserve_price * U256::from(SWAP_GAS_UNITS + APPROVE_GAS_UNITS);
                if chain.bot_eth < need {
                    return Err(RiskError::InsufficientGas {
                        have: chain.bot_eth,
                        need,
                    });
                }
            }
        }
        Ok(())
    }

    /// Starts the cooldown for this token/side; call once a trade is approved.
    pub fn record_approved(&mut self, trade: &CopyTrade, now: Instant) {
        if !self.cfg.cooldown.is_zero() {
            self.last_copied.insert((trade.token(), trade.side()), now);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eth::models::test_util::{TOKEN, intent};
    use crate::eth::storage::{DEDUP_TTL, InMemoryDedup};
    use crate::eth::strategy::test_util::snapshot;

    const ETH: u128 = 1_000_000_000_000_000_000;
    const GWEI: u128 = 1_000_000_000;

    fn engine() -> RiskEngine<InMemoryDedup> {
        RiskEngine::new(
            RiskConfig {
                chain_id: 1,
                supported_dexes: vec![Dex::UniswapV2, Dex::UniswapV3],
                token_blacklist: HashSet::new(),
                max_trade_wei: U256::from(ETH / 2),
                max_gas_wei: 100 * GWEI,
                enforce_gas_cap: true,
                cooldown: Duration::from_secs(30),
            },
            InMemoryDedup::new(DEDUP_TTL),
        )
    }

    fn trade(side: TradeSide, amount: u128) -> CopyTrade {
        CopyTrade {
            intent: intent(side, 1),
            amount_in: U256::from(amount),
            capped: false,
            decision_at: Instant::now(),
        }
    }

    fn with_hash(mut i: TradeIntent, n: u8) -> TradeIntent {
        i.source_tx_hash = alloy::primitives::B256::repeat_byte(n);
        i
    }

    #[tokio::test]
    async fn duplicate_source_tx_is_rejected() {
        let e = engine();
        let i = intent(TradeSide::Buy, ETH);
        let now = Instant::now();
        assert!(e.precheck(&i, now).await.is_ok());
        assert!(matches!(
            e.precheck(&i, now).await,
            Err(RiskError::Duplicate)
        ));
    }

    #[tokio::test]
    async fn dex_whitelist_blacklist_and_zero_address() {
        let mut e = engine();
        let now = Instant::now();

        let mut ur = with_hash(intent(TradeSide::Buy, ETH), 1);
        ur.dex = Dex::UniswapUniversalRouter;
        assert!(matches!(
            e.precheck(&ur, now).await,
            Err(RiskError::DexNotAllowed(Dex::UniswapUniversalRouter))
        ));

        let mut zero = with_hash(intent(TradeSide::Buy, ETH), 2);
        zero.token_out = Address::ZERO;
        assert!(matches!(
            e.precheck(&zero, now).await,
            Err(RiskError::ZeroAddress("token_out"))
        ));

        e.cfg.token_blacklist.insert(TOKEN);
        let black = with_hash(intent(TradeSide::Sell, 5), 3);
        assert!(matches!(
            e.precheck(&black, now).await,
            Err(RiskError::Blacklisted(t)) if t == TOKEN
        ));
    }

    #[tokio::test]
    async fn cooldown_is_per_token_and_side() {
        let mut e = engine();
        let t0 = Instant::now();
        e.record_approved(&trade(TradeSide::Buy, 1), t0);

        let buy = with_hash(intent(TradeSide::Buy, ETH), 1);
        assert!(matches!(
            e.precheck(&buy, t0 + Duration::from_secs(10)).await,
            Err(RiskError::Cooldown {
                elapsed_s: 10,
                cooldown_s: 30
            })
        ));
        // A sell of the same token is not blocked by a recent buy (exits must stay possible).
        let sell = with_hash(intent(TradeSide::Sell, 5), 2);
        assert!(
            e.precheck(&sell, t0 + Duration::from_secs(10))
                .await
                .is_ok()
        );

        let later = with_hash(intent(TradeSide::Buy, ETH), 3);
        assert!(
            e.precheck(&later, t0 + Duration::from_secs(31))
                .await
                .is_ok()
        );
    }

    #[test]
    fn max_trade_is_enforced_for_buys() {
        let e = engine();
        assert!(
            e.check(&trade(TradeSide::Buy, ETH / 2), &snapshot())
                .is_ok()
        );
        assert!(matches!(
            e.check(&trade(TradeSide::Buy, ETH / 2 + 1), &snapshot()),
            Err(RiskError::MaxTradeExceeded { .. })
        ));
    }

    #[test]
    fn gas_price_cap() {
        let e = engine();
        let mut chain = snapshot();
        chain.gas_price = 100 * GWEI;
        assert!(e.check(&trade(TradeSide::Buy, 1), &chain).is_ok());
        chain.gas_price = 100 * GWEI + 1;
        assert!(matches!(
            e.check(&trade(TradeSide::Buy, 1), &chain),
            Err(RiskError::GasTooHigh { .. })
        ));

        let mut wait_mode = engine();
        wait_mode.cfg.enforce_gas_cap = false;
        assert!(wait_mode.check(&trade(TradeSide::Buy, 1), &chain).is_ok());
    }

    #[test]
    fn balance_must_cover_trade_plus_gas_reserve() {
        let e = engine();
        let mut chain = snapshot(); // gas 1 gwei -> reserve price 2 gwei
        let reserve = 2 * GWEI * SWAP_GAS_UNITS;
        chain.bot_eth = U256::from(ETH / 10 + reserve);
        assert!(e.check(&trade(TradeSide::Buy, ETH / 10), &chain).is_ok());
        chain.bot_eth -= U256::from(1u64);
        assert!(matches!(
            e.check(&trade(TradeSide::Buy, ETH / 10), &chain),
            Err(RiskError::InsufficientBalance { .. })
        ));
    }

    #[test]
    fn sells_need_gas_for_approve_and_swap() {
        let e = engine();
        let mut chain = snapshot();
        let need = 2 * GWEI * (SWAP_GAS_UNITS + APPROVE_GAS_UNITS);
        chain.bot_eth = U256::from(need);
        assert!(e.check(&trade(TradeSide::Sell, 10), &chain).is_ok());
        chain.bot_eth = U256::from(need - 1);
        assert!(matches!(
            e.check(&trade(TradeSide::Sell, 10), &chain),
            Err(RiskError::InsufficientGas { .. })
        ));
    }

    #[test]
    fn token_without_code_is_invalid() {
        let e = engine();
        let mut chain = snapshot();
        chain.token_is_contract = false;
        assert!(matches!(
            e.check(&trade(TradeSide::Buy, 1), &chain),
            Err(RiskError::InvalidToken(..))
        ));
    }
}
