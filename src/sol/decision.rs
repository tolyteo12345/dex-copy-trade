//! Strategy + risk stage: SolTradeIntent -> SolCopyTrade (or a logged rejection).

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use solana_pubkey::Pubkey;
use solana_signature::Signature;
use tokio::sync::mpsc;

use crate::common::dedup::TtlSet;
use crate::common::env::BPS_DENOMINATOR;
use crate::sol::config::{SolConfig, SolStrategy};
use crate::sol::models::{PreBalances, SolCopyTrade, SolTradeIntent, TradeSide, Venue, VenueKind};
use crate::sol::notify::{Notifier, SolNotification};
use crate::sol::pump::{
    self, LayoutError, Market, PUMP_AMM_PROGRAM, PUMP_PROGRAM, PoolVaults, TOKEN_2022_PROGRAM,
    TOKEN_PROGRAM, WSOL_MINT,
};
use crate::sol::rpc::{AccountInfo, RpcError, SolRpc};

#[derive(Debug, thiserror::Error)]
pub enum Rejection {
    #[error("source tx already processed (duplicate)")]
    Duplicate,
    #[error("venue {0} is not in SOL_VENUES")]
    VenueNotAllowed(VenueKind),
    #[error("token {0} is blacklisted")]
    Blacklisted(Pubkey),
    #[error("cooldown: same token/side copied {elapsed_s}s ago (cooldown {cooldown_s}s)")]
    Cooldown { elapsed_s: u64, cooldown_s: u64 },
    #[error("chain data unavailable: {0}")]
    Rpc(#[from] RpcError),
    #[error("unsupported market: {0}")]
    Unsupported(&'static str),
    #[error(transparent)]
    Layout(#[from] LayoutError),
    #[error("bot holds none of the token being sold")]
    NothingToSell,
    #[error("computed trade size is zero")]
    ZeroAmount,
    #[error("insufficient SOL: have {have} lamports, need {need} (trade + SOL_FEE_RESERVE_SOL)")]
    InsufficientBalance { have: u64, need: u64 },
}

#[derive(Debug, Clone)]
pub struct DecisionConfig {
    pub bot: Pubkey,
    pub strategy: SolStrategy,
    pub sell_ratio_bps: u32,
    pub max_trade_lamports: u64,
    pub fee_reserve_lamports: u64,
    pub venues: Vec<VenueKind>,
    pub token_blacklist: HashSet<Pubkey>,
    pub cooldown: Duration,
}

impl DecisionConfig {
    pub fn from_config(cfg: &SolConfig) -> Self {
        Self {
            bot: cfg.signer.pubkey(),
            strategy: cfg.strategy,
            sell_ratio_bps: cfg.sell_ratio_bps,
            max_trade_lamports: cfg.max_trade_lamports,
            fee_reserve_lamports: cfg.fee_reserve_lamports,
            venues: cfg.venues.clone(),
            token_blacklist: cfg.token_blacklist.iter().copied().collect(),
            cooldown: cfg.trade_cooldown,
        }
    }
}

pub struct DecisionStage<R: SolRpc> {
    rpc: R,
    cfg: DecisionConfig,
    dedup: TtlSet<Signature>,
    last_copied: HashMap<(Pubkey, TradeSide), Instant>,
}

impl<R: SolRpc> DecisionStage<R> {
    pub fn new(rpc: R, cfg: DecisionConfig, dedup: TtlSet<Signature>) -> Self {
        Self {
            rpc,
            cfg,
            dedup,
            last_copied: HashMap::new(),
        }
    }

    /// The source tx is marked processed in `precheck`, so a transient RPC failure later
    /// drops the trade rather than retrying it (a retried copy would be stale anyway).
    pub async fn decide(&mut self, intent: SolTradeIntent) -> Result<SolCopyTrade, Rejection> {
        let now = Instant::now();
        self.precheck(&intent, now)?;

        // One round-trip for everything: both possible base ATAs are fetched because the
        // mint's token program (SPL vs Token-2022) is only known from the same response.
        let bot = self.cfg.bot;
        let venue_account = match intent.venue {
            Venue::BondingCurve => pump::bonding_curve(&intent.mint),
            Venue::PumpSwap { pool } => pool,
        };
        let keys = [
            bot,
            pump::ata(&bot, &WSOL_MINT, &TOKEN_PROGRAM),
            intent.mint,
            venue_account,
            pump::ata(&bot, &intent.mint, &TOKEN_PROGRAM),
            pump::ata(&bot, &intent.mint, &TOKEN_2022_PROGRAM),
        ];
        let accounts = self.rpc.get_multiple_accounts(&keys).await?;
        let get = |i: usize| accounts.get(i).and_then(Option::as_ref);
        let lamports = |i: usize| get(i).map_or(0, |a| a.lamports);

        let mint = get(2).ok_or(Rejection::Unsupported("mint account not found"))?;
        let base_token_program = mint.owner;
        let base_ata = match base_token_program {
            TOKEN_PROGRAM => get(4),
            TOKEN_2022_PROGRAM => get(5),
            _ => return Err(Rejection::Unsupported("mint is not an SPL token")),
        };
        let market = resolve_market(&intent, base_token_program, get(3))?;
        let pre = PreBalances {
            sol_lamports: lamports(0).saturating_add(lamports(1)),
            base_tokens: base_ata
                .and_then(|a| pump::token_account_amount(&a.data))
                .unwrap_or(0),
        };

        let (amount_in, capped) = self.size(&intent, pre.base_tokens)?;
        let need = match intent.side {
            TradeSide::Buy => amount_in.saturating_add(self.cfg.fee_reserve_lamports),
            TradeSide::Sell => self.cfg.fee_reserve_lamports,
        };
        let have = lamports(0);
        if have < need {
            return Err(Rejection::InsufficientBalance { have, need });
        }

        if !self.cfg.cooldown.is_zero() {
            self.last_copied.insert((intent.mint, intent.side), now);
        }
        Ok(SolCopyTrade {
            intent,
            amount_in,
            capped,
            market,
            pre,
            decision_at: Instant::now(),
        })
    }

    /// No chain data needed. Marks the signature first: a tx is decided once, whatever
    /// the outcome.
    fn precheck(&self, intent: &SolTradeIntent, now: Instant) -> Result<(), Rejection> {
        if !self.dedup.mark_if_new(intent.signature) {
            return Err(Rejection::Duplicate);
        }
        let kind = intent.venue.kind();
        if !self.cfg.venues.contains(&kind) {
            return Err(Rejection::VenueNotAllowed(kind));
        }
        if self.cfg.token_blacklist.contains(&intent.mint) {
            return Err(Rejection::Blacklisted(intent.mint));
        }
        if let Some(at) = self.last_copied.get(&(intent.mint, intent.side)) {
            let elapsed = now.saturating_duration_since(*at);
            if elapsed < self.cfg.cooldown {
                return Err(Rejection::Cooldown {
                    elapsed_s: elapsed.as_secs(),
                    cooldown_s: self.cfg.cooldown.as_secs(),
                });
            }
        }
        Ok(())
    }

    /// BUY: lamports to spend (capped at SOL_MAX_TRADE_SOL). SELL: base units to sell.
    fn size(&self, intent: &SolTradeIntent, bot_tokens: u64) -> Result<(u64, bool), Rejection> {
        match intent.side {
            TradeSide::Buy => {
                let raw = match self.cfg.strategy {
                    SolStrategy::FixedSol { lamports } => lamports,
                    SolStrategy::Ratio { bps } => mul_bps(intent.source_sol, bps),
                };
                if raw == 0 {
                    return Err(Rejection::ZeroAmount);
                }
                let max = self.cfg.max_trade_lamports;
                Ok((raw.min(max), raw > max))
            }
            TradeSide::Sell => {
                if bot_tokens == 0 {
                    return Err(Rejection::NothingToSell);
                }
                let amount = mul_bps(bot_tokens, self.cfg.sell_ratio_bps);
                if amount == 0 {
                    return Err(Rejection::ZeroAmount);
                }
                Ok((amount, false))
            }
        }
    }
}

/// Checks the venue account and extracts what the executor needs to build the swap.
fn resolve_market(
    intent: &SolTradeIntent,
    base_token_program: Pubkey,
    venue: Option<&AccountInfo>,
) -> Result<Market, Rejection> {
    let venue = venue.ok_or(Rejection::Unsupported("venue account not found"))?;
    let pool = match intent.venue {
        Venue::BondingCurve => {
            if venue.owner != PUMP_PROGRAM {
                return Err(Rejection::Unsupported("bonding curve not owned by pump"));
            }
            let curve = pump::parse_bonding_curve(&venue.data)?;
            if curve.complete {
                return Err(Rejection::Unsupported("bonding curve complete (migrating)"));
            }
            if !curve.is_sol_paired() {
                return Err(Rejection::Unsupported("coin is not paired with SOL"));
            }
            if curve.is_cashback_coin {
                return Err(Rejection::Unsupported(
                    "cashback coin (v3 trades not supported)",
                ));
            }
            None
        }
        Venue::PumpSwap { pool } => {
            if venue.owner != PUMP_AMM_PROGRAM {
                return Err(Rejection::Unsupported("pool not owned by PumpSwap"));
            }
            let state = pump::parse_pool(&venue.data)?;
            if state.base_mint != intent.mint || state.quote_mint != WSOL_MINT {
                return Err(Rejection::Unsupported("pool is not <coin>/WSOL"));
            }
            if state.is_cashback_coin {
                return Err(Rejection::Unsupported(
                    "cashback pool (v2 trades not supported)",
                ));
            }
            Some(PoolVaults {
                pool,
                base_vault: state.base_vault,
                quote_vault: state.quote_vault,
            })
        }
    };
    Ok(Market {
        mint: intent.mint,
        base_token_program,
        pool,
    })
}

/// `value * bps / 10_000`, rounding down.
fn mul_bps(value: u64, bps: u32) -> u64 {
    let v = u128::from(value) * u128::from(bps) / u128::from(BPS_DENOMINATOR);
    u64::try_from(v).unwrap_or(u64::MAX)
}

/// Processes intents one at a time on purpose: cooldown and balance checks would race if
/// two decisions ran concurrently. Each decision costs one RPC round-trip.
pub async fn run_stage<R: SolRpc>(
    mut stage: DecisionStage<R>,
    mut rx: mpsc::Receiver<SolTradeIntent>,
    out: mpsc::Sender<SolCopyTrade>,
    notifier: Notifier,
) {
    while let Some(intent) = rx.recv().await {
        let (signature, wallet, side, mint, venue, seen_at) = (
            intent.signature,
            intent.source_wallet,
            intent.side,
            intent.mint,
            intent.venue.kind(),
            intent.seen_at,
        );
        match stage.decide(intent).await {
            Ok(trade) => {
                tracing::info!(
                    event = "trade_approved",
                    %signature,
                    source_wallet = %wallet,
                    %venue,
                    side = ?side,
                    %mint,
                    amount_in = trade.amount_in,
                    capped = trade.capped,
                    seen_to_decision_ms = (trade.decision_at - seen_at).as_secs_f64() * 1000.0,
                );
                notifier.notify(SolNotification::Signal {
                    source_wallet: wallet,
                    signature,
                    venue,
                    side,
                    mint,
                    amount_in: trade.amount_in,
                    capped: trade.capped,
                });
                if out.send(trade).await.is_err() {
                    break;
                }
            }
            Err(Rejection::Duplicate) => {
                tracing::debug!(event = "trade_rejected", %signature, reason = "duplicate");
            }
            Err(reason) => tracing::info!(
                event = "trade_rejected",
                %signature,
                source_wallet = %wallet,
                %venue,
                side = ?side,
                %mint,
                reason = %reason,
            ),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Arc;

    use parking_lot::Mutex;
    use solana_hash::Hash;
    use solana_transaction::versioned::VersionedTransaction;

    use super::*;
    use crate::common::dedup::DEDUP_TTL;
    use crate::sol::rpc::{SigStatus, SimResult};

    pub const SOL: u64 = 1_000_000_000;

    /// Accounts by address; everything else does not exist.
    #[derive(Clone, Default)]
    pub struct MockRpc {
        pub accounts: Arc<Mutex<HashMap<Pubkey, AccountInfo>>>,
        pub calls: Arc<Mutex<u32>>,
    }

    impl SolRpc for MockRpc {
        async fn get_multiple_accounts(
            &self,
            keys: &[Pubkey],
        ) -> Result<Vec<Option<AccountInfo>>, RpcError> {
            *self.calls.lock() += 1;
            let map = self.accounts.lock();
            Ok(keys.iter().map(|k| map.get(k).cloned()).collect())
        }
        async fn latest_blockhash(&self) -> Result<Hash, RpcError> {
            unimplemented!()
        }
        async fn simulate(
            &self,
            _: &VersionedTransaction,
            _: &[Pubkey],
        ) -> Result<SimResult, RpcError> {
            unimplemented!()
        }
        async fn send(&self, _: &VersionedTransaction) -> Result<Signature, RpcError> {
            unimplemented!()
        }
        async fn signature_status(&self, _: &Signature) -> Result<Option<SigStatus>, RpcError> {
            unimplemented!()
        }
    }

    pub fn token_account(amount: u64) -> AccountInfo {
        let mut data = vec![0u8; 165];
        data[64..72].copy_from_slice(&amount.to_le_bytes());
        AccountInfo {
            lamports: 2_039_280,
            owner: TOKEN_2022_PROGRAM,
            data,
        }
    }

    pub fn curve_account(complete: bool) -> AccountInfo {
        let mut data = vec![0u8; 151];
        data[..8].copy_from_slice(&[23, 183, 248, 55, 96, 216, 172, 96]);
        data[48] = u8::from(complete);
        AccountInfo {
            lamports: 1,
            owner: PUMP_PROGRAM,
            data,
        }
    }

    pub fn intent(side: TradeSide, mint: Pubkey, venue: Venue) -> SolTradeIntent {
        SolTradeIntent {
            signature: Signature::from([7u8; 64]),
            slot: 1,
            source_wallet: Pubkey::new_unique(),
            venue,
            side,
            mint,
            source_sol: 2 * SOL,
            source_tokens: 1_000,
            seen_at: Instant::now(),
        }
    }

    fn cfg(bot: Pubkey) -> DecisionConfig {
        DecisionConfig {
            bot,
            strategy: SolStrategy::FixedSol { lamports: SOL / 10 },
            sell_ratio_bps: 5_000,
            max_trade_lamports: SOL / 2,
            fee_reserve_lamports: SOL / 100,
            venues: vec![VenueKind::PumpFun, VenueKind::PumpSwap],
            token_blacklist: HashSet::new(),
            cooldown: Duration::from_secs(30),
        }
    }

    /// Bot with `sol` lamports, a Token-2022 mint on a live curve, bot holding `tokens`.
    fn setup(sol: u64, tokens: Option<u64>) -> (MockRpc, Pubkey, Pubkey) {
        let (bot, mint) = (Pubkey::new_unique(), Pubkey::new_unique());
        let rpc = MockRpc::default();
        {
            let mut m = rpc.accounts.lock();
            m.insert(
                bot,
                AccountInfo {
                    lamports: sol,
                    owner: pump::SYSTEM_PROGRAM,
                    data: vec![],
                },
            );
            m.insert(
                mint,
                AccountInfo {
                    lamports: 1,
                    owner: TOKEN_2022_PROGRAM,
                    data: vec![0; 82],
                },
            );
            m.insert(pump::bonding_curve(&mint), curve_account(false));
            if let Some(t) = tokens {
                m.insert(
                    pump::ata(&bot, &mint, &TOKEN_2022_PROGRAM),
                    token_account(t),
                );
            }
        }
        (rpc, bot, mint)
    }

    #[tokio::test]
    async fn buy_is_sized_capped_and_resolves_market() {
        let (rpc, bot, mint) = setup(SOL, None);
        let mut c = cfg(bot);
        c.strategy = SolStrategy::Ratio { bps: 5_000 };
        let mut stage = DecisionStage::new(rpc.clone(), c, TtlSet::new(DEDUP_TTL));
        let t = stage
            .decide(intent(TradeSide::Buy, mint, Venue::BondingCurve))
            .await
            .unwrap();
        // 50% of the source's 2 SOL = 1 SOL, capped to 0.5.
        assert_eq!((t.amount_in, t.capped), (SOL / 2, true));
        assert_eq!(t.market.base_token_program, TOKEN_2022_PROGRAM);
        assert_eq!(t.market.pool, None);
        assert_eq!(
            t.pre,
            PreBalances {
                sol_lamports: SOL,
                base_tokens: 0
            }
        );
        assert_eq!(*rpc.calls.lock(), 1, "one RPC round-trip per decision");
    }

    #[tokio::test]
    async fn duplicate_and_cooldown_are_rejected_without_rpc() {
        let (rpc, bot, mint) = setup(SOL, None);
        let mut stage = DecisionStage::new(rpc.clone(), cfg(bot), TtlSet::new(DEDUP_TTL));
        let i = intent(TradeSide::Buy, mint, Venue::BondingCurve);
        stage.decide(i.clone()).await.unwrap();
        assert!(matches!(
            stage.decide(i.clone()).await,
            Err(Rejection::Duplicate)
        ));
        let mut again = i;
        again.signature = Signature::from([8u8; 64]);
        assert!(matches!(
            stage.decide(again).await,
            Err(Rejection::Cooldown { .. })
        ));
        assert_eq!(*rpc.calls.lock(), 1);
    }

    #[tokio::test]
    async fn sell_uses_bot_balance_and_rejects_when_empty() {
        let (rpc, bot, mint) = setup(SOL, Some(1_001));
        let mut stage = DecisionStage::new(rpc, cfg(bot), TtlSet::new(DEDUP_TTL));
        let t = stage
            .decide(intent(TradeSide::Sell, mint, Venue::BondingCurve))
            .await
            .unwrap();
        assert_eq!(t.amount_in, 500, "50% of 1001, rounded down");
        assert_eq!(t.pre.base_tokens, 1_001);

        let (rpc, bot, mint) = setup(SOL, None);
        let mut stage = DecisionStage::new(rpc, cfg(bot), TtlSet::new(DEDUP_TTL));
        assert!(matches!(
            stage
                .decide(intent(TradeSide::Sell, mint, Venue::BondingCurve))
                .await,
            Err(Rejection::NothingToSell)
        ));
    }

    #[tokio::test]
    async fn balance_must_cover_trade_and_fee_reserve() {
        let (rpc, bot, mint) = setup(SOL / 10, None);
        let mut stage = DecisionStage::new(rpc, cfg(bot), TtlSet::new(DEDUP_TTL));
        match stage
            .decide(intent(TradeSide::Buy, mint, Venue::BondingCurve))
            .await
        {
            Err(Rejection::InsufficientBalance { have, need }) => {
                assert_eq!((have, need), (SOL / 10, SOL / 10 + SOL / 100));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn completed_curve_blacklist_and_venue_filter() {
        let (rpc, bot, mint) = setup(SOL, None);
        rpc.accounts
            .lock()
            .insert(pump::bonding_curve(&mint), curve_account(true));
        let mut stage = DecisionStage::new(rpc.clone(), cfg(bot), TtlSet::new(DEDUP_TTL));
        assert!(matches!(
            stage
                .decide(intent(TradeSide::Buy, mint, Venue::BondingCurve))
                .await,
            Err(Rejection::Unsupported(_))
        ));

        let mut c = cfg(bot);
        c.token_blacklist.insert(mint);
        c.venues = vec![VenueKind::PumpFun];
        let mut stage = DecisionStage::new(rpc, c, TtlSet::new(DEDUP_TTL));
        let mut i = intent(TradeSide::Buy, mint, Venue::BondingCurve);
        assert!(matches!(
            stage.decide(i.clone()).await,
            Err(Rejection::Blacklisted(_))
        ));
        i.signature = Signature::from([9u8; 64]);
        i.venue = Venue::PumpSwap {
            pool: Pubkey::new_unique(),
        };
        assert!(matches!(
            stage.decide(i).await,
            Err(Rejection::VenueNotAllowed(VenueKind::PumpSwap))
        ));
    }

    #[tokio::test]
    async fn pool_must_be_coin_over_wsol() {
        let (rpc, bot, mint) = setup(SOL, None);
        let pool = Pubkey::new_unique();
        let mut data = vec![0u8; 300];
        data[..8].copy_from_slice(&[241, 154, 109, 4, 17, 177, 109, 188]);
        data[43..75].copy_from_slice(mint.as_ref());
        data[75..107].copy_from_slice(WSOL_MINT.as_ref());
        let (bv, qv) = (Pubkey::new_unique(), Pubkey::new_unique());
        data[139..171].copy_from_slice(bv.as_ref());
        data[171..203].copy_from_slice(qv.as_ref());
        let account = |data: Vec<u8>| AccountInfo {
            lamports: 1,
            owner: PUMP_AMM_PROGRAM,
            data,
        };
        rpc.accounts.lock().insert(pool, account(data.clone()));
        let mut stage = DecisionStage::new(rpc.clone(), cfg(bot), TtlSet::new(DEDUP_TTL));
        let t = stage
            .decide(intent(TradeSide::Buy, mint, Venue::PumpSwap { pool }))
            .await
            .unwrap();
        assert_eq!(
            t.market.pool,
            Some(PoolVaults {
                pool,
                base_vault: bv,
                quote_vault: qv
            })
        );

        // Pool quoted in something other than WSOL.
        data[75..107].copy_from_slice(Pubkey::new_unique().as_ref());
        rpc.accounts.lock().insert(pool, account(data));
        let mut stage = DecisionStage::new(rpc, cfg(bot), TtlSet::new(DEDUP_TTL));
        assert!(matches!(
            stage
                .decide(intent(TradeSide::Buy, mint, Venue::PumpSwap { pool }))
                .await,
            Err(Rejection::Unsupported(_))
        ));
    }
}
