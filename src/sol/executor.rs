//! SolCopyTrade -> simulated plan -> [live] signed + sent tx -> confirmation.
//!
//! Same safety rules as the ETH executor:
//! - The sent tx is the simulated tx with only `min_out` and the compute-unit limit changed.
//! - `min_out` = amount *actually received* in the simulation (balance after − before) ×
//!   (1 − SOL_MAX_SLIPPAGE_BPS); never a quote, never the source's min-out.
//! - Nothing is sent if the simulation fails.

use std::sync::Arc;
use std::time::Duration;

use solana_hash::Hash;
use solana_instruction::Instruction;
use solana_message::{VersionedMessage, v0};
use solana_pubkey::Pubkey;
use solana_signature::Signature;
use solana_transaction::versioned::VersionedTransaction;
use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinSet;

use crate::common::env::BPS_DENOMINATOR;
use crate::sol::models::{SolCopyTrade, TradeSide};
use crate::sol::notify::{Notifier, SolNotification};
use crate::sol::pump::{self, TOKEN_PROGRAM, WSOL_MINT};
use crate::sol::rpc::{RpcError, SigStatus, SolRpc};
use crate::sol::signer::SolSigner;

/// Compute-unit limit of the simulated tx. pump swaps use ~50-80k CU (incl. ATA creation),
/// so this is ~5× headroom; a swap needing more fails simulation and is skipped. Not the
/// 1.4M max: the simulated tx pays priority fee on its limit, which would understate SOL
/// received on small sells.
const SIM_CU: u32 = 400_000;
/// Margin over simulated units (same 1.3× as the ETH gas limit).
const CU_MARGIN_NUM: u64 = 13;
const CU_MARGIN_DEN: u64 = 10;
/// A blockhash is valid for ~150 slots (~60-90 s); after that the tx can never land.
const CONFIRM_TIMEOUT: Duration = Duration::from_secs(90);
const CONFIRM_POLL: Duration = Duration::from_millis(800);
/// Simulation logs kept in an error message (the tail holds the program error).
const LOG_TAIL: usize = 4;

#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    #[error(transparent)]
    Rpc(#[from] RpcError),
    #[error("building tx: {0}")]
    Build(String),
    #[error("simulation failed: {err}; logs: {logs}")]
    SimulationFailed { err: String, logs: String },
    #[error("simulation did not report consumed compute units")]
    NoUnits,
    #[error("swap pays out nothing in simulation (received {received})")]
    NothingReceived { received: u64 },
    #[error("min_out rounds to zero (received {received})")]
    MinOutZero { received: u64 },
}

#[derive(Debug, Clone)]
pub struct ExecutorConfig {
    pub max_slippage_bps: u32,
    pub priority_fee_micro_lamports: u64,
    pub dry_run: bool,
}

pub struct SolExecutionStage<R: SolRpc> {
    pub rpc: R,
    pub signer: SolSigner,
    pub cfg: ExecutorConfig,
}

/// A signed, ready-to-send transaction plus what the simulation measured.
#[derive(Debug, Clone)]
pub struct Plan {
    pub tx: VersionedTransaction,
    pub received: u64,
    pub min_out: u64,
    pub cu_limit: u32,
}

impl<R: SolRpc> SolExecutionStage<R> {
    fn build(
        &self,
        trade: &SolCopyTrade,
        cu_limit: u32,
        min_out: u64,
        blockhash: &Hash,
    ) -> Result<VersionedTransaction, ExecError> {
        let bot = self.signer.pubkey();
        let mut ixs: Vec<Instruction> = vec![
            pump::compute_unit_limit(cu_limit),
            pump::compute_unit_price(self.cfg.priority_fee_micro_lamports),
        ];
        ixs.extend(pump::swap_instructions(
            &bot,
            trade.intent.side,
            &trade.market,
            trade.amount_in,
            min_out,
        ));
        let msg = v0::Message::try_compile(&bot, &ixs, &[], blockhash.clone())
            .map_err(|e| ExecError::Build(e.to_string()))?;
        VersionedTransaction::try_new(VersionedMessage::V0(msg), &[self.signer.keypair()])
            .map_err(|e| ExecError::Build(e.to_string()))
    }

