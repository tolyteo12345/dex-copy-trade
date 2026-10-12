//! CopyTrade -> route choice -> quote -> post-quote risk -> approvals -> swap -> simulation.
//! The stage then signs + broadcasts the plan (unless DRY_RUN) and tracks the receipt.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use alloy::primitives::{Address, U256};
use alloy::rpc::types::TransactionRequest;
use alloy::sol;
use alloy::sol_types::{SolCall, SolValue};
use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinSet;

use super::allowance::{approval_txs, permit2_approval_txs};
use super::broadcast::{Broadcaster, SentTrade, TxSender};
use super::gas::GasStrategy;
use super::simulator::{CallOutcome, Simulator};
use super::{ExecutionError, SimulationError};
use crate::config::BPS_DENOMINATOR;
use crate::dex::uniswap_v2::UniswapV2Adapter;
use crate::dex::uniswap_v3::UniswapV3Adapter;
use crate::dex::uniswap_v4::UniswapV4Adapter;
use crate::dex::{DexAdapter, DexError, SwapParams};
use crate::ethereum::chain_state::{ChainReader, IERC20};
use crate::models::{CopyTrade, PoolKind, RouteLeg, TradeIntent, TradeSide};
use crate::notify::{Notification, Notifier};
use crate::strategy::mul_bps;
use crate::utils::ChainAddresses;

pub const DEADLINE_SECS: u64 = 120;
/// Price impact = our quote vs a quote for 1/PROBE_DIVISOR of the amount (≈ spot price).
const PROBE_DIVISOR: u64 = 1_000;

#[derive(Debug, Clone)]
pub struct ExecutorConfig {
    pub bot: Address,
    pub max_slippage_bps: u32,
    pub max_trade_wei: U256,
}

#[derive(Debug, Clone)]
pub struct PlannedTx {
    pub step: &'static str,
    pub request: TransactionRequest,
    /// Simulated gas after refunds (for cost reporting).
    pub gas_used: u64,
    /// Simulated peak gas before refunds (for the gas limit).
    pub peak_gas: u64,
}

#[derive(Debug, Clone)]
pub struct ExecutionPlan {
    pub trade: CopyTrade,
    pub route: RouteLeg,
    /// Below `trade.amount_in` when a SELL was capped to MAX_TRADE_ETH.
    pub amount_in: U256,
    pub sell_capped: bool,
    /// Router quote (pre transfer tax).
    pub quoted_out: U256,
    /// Output the bot actually received in simulation (post transfer tax).
    pub expected_out: U256,
    /// How far `expected_out` fell short of `quoted_out` (token transfer tax).
    pub transfer_tax_bps: u32,
    pub min_out: U256,
    pub price_impact_bps: u32,
    /// Quote/simulation ran on top of the still-pending source tx.
    pub after_source: bool,
    /// Approvals (if any) then the swap, in send order.
    pub txs: Vec<PlannedTx>,
    pub simulated_at: Instant,
}

pub struct Executor<R: ChainReader, S: Simulator> {
    reader: R,
    sim: S,
    v2: UniswapV2Adapter,
    v3: UniswapV3Adapter,
    v4: UniswapV4Adapter,
    permit2: Address,
    multicall3: Address,
    cfg: ExecutorConfig,
}

sol! {
    interface IMulticall3 {
        function getEthBalance(address addr) external view returns (uint256 balance);
    }
}

#[derive(Default)]
struct Allowances {
    erc20: HashMap<Address, U256>,
    permit2: HashMap<Address, (U256, u64)>,
}

struct Quoted<'a> {
    route: &'a RouteLeg,
    expected_out: U256,
    impact_bps: Option<u32>,
}

impl<R: ChainReader, S: Simulator> Executor<R, S> {
    pub fn new(reader: R, sim: S, addrs: &ChainAddresses, cfg: ExecutorConfig) -> Self {
        Self {
            reader,
            sim,
            v2: UniswapV2Adapter::new(addrs),
            v3: UniswapV3Adapter::new(addrs),
            v4: UniswapV4Adapter::new(addrs),
            permit2: addrs.permit2,
            multicall3: addrs.multicall3,
            cfg,
        }
    }

    fn adapter(&self, pool: PoolKind) -> &dyn DexAdapter {
        match pool {
            PoolKind::UniswapV2 => &self.v2,
            PoolKind::UniswapV3 => &self.v3,
            PoolKind::UniswapV4 => &self.v4,
        }
    }

    pub async fn plan(&self, trade: CopyTrade) -> Result<ExecutionPlan, ExecutionError> {
        let intent = &trade.intent;
        let routes = candidate_routes(intent);
        if routes.is_empty() {
            return Err(ExecutionError::NoRoute(
                "no source leg goes directly from token_in to token_out",
            ));
        }

        let (source_call, allowances) = tokio::try_join!(
            self.pending_source_call(intent),
            self.allowances(intent, &routes),
        )
        .map_err(ExecutionError::Chain)?;
        let skip = usize::from(source_call.is_some());

        // 1. Quote every candidate (full amount + small probe) in one simulation.
        let amount = trade.amount_in;
        let probe = (amount / U256::from(PROBE_DIVISOR)).max(U256::from(1));
        let mut calls: Vec<TransactionRequest> = source_call.iter().cloned().collect();
        for r in &routes {
            let a = self.adapter(r.pool);
            calls.push(a.quote_call(r, amount)?);
            calls.push(a.quote_call(r, probe)?);
        }
        let out = self.sim.simulate(calls).await?;
        check_source(&out, skip)?;
        let best = self.best_quote(&routes, &out[skip..], amount, probe)?;

        // 2. Post-quote risk: price impact, then MAX_TRADE_ETH for sells.
        let impact_bps = best.impact_bps.ok_or(ExecutionError::ImpactUnmeasurable)?;
        if impact_bps > self.cfg.max_slippage_bps {
            return Err(ExecutionError::PriceImpact {
                impact_bps,
                max_bps: self.cfg.max_slippage_bps,
            });
        }
        let (amount_in, quoted_out, sell_capped) = cap_sell(
            intent.side,
            amount,
            best.expected_out,
            self.cfg.max_trade_wei,
        );
        if amount_in.is_zero() {
            return Err(ExecutionError::ZeroQuote {
                min_out: U256::ZERO,
            });
        }

        // 3. Simulate approvals + the swap with no output bound, reading the bot's balance
        //    of the output before/after. Quoters ignore transfer taxes; this measures what
        //    the bot really receives, so min_out also works for fee-on-transfer tokens.
        let route = best.route.clone();
        let adapter = self.adapter(route.pool);
        let bot = self.cfg.bot;
        let mut approvals: Vec<(&'static str, TransactionRequest)> = Vec::new();
        if intent.side == TradeSide::Sell {
            let erc20 = allowances.erc20.get(&adapter.spender()).copied();
            approvals.extend(approval_txs(
                intent.token_in,
                adapter.spender(),
                erc20.unwrap_or_default(),
                amount_in,
            ));
            if let Some(router) = adapter.permit2_spender() {
                let current = allowances.permit2.get(&router).copied().unwrap_or_default();
                approvals.extend(permit2_approval_txs(
                    self.permit2,
                    intent.token_in,
                    router,
                    current,
                    amount_in,
                    unix_now() + DEADLINE_SECS,
                ));
            }
        }
        for (_, tx) in &mut approvals {
            tx.from = Some(bot);
        }
        let deadline = U256::from(unix_now() + DEADLINE_SECS);
        let swap = |min_out| -> Result<TransactionRequest, ExecutionError> {
            let params = SwapParams {
                side: intent.side,
                amount_in,
                min_out,
                recipient: bot,
                deadline,
            };
            Ok(adapter.swap_tx(&route, &params)?.from(bot))
        };
        let balance = self.output_balance_call(intent);
        let calls = source_call
            .iter()
            .cloned()
            .chain(approvals.iter().map(|(_, tx)| tx.clone()))
            .chain([balance.clone(), swap(U256::ZERO)?, balance])
            .collect();
        let out = self.sim.simulate(calls).await?;
        check_source(&out, skip)?;
        let (approval_out, measured) = out[skip..].split_at(approvals.len());
        let mut txs = Vec::with_capacity(approvals.len() + 1);
        for ((step, request), result) in approvals.into_iter().zip(approval_out) {
            ensure_success(step, result)?;
            txs.push(PlannedTx {
                step,
                request,
                gas_used: result.gas_used,
                peak_gas: result.peak_gas,
            });
        }
        let [before, swap_result, after] = measured else {
            return Err(SimulationError::ResultCount {
                expected: 3,
                got: measured.len(),
            }
            .into());
        };
        ensure_success("swap", swap_result)?;
        let received = decode_u256(after)?.saturating_sub(decode_u256(before)?);
        let min_out = mul_bps(received, BPS_DENOMINATOR - self.cfg.max_slippage_bps);
        if min_out.is_zero() {
            return Err(ExecutionError::ZeroQuote { min_out });
        }
        // Same state + same call; only the min-output check differs, and min_out < received.
        txs.push(PlannedTx {
            step: "swap",
            request: swap(min_out)?,
            gas_used: swap_result.gas_used,
            peak_gas: swap_result.peak_gas,
        });

        Ok(ExecutionPlan {
            amount_in,
            sell_capped,
            quoted_out,
            expected_out: received,
            transfer_tax_bps: shortfall_bps(quoted_out, received),
            min_out,
            price_impact_bps: impact_bps,
            after_source: skip == 1,
            txs,
            route,
            trade,
            simulated_at: Instant::now(),
        })
    }

    /// Reads the bot's balance of what the trade outputs: the token on BUY, native ETH on SELL.
    fn output_balance_call(&self, intent: &TradeIntent) -> TransactionRequest {
        let (to, input) = match intent.side {
            TradeSide::Buy => (
                intent.token_out,
                IERC20::balanceOfCall {
                    owner: self.cfg.bot,
                }
                .abi_encode(),
            ),
            TradeSide::Sell => (
                self.multicall3,
                IMulticall3::getEthBalanceCall { addr: self.cfg.bot }.abi_encode(),
            ),
        };
        TransactionRequest::default().to(to).input(input.into())
    }

    /// While the source tx is pending, our tx will land after it: quote on top of it.
    /// Already mined (or replaced) => latest state already reflects it.
    async fn pending_source_call(
        &self,
        intent: &TradeIntent,
    ) -> Result<Option<TransactionRequest>, crate::ethereum::provider::ProviderError> {
        if intent.source_block.is_some() {
            return Ok(None);
        }
        let mined = self.reader.mined_nonce(intent.source_wallet).await?;
        if mined > intent.source_call.nonce {
            return Ok(None);
        }
        let s = &intent.source_call;
        Ok(Some(
            TransactionRequest::default()
                .from(intent.source_wallet)
                .to(s.to)
                .value(s.value)
                .input(s.input.clone().into())
                .gas_limit(s.gas_limit),
        ))
    }

    /// SELL only: ERC20 allowance per spender, and Permit2 sub-allowance per router.
    async fn allowances(
        &self,
        intent: &TradeIntent,
        routes: &[RouteLeg],
    ) -> Result<Allowances, crate::ethereum::provider::ProviderError> {
        let mut out = Allowances::default();
        if intent.side == TradeSide::Buy {
            return Ok(out);
        }
        let (mut spenders, mut routers): (Vec<Address>, Vec<Address>) = (Vec::new(), Vec::new());
        for r in routes {
            let a = self.adapter(r.pool);
            if !spenders.contains(&a.spender()) {
                spenders.push(a.spender());
            }
            if let Some(router) = a.permit2_spender().filter(|r| !routers.contains(r)) {
                routers.push(router);
            }
        }
        let (token, bot) = (intent.token_in, self.cfg.bot);
        let (erc20, permit2) = tokio::try_join!(
            futures::future::try_join_all(
                spenders
                    .iter()
                    .map(|s| self.reader.allowance(token, bot, *s))
            ),
            futures::future::try_join_all(routers.iter().map(|r| self.reader.permit2_allowance(
                self.permit2,
                bot,
                token,
                *r
            ))),
        )?;
        out.erc20 = spenders.into_iter().zip(erc20).collect();
        out.permit2 = routers.into_iter().zip(permit2).collect();
        Ok(out)
    }

    fn best_quote<'a>(
        &self,
        routes: &'a [RouteLeg],
        out: &[CallOutcome],
        amount: U256,
        probe: U256,
    ) -> Result<Quoted<'a>, ExecutionError> {
        let mut best: Option<Quoted<'a>> = None;
        let mut first_failure = None;
        for (route, pair) in routes.iter().zip(out.chunks_exact(2)) {
            let a = self.adapter(route.pool);
            let decoded = |o: &CallOutcome| {
                if o.success {
                    a.decode_quote(&o.return_data).map_err(|e| e.to_string())
                } else {
                    Err(o.revert_reason.clone().unwrap_or_default())
                }
            };
            match (decoded(&pair[0]), decoded(&pair[1])) {
                (Ok(full), Ok(small)) if !full.is_zero() => {
                    if best.as_ref().is_none_or(|b| full > b.expected_out) {
                        best = Some(Quoted {
                            route,
                            expected_out: full,
                            impact_bps: price_impact_bps(amount, full, probe, small),
                        });
                    }
                }
                (Err(e), _) | (_, Err(e)) => {
                    first_failure.get_or_insert(e);
                }
                _ => {
                    first_failure.get_or_insert("zero output".into());
                }
            }
        }
        best.ok_or_else(|| ExecutionError::NoQuote(first_failure.unwrap_or_default()))
    }
}