    /// Simulate with `min_out = 1`, measure, then rebuild with the real `min_out`.
    pub async fn plan(&self, trade: &SolCopyTrade) -> Result<Plan, ExecError> {
        let bot = self.signer.pubkey();
        let blockhash = self.rpc.latest_blockhash().await?;
        let sim_tx = self.build(trade, SIM_CU, 1, &blockhash)?;
        let watch: [Pubkey; 3] = [
            bot,
            pump::ata(&bot, &WSOL_MINT, &TOKEN_PROGRAM),
            pump::ata(&bot, &trade.market.mint, &trade.market.base_token_program),
        ];
        let sim = self.rpc.simulate(&sim_tx, &watch).await?;
        if let Some(err) = sim.err {
            let tail = sim.logs.len().saturating_sub(LOG_TAIL);
            return Err(ExecError::SimulationFailed {
                err,
                logs: sim.logs[tail..].join(" | "),
            });
        }
        let post = |i: usize| sim.accounts.get(i).and_then(Option::as_ref);
        let received = match trade.intent.side {
            TradeSide::Buy => post(2)
                .and_then(|a| pump::token_account_amount(&a.data))
                .unwrap_or(0)
                .saturating_sub(trade.pre.base_tokens),
            // Wallet + WSOL ATA together: wrapping/closing only moves SOL between them, so
            // the sum changes by proceeds − tx fee − new rent (a conservative measure).
            TradeSide::Sell => {
                let lamports = |i: usize| post(i).map_or(0, |a| a.lamports);
                lamports(0)
                    .saturating_add(lamports(1))
                    .saturating_sub(trade.pre.sol_lamports)
            }
        };
        if received == 0 {
            return Err(ExecError::NothingReceived { received });
        }
        let min_out = received
            .saturating_mul(u64::from(BPS_DENOMINATOR - self.cfg.max_slippage_bps))
            / u64::from(BPS_DENOMINATOR);
        if min_out == 0 {
            return Err(ExecError::MinOutZero { received });
        }
        let units = sim.units_consumed.ok_or(ExecError::NoUnits)?;
        let cu_limit = u32::try_from(units.saturating_mul(CU_MARGIN_NUM) / CU_MARGIN_DEN + 1)
            .unwrap_or(SIM_CU)
            .min(SIM_CU);
        Ok(Plan {
            tx: self.build(trade, cu_limit, min_out, &blockhash)?,
            received,
            min_out,
            cu_limit,
        })
    }
}

pub async fn run_stage<R: SolRpc + 'static>(
    stage: Arc<SolExecutionStage<R>>,
    mut rx: mpsc::Receiver<SolCopyTrade>,
    workers: usize,
    notifier: Notifier,
) {
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
                execute_one(&stage, &trade, &notifier).await
            };
            // Tracking does not hold a worker slot: it only polls.
            if let Some(tx) = sent {
                track_one(&stage.rpc, &trade, tx, &notifier).await;
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

/// plan (simulate) -> [live] send. Returns the sent tx signature.
async fn execute_one<R: SolRpc>(
    stage: &SolExecutionStage<R>,
    trade: &SolCopyTrade,
    notifier: &Notifier,
) -> Option<Signature> {
    let (signature, side, mint) = (trade.intent.signature, trade.intent.side, trade.market.mint);
    let fail = |stage: &'static str, reason: &dyn std::fmt::Display, tx: Option<Signature>| {
        tracing::info!(event = "trade_failed", %signature, side = ?side, %mint, stage, reason = %reason);
        notifier.notify(SolNotification::Failed {
            signature,
            side,
            mint,
            stage,
            reason: reason.to_string(),
            tx,
        });
    };

    let plan = match stage.plan(trade).await {
        Ok(p) => p,
        Err(e) => {
            fail("plan", &e, None);
            return None;
        }
    };
    tracing::info!(
        event = "trade_planned",
        %signature,
        side = ?side,
        %mint,
        amount_in = trade.amount_in,
        received = plan.received,
        min_out = plan.min_out,
        cu_limit = plan.cu_limit,
        decision_to_plan_ms = trade.decision_at.elapsed().as_secs_f64() * 1000.0,
        dry_run = stage.cfg.dry_run,
    );
    if stage.cfg.dry_run {
        notifier.notify(SolNotification::Simulated {
            signature,
            side,
            mint,
            amount_in: trade.amount_in,
            min_out: plan.min_out,
        });
        return None;
    }
    match stage.rpc.send(&plan.tx).await {
        Ok(tx) => {
            tracing::info!(event = "trade_sent", %signature, %tx);
            notifier.notify(SolNotification::Broadcasted {
                signature,
                side,
                mint,
                amount_in: trade.amount_in,
                min_out: plan.min_out,
                tx,
            });
            Some(tx)
        }
        Err(e) => {
            fail("send", &e, plan.tx.signatures.first().copied());
            None
        }
    }
}