fn ensure_success(step: &'static str, result: &CallOutcome) -> Result<(), ExecutionError> {
    if result.success {
        return Ok(());
    }
    Err(SimulationError::Reverted {
        step,
        reason: result.revert_reason.clone().unwrap_or_default(),
    }
    .into())
}

/// `balanceOf` and `getEthBalance` both return a single uint256.
fn decode_u256(result: &CallOutcome) -> Result<U256, ExecutionError> {
    ensure_success("balance read", result)?;
    U256::abi_decode(&result.return_data)
        .map_err(|e| DexError::BadQuote(format!("balance read: {e}")).into())
}

/// `(quoted - received) / quoted` in bps, 0 when received >= quoted.
pub fn shortfall_bps(quoted: U256, received: U256) -> u32 {
    if quoted.is_zero() || received >= quoted {
        return 0;
    }
    ((quoted - received).saturating_mul(U256::from(BPS_DENOMINATOR)) / quoted).saturating_to()
}

fn check_source(out: &[CallOutcome], skip: usize) -> Result<(), ExecutionError> {
    match out.first() {
        Some(src) if skip == 1 && !src.success => Err(ExecutionError::SourceReverts(
            src.revert_reason.clone().unwrap_or_default(),
        )),
        _ => Ok(()),
    }
}

/// Source legs that go straight from the trade's input to its output token, deduplicated.
/// (Chained multi-protocol legs can't be replayed through a single router call.)
pub fn candidate_routes(intent: &TradeIntent) -> Vec<RouteLeg> {
    let mut out: Vec<RouteLeg> = Vec::new();
    for leg in &intent.route {
        let spans = leg.tokens.first() == Some(&intent.token_in)
            && leg.tokens.last() == Some(&intent.token_out);
        if spans && !out.contains(leg) {
            out.push(leg.clone());
        }
    }
    out
}