async fn track_one<R: SolRpc>(rpc: &R, trade: &SolCopyTrade, tx: Signature, notifier: &Notifier) {
    let (signature, side, mint) = (trade.intent.signature, trade.intent.side, trade.market.mint);
    let deadline = tokio::time::Instant::now() + CONFIRM_TIMEOUT;
    let failed = |reason: String| {
        tracing::warn!(event = "trade_failed", %signature, %tx, stage = "confirm", %reason);
        notifier.notify(SolNotification::Failed {
            signature,
            side,
            mint,
            stage: "confirm",
            reason,
            tx: Some(tx),
        });
    };
    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep(CONFIRM_POLL).await;
        match rpc.signature_status(&tx).await {
            Ok(Some(SigStatus::Confirmed { slot })) => {
                tracing::info!(event = "trade_confirmed", %signature, %tx, slot);
                notifier.notify(SolNotification::Confirmed {
                    signature,
                    side,
                    mint,
                    tx,
                    slot,
                });
                return;
            }
            Ok(Some(SigStatus::Failed { err, .. })) => return failed(format!("reverted: {err}")),
            Ok(Some(SigStatus::Processed) | None) => {}
            Err(e) => tracing::debug!(event = "status_poll_failed", %tx, error = %e),
        }
    }
    failed("not confirmed before the blockhash expired".into());
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use parking_lot::Mutex;
    use solana_keypair::Keypair;

    use super::*;
    use crate::sol::models::{PreBalances, SolTradeIntent, Venue};
    use crate::sol::pump::{Market, PUMP_PROGRAM, TOKEN_2022_PROGRAM};
    use crate::sol::rpc::{AccountInfo, SimResult};

    #[derive(Default)]
    struct Rpc {
        sim: Mutex<Option<SimResult>>,
        simulated: Mutex<Vec<VersionedTransaction>>,
        sent: Mutex<Vec<VersionedTransaction>>,
        statuses: Mutex<Vec<Option<SigStatus>>>,
    }

    impl SolRpc for Rpc {
        async fn get_multiple_accounts(
            &self,
            _: &[Pubkey],
        ) -> Result<Vec<Option<AccountInfo>>, RpcError> {
            unimplemented!()
        }
        async fn latest_blockhash(&self) -> Result<Hash, RpcError> {
            Ok(Hash::new_from_array([3; 32]))
        }
        async fn simulate(
            &self,
            tx: &VersionedTransaction,
            _: &[Pubkey],
        ) -> Result<SimResult, RpcError> {
            self.simulated.lock().push(tx.clone());
            self.sim
                .lock()
                .clone()
                .ok_or_else(|| RpcError::Decode("no sim".into()))
        }
        async fn send(&self, tx: &VersionedTransaction) -> Result<Signature, RpcError> {
            self.sent.lock().push(tx.clone());
            Ok(tx.signatures[0])
        }
        async fn signature_status(&self, _: &Signature) -> Result<Option<SigStatus>, RpcError> {
            let mut s = self.statuses.lock();
            Ok(if s.is_empty() { None } else { s.remove(0) })
        }
    }

    fn stage(dry_run: bool, sim: Option<SimResult>) -> SolExecutionStage<Rpc> {
        SolExecutionStage {
            rpc: Rpc {
                sim: Mutex::new(sim),
                ..Rpc::default()
            },
            signer: SolSigner::from_secret(&Keypair::new().to_base58_string()).unwrap(),
            cfg: ExecutorConfig {
                max_slippage_bps: 500,
                priority_fee_micro_lamports: 1_000,
                dry_run,
            },
        }
    }

    fn trade(side: TradeSide, pre: PreBalances) -> SolCopyTrade {
        let mint = Pubkey::new_unique();
        SolCopyTrade {
            intent: SolTradeIntent {
                signature: Signature::from([5u8; 64]),
                slot: 1,
                source_wallet: Pubkey::new_unique(),
                venue: Venue::BondingCurve,
                side,
                mint,
                source_sol: 1,
                source_tokens: 1,
                seen_at: Instant::now(),
            },
            amount_in: 100_000_000,
            capped: false,
            market: Market {
                mint,
                base_token_program: TOKEN_2022_PROGRAM,
                pool: None,
            },
            pre,
            decision_at: Instant::now(),
        }
    }

    fn ok_sim(accounts: Vec<Option<AccountInfo>>) -> SimResult {
        SimResult {
            err: None,
            logs: vec![],
            units_consumed: Some(100_000),
            accounts,
        }
    }

    fn wallet(lamports: u64) -> Option<AccountInfo> {
        Some(AccountInfo {
            lamports,
            owner: pump::SYSTEM_PROGRAM,
            data: vec![],
        })
    }

    fn tokens(amount: u64) -> Option<AccountInfo> {
        let mut data = vec![0u8; 165];
        data[64..72].copy_from_slice(&amount.to_le_bytes());
        Some(AccountInfo {
            lamports: 1,
            owner: TOKEN_2022_PROGRAM,
            data,
        })
    }

    /// (cu_limit, min_out) read back from a built tx.
    fn decode(tx: &VersionedTransaction) -> (u32, u64) {
        let keys = tx.message.static_account_keys();
        let ixs = tx.message.instructions();
        let cu = u32::from_le_bytes(ixs[0].data[1..5].try_into().unwrap());
        let trade = ixs
            .iter()
            .find(|ix| keys[usize::from(ix.program_id_index)] == PUMP_PROGRAM)
            .unwrap();
        (
            cu,
            u64::from_le_bytes(trade.data[16..24].try_into().unwrap()),
        )
    }

    #[tokio::test]
    async fn buy_min_out_is_received_tokens_minus_slippage() {
        let pre = PreBalances {
            sol_lamports: 0,
            base_tokens: 1_000,
        };
        let s = stage(false, Some(ok_sim(vec![wallet(0), None, tokens(11_000)])));
        let plan = s.plan(&trade(TradeSide::Buy, pre)).await.unwrap();
        assert_eq!(plan.received, 10_000);
        assert_eq!(plan.min_out, 9_500);
        assert_eq!(plan.cu_limit, 130_001);
        // Simulated tx: min_out 1, max CU. Final tx: only those two values differ.
        let simulated = &s.rpc.simulated.lock()[0];
        assert_eq!(decode(simulated), (SIM_CU, 1));
        assert_eq!(decode(&plan.tx), (130_001, 9_500));
        assert_eq!(
            simulated.message.static_account_keys(),
            plan.tx.message.static_account_keys()
        );
        assert_eq!(
            simulated.message.recent_blockhash(),
            plan.tx.message.recent_blockhash()
        );
    }

    #[tokio::test]
    async fn sell_measures_wallet_plus_wsol_lamports() {
        let pre = PreBalances {
            sol_lamports: 1_000_000,
            base_tokens: 5,
        };
        // Wallet ends at 1.3M, WSOL ATA holds 0.1M more than nothing: received 0.4M.
        let wsol = Some(AccountInfo {
            lamports: 100_000,
            owner: TOKEN_PROGRAM,
            data: vec![0; 165],
        });
        let s = stage(true, Some(ok_sim(vec![wallet(1_300_000), wsol, tokens(0)])));
        let plan = s.plan(&trade(TradeSide::Sell, pre)).await.unwrap();
        assert_eq!((plan.received, plan.min_out), (400_000, 380_000));
    }

    #[tokio::test]
    async fn failed_or_empty_simulation_never_sends() {
        let notifier = Notifier::disabled();
        let pre = PreBalances {
            sol_lamports: 0,
            base_tokens: 0,
        };
        let reverted = SimResult {
            err: Some("{\"InstructionError\":[2,{\"Custom\":6003}]}".into()),
            logs: vec!["a".into(), "Program log: slippage".into()],
            units_consumed: Some(1),
            accounts: vec![],
        };
        let s = stage(false, Some(reverted));
        let err = s.plan(&trade(TradeSide::Buy, pre)).await.unwrap_err();
        assert!(err.to_string().contains("6003") && err.to_string().contains("slippage"));
        assert!(
            execute_one(&s, &trade(TradeSide::Buy, pre), &notifier)
                .await
                .is_none()
        );
        assert!(s.rpc.sent.lock().is_empty());

        let s = stage(false, Some(ok_sim(vec![wallet(0), None, None])));
        assert!(matches!(
            s.plan(&trade(TradeSide::Buy, pre)).await,
            Err(ExecError::NothingReceived { .. })
        ));
        assert!(
            execute_one(&s, &trade(TradeSide::Buy, pre), &notifier)
                .await
                .is_none()
        );
        assert!(s.rpc.sent.lock().is_empty());
    }

    #[tokio::test]
    async fn dry_run_simulates_but_never_sends_and_live_sends_the_plan() {
        let notifier = Notifier::disabled();
        let pre = PreBalances {
            sol_lamports: 0,
            base_tokens: 0,
        };
        let sim = ok_sim(vec![wallet(0), None, tokens(10)]);
        let dry = stage(true, Some(sim.clone()));
        assert!(
            execute_one(&dry, &trade(TradeSide::Buy, pre), &notifier)
                .await
                .is_none()
        );
        assert_eq!(dry.rpc.simulated.lock().len(), 1);
        assert!(dry.rpc.sent.lock().is_empty());

        let live = stage(false, Some(sim));
        let sig = execute_one(&live, &trade(TradeSide::Buy, pre), &notifier)
            .await
            .unwrap();
        let sent = live.rpc.sent.lock();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].signatures[0], sig);
        assert_eq!(decode(&sent[0]).1, 9, "10 received − 5%");
    }

    #[tokio::test(start_paused = true)]
    async fn tracking_reports_confirmation_and_reverts() {
        let (n, mut rx) = {
            let (tx, rx) = mpsc::channel(8);
            (telegram_notifier(tx), rx)
        };
        let s = stage(false, None);
        let t = trade(
            TradeSide::Buy,
            PreBalances {
                sol_lamports: 0,
                base_tokens: 0,
            },
        );
        s.rpc.statuses.lock().extend([
            None,
            Some(SigStatus::Processed),
            Some(SigStatus::Confirmed { slot: 9 }),
        ]);
        track_one(&s.rpc, &t, Signature::from([1; 64]), &n).await;
        assert!(matches!(
            rx.recv().await,
            Some(SolNotification::Confirmed { slot: 9, .. })
        ));

        s.rpc.statuses.lock().push(Some(SigStatus::Failed {
            slot: 3,
            err: "x".into(),
        }));
        track_one(&s.rpc, &t, Signature::from([1; 64]), &n).await;
        assert!(matches!(
            rx.recv().await,
            Some(SolNotification::Failed {
                stage: "confirm",
                ..
            })
        ));

        track_one(&s.rpc, &t, Signature::from([1; 64]), &n).await;
        match rx.recv().await {
            Some(SolNotification::Failed { reason, .. }) => assert!(reason.contains("expired")),
            other => panic!("unexpected {other:?}"),
        }
    }

    fn telegram_notifier(tx: mpsc::Sender<SolNotification>) -> Notifier {
        crate::common::telegram::Notifier::from_sender(tx)
    }
}