/// Execution price of `amount` vs the near-spot price of a tiny `probe`, in bps.
/// Pool fees appear in both quotes and cancel out. `None` when not measurable.
pub fn price_impact_bps(amount: U256, out: U256, probe: U256, probe_out: U256) -> Option<u32> {
    if probe_out.is_zero() || amount.is_zero() {
        return None;
    }
    let actual = out.saturating_mul(probe);
    let spot = probe_out.saturating_mul(amount);
    if actual >= spot {
        return Some(0);
    }
    let bps = (spot - actual).saturating_mul(U256::from(BPS_DENOMINATOR)) / spot;
    Some(bps.saturating_to())
}

/// A SELL worth more than MAX_TRADE_ETH sells only the fraction worth MAX_TRADE_ETH.
/// Linear scaling under-estimates the output of the smaller trade, so `min_out` stays safe.
pub fn cap_sell(
    side: TradeSide,
    amount: U256,
    expected_out: U256,
    max: U256,
) -> (U256, U256, bool) {
    if side == TradeSide::Buy || expected_out <= max {
        return (amount, expected_out, false);
    }
    (amount.saturating_mul(max) / expected_out, max, true)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Everything the executor stage needs; `broadcaster = None` is DRY_RUN.
pub struct ExecutionStage<R: ChainReader, S: Simulator, G: GasStrategy, T: TxSender> {
    pub executor: Executor<R, S>,
    pub gas: G,
    pub broadcaster: Option<Broadcaster<T>>,
}

/// Runs up to `workers` trades concurrently; on channel close waits for in-flight ones
/// (including receipt tracking). A worker slot is released once the trade is broadcast,
/// so waiting for receipts never blocks new trades.
pub async fn run_stage<R, S, G, T>(
    stage: Arc<ExecutionStage<R, S, G, T>>,
    mut rx: mpsc::Receiver<CopyTrade>,
    workers: usize,
    notifier: Notifier,
) where
    R: ChainReader + 'static,
    S: Simulator + 'static,
    G: GasStrategy + 'static,
    T: TxSender + 'static,
{
    let permits = Arc::new(Semaphore::new(workers));
    let mut tasks = JoinSet::new();
    while let Some(trade) = rx.recv().await {
        let Ok(permit) = Arc::clone(&permits).acquire_owned().await else {
            break;
        };
        let stage = Arc::clone(&stage);
        let notifier = notifier.clone();
        tasks.spawn(async move {
            let sent = {
                let _permit = permit;
                execute_one(&stage, trade, &notifier).await
            };
            if let (Some((plan, sent)), Some(b)) = (sent, &stage.broadcaster) {
                track_one(b, &plan, &sent, &notifier).await;
            }
        });
        while let Some(done) = tasks.try_join_next() {
            log_join(done);
        }
    }
    while let Some(done) = tasks.join_next().await {
        log_join(done);
    }
}

fn log_join(res: Result<(), tokio::task::JoinError>) {
    if let Err(e) = res {
        tracing::error!(event = "executor_task_panicked", error = %e);
    }
}

/// gas policy -> plan (quote + simulate) -> [live] sign + broadcast.
async fn execute_one<R, S, G, T>(
    stage: &ExecutionStage<R, S, G, T>,
    trade: CopyTrade,
    notifier: &Notifier,
) -> Option<(ExecutionPlan, SentTrade)>
where
    R: ChainReader,
    S: Simulator,
    G: GasStrategy,
    T: TxSender,
{
    let source_tx = trade.intent.source_tx_hash;
    let side = trade.side();
    let token = trade.token();
    let decision_at = trade.decision_at;
    let fail = |stage: &'static str, reason: &dyn std::fmt::Display| {
        tracing::info!(
            event = "trade_failed",
            %source_tx,
            side = ?side,
            %token,
            stage,
            reason = %reason,
            decision_to_fail_ms = ms(decision_at.elapsed()),
        );
        notifier.notify(Notification::Failed {
            source_tx,
            side,
            token,
            stage,
            reason: reason.to_string(),
            tx: None,
        });
    };

    // Gas first: in wait mode, quoting before a long wait would go stale.
    let fees = match stage.gas.fees().await {
        Ok(f) => f,
        Err(e) => {
            fail("gas", &e);
            return None;
        }
    };
    let plan = match stage.executor.plan(trade).await {
        Ok(p) => p,
        Err(e) => {
            fail("plan", &e);
            return None;
        }
    };
    let dry_run = stage.broadcaster.is_none();
    tracing::info!(
        event = "trade_simulated",
        %source_tx,
        side = ?side,
        %token,
        pool = ?plan.route.pool,
        hops = plan.route.tokens.len() - 1,
        amount_in = %plan.amount_in,
        sell_capped = plan.sell_capped,
        quoted_out = %plan.quoted_out,
        expected_out = %plan.expected_out,
        transfer_tax_bps = plan.transfer_tax_bps,
        min_out = %plan.min_out,
        price_impact_bps = plan.price_impact_bps,
        after_source = plan.after_source,
        steps = ?plan.txs.iter().map(|t| t.step).collect::<Vec<_>>(),
        gas_used = plan.txs.iter().map(|t| t.gas_used).sum::<u64>(),
        max_fee_per_gas = fees.max_fee_per_gas,
        max_priority_fee_per_gas = fees.max_priority_fee_per_gas,
        detection_to_decode_ms = ms(plan.trade.intent.decoded_at - plan.trade.intent.detected_at),
        decode_to_decision_ms = ms(decision_at - plan.trade.intent.decoded_at),
        decision_to_simulated_ms = ms(plan.simulated_at - decision_at),
        detection_to_simulated_ms = ms(plan.simulated_at - plan.trade.intent.detected_at),
        dry_run,
    );
    if dry_run {
        notifier.notify(Notification::Simulated {
            source_tx,
            side,
            token,
            amount_in: plan.amount_in,
            min_out: plan.min_out,
        });
    }
    let broadcaster = stage.broadcaster.as_ref()?;

    let sent = match broadcaster.send(&plan, fees).await {
        Ok(s) => s,
        Err(e) => {
            fail("broadcast", &e);
            return None;
        }
    };
    let intent = &plan.trade.intent;
    tracing::info!(
        event = "trade_broadcasted",
        %source_tx,
        source_wallet = %intent.source_wallet,
        dex = %intent.dex,
        side = ?side,
        %token,
        amount_in = %plan.amount_in,
        min_out = %plan.min_out,
        txs = ?sent.txs.iter().map(|t| (t.step, t.nonce, t.hash)).collect::<Vec<_>>(),
        detection_to_decode_ms = ms(intent.decoded_at - intent.detected_at),
        decode_to_decision_ms = ms(decision_at - intent.decoded_at),
        decision_to_broadcast_ms = ms(sent.broadcast_at - decision_at),
        sign_to_broadcast_ms = ms(sent.broadcast_at - sent.signed_at),
        total_latency_ms = ms(sent.broadcast_at - intent.detected_at),
    );
    notifier.notify(Notification::Broadcasted {
        source_tx,
        side,
        token,
        amount_in: plan.amount_in,
        min_out: plan.min_out,
        txs: sent.txs.iter().map(|t| (t.step, t.hash)).collect(),
    });
    Some((plan, sent))
}

async fn track_one<T: TxSender>(
    b: &Broadcaster<T>,
    plan: &ExecutionPlan,
    sent: &SentTrade,
    notifier: &Notifier,
) {
    let source_tx = plan.trade.intent.source_tx_hash;
    let side = plan.trade.side();
    let token = plan.trade.token();
    let swap = sent.txs.last().map(|t| t.hash);
    let failed = |stage: &'static str, reason: String| Notification::Failed {
        source_tx,
        side,
        token,
        stage,
        reason,
        tx: swap,
    };
    match b.track(sent).await {
        Ok(r) if r.success => {
            tracing::info!(
                event = "trade_confirmed",
                %source_tx,
                tx = ?swap,
                block = ?r.block_number,
                gas_used = r.gas_used,
                effective_gas_price = r.effective_gas_price,
                broadcast_to_confirm_ms = ms(sent.broadcast_at.elapsed()),
            );
            notifier.notify(Notification::Confirmed {
                source_tx,
                side,
                token,
                tx: swap,
                block: r.block_number,
            });
        }
        Ok(r) => {
            tracing::warn!(
                event = "trade_failed",
                %source_tx,
                tx = ?swap,
                stage = "onchain",
                reason = "swap reverted on-chain",
                block = ?r.block_number,
                gas_used = r.gas_used,
            );
            notifier.notify(failed("onchain", "swap reverted on-chain".into()));
        }
        Err(e) => {
            tracing::warn!(
                event = "trade_failed",
                %source_tx,
                tx = ?swap,
                stage = "receipt",
                reason = %e,
            );
            notifier.notify(failed("receipt", e.to_string()));
        }
    }
}

fn ms(d: std::time::Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use alloy::primitives::{Bytes, address};
    use alloy::sol_types::SolCall;
    use parking_lot::Mutex;

    use super::*;
    use crate::dex::uniswap_v2::IUniswapV2Router02Exec as V2;
    use crate::ethereum::provider::ProviderError;
    use crate::models::EthUsdPrice;
    use crate::models::test_util::{SOURCE, TOKEN, WETH, intent};
    use crate::utils::chain_addresses;

    const ETH: u128 = 1_000_000_000_000_000_000;
    const BOT: Address = address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266");

    #[derive(Default)]
    struct Reader {
        mined_nonce: u64,
        allowance: U256,
    }

    impl ChainReader for Reader {
        async fn gas_price(&self) -> Result<u128, ProviderError> {
            unreachable!()
        }
        async fn eth_balance(
            &self,
            _: Address,
            _: alloy::eips::BlockId,
        ) -> Result<U256, ProviderError> {
            unreachable!()
        }
        async fn token_balance(
            &self,
            _: Address,
            _: Address,
            _: alloy::eips::BlockId,
        ) -> Result<U256, ProviderError> {
            unreachable!()
        }
        async fn is_contract(&self, _: Address) -> Result<bool, ProviderError> {
            unreachable!()
        }
        async fn eth_usd(&self) -> Result<EthUsdPrice, ProviderError> {
            unreachable!()
        }
        async fn allowance(
            &self,
            _: Address,
            owner: Address,
            _: Address,
        ) -> Result<U256, ProviderError> {
            assert_eq!(owner, BOT);
            Ok(self.allowance)
        }
        async fn permit2_allowance(
            &self,
            _: Address,
            _: Address,
            _: Address,
            _: Address,
        ) -> Result<(U256, u64), ProviderError> {
            Ok((U256::ZERO, 0))
        }
        async fn mined_nonce(&self, who: Address) -> Result<u64, ProviderError> {
            assert_eq!(who, SOURCE);
            Ok(self.mined_nonce)
        }
    }

    /// Replays scripted outcomes and records every bundle it was asked to simulate.
    #[derive(Default)]
    struct Sim {
        responses: Mutex<VecDeque<Vec<CallOutcome>>>,
        seen: Mutex<Vec<Vec<TransactionRequest>>>,
    }

    impl Simulator for Sim {
        async fn simulate(
            &self,
            calls: Vec<TransactionRequest>,
        ) -> Result<Vec<CallOutcome>, SimulationError> {
            self.seen.lock().push(calls);
            Ok(self
                .responses
                .lock()
                .pop_front()
                .expect("scripted response"))
        }
    }

    fn ok(data: Vec<u8>, gas: u64) -> CallOutcome {
        CallOutcome {
            success: true,
            return_data: Bytes::from(data),
            gas_used: gas,
            peak_gas: gas,
            revert_reason: None,
        }
    }

    fn fail(reason: &str) -> CallOutcome {
        CallOutcome {
            success: false,
            return_data: Bytes::new(),
            gas_used: 0,
            peak_gas: 0,
            revert_reason: Some(reason.into()),
        }
    }

    fn bal(v: u128) -> CallOutcome {
        ok(U256::from(v).abi_encode(), 3_000)
    }

    fn v2_quote(amount_in: U256, out: U256) -> CallOutcome {
        ok(
            V2::getAmountsOutCall::abi_encode_returns(&vec![amount_in, out]),
            30_000,
        )
    }

    fn executor(reader: Reader, sim: Sim) -> Executor<Reader, Sim> {
        Executor::new(
            reader,
            sim,
            &chain_addresses(1).unwrap(),
            ExecutorConfig {
                bot: BOT,
                max_slippage_bps: 300,
                max_trade_wei: U256::from(ETH / 2),
            },
        )
    }

    fn trade(side: TradeSide, amount: u128, mined: bool) -> CopyTrade {
        let mut i = intent(side, amount);
        if mined {
            i.source_block = Some(100);
        }
        CopyTrade {
            intent: i,
            amount_in: U256::from(amount),
            capped: false,
            decision_at: Instant::now(),
        }
    }

    #[test]
    fn impact_math() {
        let e = U256::from(ETH);
        // probe 1/1000 gets 1000 tokens per ETH; full gets 990 per ETH -> 1% impact
        assert_eq!(
            price_impact_bps(
                e,
                U256::from(990_000u64),
                e / U256::from(1000),
                U256::from(1000u64)
            ),
            Some(100)
        );
        assert_eq!(
            price_impact_bps(e, U256::from(5u64), U256::from(1u64), U256::ZERO),
            None
        );
        assert_eq!(
            price_impact_bps(
                U256::from(10u64),
                U256::from(20u64),
                U256::from(1u64),
                U256::from(1u64)
            ),
            Some(0)
        );
    }

    #[test]
    fn sell_cap_scales_linearly() {
        let max = U256::from(ETH / 2);
        // selling 1000 tokens quoted at 2 ETH -> sell 250 tokens for (at least) 0.5 ETH
        let (amt, out, capped) = cap_sell(
            TradeSide::Sell,
            U256::from(1000u64),
            U256::from(2 * ETH),
            max,
        );
        assert_eq!((amt, out, capped), (U256::from(250u64), max, true));
        let (amt, _, capped) = cap_sell(
            TradeSide::Buy,
            U256::from(1000u64),
            U256::from(2 * ETH),
            max,
        );
        assert_eq!((amt, capped), (U256::from(1000u64), false));
    }

    #[test]
    fn only_spanning_legs_are_candidates() {
        let mut i = intent(TradeSide::Buy, ETH);
        let usdc = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let direct = i.route[0].clone();
        i.route = vec![
            RouteLeg {
                pool: PoolKind::UniswapV3,
                tokens: vec![WETH, usdc],
                fees: vec![500],
                v4: None,
            },
            direct.clone(),
            direct.clone(),
        ];
        assert_eq!(candidate_routes(&i), vec![direct]);
        i.route.remove(2);
        i.route.remove(1);
        assert!(candidate_routes(&i).is_empty());
    }

    #[tokio::test]
    async fn buy_plan_quotes_then_simulates_swap() {
        let amount = U256::from(ETH / 10);
        let probe = amount / U256::from(PROBE_DIVISOR);
        let sim = Sim::default();
        sim.responses.lock().extend([
            vec![
                v2_quote(amount, U256::from(1_000_000u64)),
                v2_quote(probe, U256::from(1_005u64)),
            ],
            vec![bal(0), ok(vec![], 120_000), bal(1_000_000)],
        ]);
        let ex = executor(Reader::default(), sim);
        let plan = ex
            .plan(trade(TradeSide::Buy, ETH / 10, true))
            .await
            .unwrap();

        assert_eq!(plan.expected_out, U256::from(1_000_000u64));
        assert_eq!(
            plan.min_out,
            U256::from(970_000u64),
            "3% slippage tolerance"
        );
        assert_eq!(plan.price_impact_bps, 49);
        assert_eq!(plan.transfer_tax_bps, 0);
        let swap = V2::swapExactETHForTokensSupportingFeeOnTransferTokensCall::abi_decode(
            plan.txs[0].request.input.input().unwrap(),
        )
        .unwrap();
        assert_eq!(
            swap.amountOutMin, plan.min_out,
            "final tx carries the real bound"
        );
        assert!(!plan.after_source, "mined source => latest state");
        assert_eq!(plan.txs.len(), 1);
        assert_eq!(plan.txs[0].step, "swap");
        assert_eq!(plan.txs[0].gas_used, 120_000);
        assert_eq!(plan.txs[0].request.from, Some(BOT));
        assert_eq!(plan.txs[0].request.value, Some(amount));
        assert_eq!(ex.sim.seen.lock()[0].len(), 2, "full + probe quote");
    }

    #[tokio::test]
    async fn pending_source_is_simulated_first_and_must_succeed() {
        let sim = Sim::default();
        sim.responses
            .lock()
            .push_back(vec![fail("TRANSFER_FAILED"), fail("x"), fail("x")]);
        let ex = executor(
            Reader {
                mined_nonce: 7,
                ..Default::default()
            },
            sim,
        );
        let err = ex
            .plan(trade(TradeSide::Buy, ETH / 10, false))
            .await
            .unwrap_err();
        assert!(matches!(err, ExecutionError::SourceReverts(ref r) if r == "TRANSFER_FAILED"));
        let seen = ex.sim.seen.lock();
        assert_eq!(seen[0][0].from, Some(SOURCE), "source tx first in bundle");
    }

    #[tokio::test]
    async fn excessive_price_impact_is_rejected() {
        let amount = U256::from(ETH / 10);
        let probe = amount / U256::from(PROBE_DIVISOR);
        let sim = Sim::default();
        // full gets 90% of spot rate -> 10% impact > 3%
        sim.responses.lock().push_back(vec![
            v2_quote(amount, U256::from(900_000u64)),
            v2_quote(probe, U256::from(1_000u64)),
        ]);
        let ex = executor(Reader::default(), sim);
        assert!(matches!(
            ex.plan(trade(TradeSide::Buy, ETH / 10, true)).await,
            Err(ExecutionError::PriceImpact {
                impact_bps: 1000,
                max_bps: 300
            })
        ));
    }

    #[tokio::test]
    async fn sell_without_allowance_approves_then_swaps() {
        let amount = U256::from(1_000_000u64);
        let probe = amount / U256::from(PROBE_DIVISOR);
        let sim = Sim::default();
        sim.responses.lock().extend([
            vec![
                v2_quote(amount, U256::from(ETH / 10)),
                v2_quote(probe, U256::from(ETH / 10_000)),
            ],
            vec![
                ok(vec![], 46_000),
                bal(5),
                ok(vec![], 150_000),
                bal(5 + ETH / 10),
            ],
        ]);
        let ex = executor(Reader::default(), sim);
        let plan = ex
            .plan(trade(TradeSide::Sell, 1_000_000, true))
            .await
            .unwrap();
        let steps: Vec<_> = plan.txs.iter().map(|t| t.step).collect();
        assert_eq!(steps, vec!["approve", "swap"]);
        assert_eq!(plan.txs[0].request.to, Some(TOKEN.into()));
        assert!(!plan.sell_capped);
        assert_eq!(
            plan.expected_out,
            U256::from(ETH / 10),
            "ETH delta via getEthBalance"
        );
        assert_eq!(plan.txs[0].gas_used, 46_000);
    }

    #[tokio::test]
    async fn fee_on_transfer_output_sets_min_out_from_received_amount() {
        let amount = U256::from(ETH / 10);
        let probe = amount / U256::from(PROBE_DIVISOR);
        let sim = Sim::default();
        sim.responses.lock().extend([
            vec![
                v2_quote(amount, U256::from(1_000_000u64)),
                v2_quote(probe, U256::from(1_000u64)),
            ],
            // 10% of the output is taxed away on transfer to the bot.
            vec![bal(0), ok(vec![], 150_000), bal(900_000)],
        ]);
        let ex = executor(Reader::default(), sim);
        let plan = ex
            .plan(trade(TradeSide::Buy, ETH / 10, true))
            .await
            .unwrap();
        assert_eq!(plan.quoted_out, U256::from(1_000_000u64));
        assert_eq!(plan.expected_out, U256::from(900_000u64));
        assert_eq!(plan.transfer_tax_bps, 1_000);
        assert_eq!(plan.min_out, U256::from(873_000u64));
        let seen = ex.sim.seen.lock();
        let probe_swap = V2::swapExactETHForTokensSupportingFeeOnTransferTokensCall::abi_decode(
            seen[1][1].input.input().unwrap(),
        )
        .unwrap();
        assert_eq!(
            probe_swap.amountOutMin,
            U256::ZERO,
            "measuring run is unbounded"
        );
    }

    #[tokio::test]
    async fn swap_revert_is_reported_with_step_and_reason() {
        let amount = U256::from(ETH / 10);
        let probe = amount / U256::from(PROBE_DIVISOR);
        let sim = Sim::default();
        sim.responses.lock().extend([
            vec![
                v2_quote(amount, U256::from(1_000u64)),
                v2_quote(probe, U256::from(1u64)),
            ],
            vec![bal(0), fail("TransferHelper: TRANSFER_FAILED"), bal(0)],
        ]);
        let ex = executor(Reader::default(), sim);
        let err = ex
            .plan(trade(TradeSide::Buy, ETH / 10, true))
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            ExecutionError::Simulation(SimulationError::Reverted { step: "swap", ref reason })
                if reason.contains("TRANSFER_FAILED")
        ));
    }

    struct FixedGas;

    impl GasStrategy for FixedGas {
        async fn fees(
            &self,
        ) -> Result<crate::execution::gas::Eip1559Fees, crate::execution::gas::GasError> {
            Ok(crate::execution::gas::Eip1559Fees {
                max_fee_per_gas: 20_000_000_000,
                max_priority_fee_per_gas: 1_000_000_000,
            })
        }
    }

    #[derive(Default)]
    struct Sender {
        sent: Mutex<Vec<Bytes>>,
        receipts: std::sync::atomic::AtomicUsize,
    }

    impl TxSender for Sender {
        async fn pending_nonce(&self, _: Address) -> Result<u64, ProviderError> {
            Ok(3)
        }
        async fn send_raw(&self, raw: Bytes) -> Result<alloy::primitives::B256, ProviderError> {
            self.sent.lock().push(raw.clone());
            Ok(alloy::primitives::keccak256(&raw))
        }
        async fn wait_receipt(
            &self,
            _: alloy::primitives::B256,
            _: std::time::Duration,
        ) -> Result<
            crate::execution::broadcast::ReceiptSummary,
            crate::execution::broadcast::BroadcastError,
        > {
            self.receipts
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(crate::execution::broadcast::ReceiptSummary {
                success: true,
                block_number: Some(1),
                gas_used: 100_000,
                effective_gas_price: 1,
            })
        }
    }

    async fn run_one(live: bool) -> Arc<ExecutionStage<Reader, Sim, FixedGas, Sender>> {
        let amount = U256::from(ETH / 10);
        let probe = amount / U256::from(PROBE_DIVISOR);
        let sim = Sim::default();
        sim.responses.lock().extend([
            vec![
                v2_quote(amount, U256::from(1_000_000u64)),
                v2_quote(probe, U256::from(1_000u64)),
            ],
            vec![bal(0), ok(vec![], 120_000), bal(1_000_000)],
        ]);
        // Anvil dev key #0 (address == BOT) — test only.
        let signer = crate::ethereum::signer::BotSigner::from_hex(
            "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80",
        )
        .unwrap();
        let stage = Arc::new(ExecutionStage {
            executor: executor(Reader::default(), sim),
            gas: FixedGas,
            broadcaster: live.then(|| {
                Broadcaster::new(
                    Sender::default(),
                    signer,
                    crate::ethereum::nonce::NonceManager::new(),
                    1,
                )
            }),
        });
        let (tx, rx) = mpsc::channel(1);
        tx.send(trade(TradeSide::Buy, ETH / 10, true))
            .await
            .unwrap();
        drop(tx);
        run_stage(Arc::clone(&stage), rx, 2, Notifier::disabled()).await;
        stage
    }

    #[tokio::test]
    async fn dry_run_never_broadcasts() {
        let stage = run_one(false).await;
        assert!(stage.broadcaster.is_none());
        assert_eq!(
            stage.executor.sim.seen.lock().len(),
            2,
            "full pipeline up to simulation"
        );
    }

    #[tokio::test]
    async fn live_mode_signs_sends_and_tracks_receipt() {
        let stage = run_one(true).await;
        let b = stage.broadcaster.as_ref().unwrap();
        assert_eq!(
            b.sender_for_tests().sent.lock().len(),
            1,
            "one swap tx sent"
        );
        assert_eq!(
            b.sender_for_tests()
                .receipts
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "receipt awaited"
        );
    }

    #[tokio::test]
    async fn unquotable_route_reports_reason() {
        let sim = Sim::default();
        sim.responses
            .lock()
            .push_back(vec![fail("ds-math-sub-underflow"), fail("x")]);
        let ex = executor(Reader::default(), sim);
        assert!(matches!(
            ex.plan(trade(TradeSide::Buy, ETH / 10, true)).await,
            Err(ExecutionError::NoQuote(ref r)) if r == "ds-math-sub-underflow"
        ));
    }
}
