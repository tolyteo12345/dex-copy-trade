#![forbid(unsafe_code)]

//! Solana / pump.fun copy-trading bot. Separate process from `eth-copy-trade`: own runtime,
//! own RPC connections, own config (`SOL_*`), so neither chain can slow the other down.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use tokio::sync::{mpsc, watch};

use dex_copy_trade::common::dedup::{DEDUP_TTL, TtlSet};
use dex_copy_trade::common::redact::redact_url;
use dex_copy_trade::common::runtime::{init_tracing, shutdown_signal};
use dex_copy_trade::common::telegram::{self, TelegramSink};
use dex_copy_trade::sol::config::SolConfig;
use dex_copy_trade::sol::decision::{self, DecisionConfig, DecisionStage};
use dex_copy_trade::sol::decoder::{self, SolDecoder};
use dex_copy_trade::sol::executor::{self, ExecutorConfig, SolExecutionStage};
use dex_copy_trade::sol::models::{SolCopyTrade, SolTradeIntent, SourceTx};
use dex_copy_trade::sol::notify::Notifier;
use dex_copy_trade::sol::pump::SYSTEM_PROGRAM;
use dex_copy_trade::sol::rpc::{HttpRpc, MAINNET_GENESIS_HASH, SolRpc};
use dex_copy_trade::sol::watcher::SolWatcher;

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(15);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // A missing .env is fine (env may come from docker/systemd).
    let _ = dotenvy::dotenv();
    init_tracing()?;

    let config = SolConfig::from_env().context("invalid configuration")?;
    log_startup(&config);
    let bot = config.signer.pubkey();

    let rpc = HttpRpc::new(&config.rpc_url)?;
    let genesis = rpc
        .genesis_hash()
        .await
        .context("startup RPC check failed")?;
    anyhow::ensure!(
        genesis == MAINNET_GENESIS_HASH,
        "SOL_RPC_URL is not mainnet-beta (genesis {genesis}); pump addresses are mainnet-only"
    );

    // A system-owned account is a plain wallet. Anything else (program-owned, executable)
    // cannot sign for SOL transfers the way the bot expects: never trade from it.
    let bot_account = rpc
        .get_multiple_accounts(&[bot])
        .await
        .context("reading bot wallet")?
        .pop()
        .flatten();
    match bot_account {
        None => tracing::warn!(event = "bot_wallet_empty", %bot, "bot wallet has no SOL"),
        Some(a) if a.owner != SYSTEM_PROGRAM => {
            let msg = format!(
                "bot wallet {bot} is owned by {} (not a plain wallet)",
                a.owner
            );
            if !config.dry_run {
                anyhow::bail!("{msg}; refusing to trade with SOL_DRY_RUN=false");
            }
            tracing::warn!(event = "bot_wallet_not_system_owned", "{msg}");
        }
        Some(_) => {}
    }

    let (notifier, notify_task): (Notifier, _) = match &config.telegram {
        Some(tg) => {
            let sink = TelegramSink::new(tg).context("building Telegram client")?;
            let (notifier, task) = telegram::spawn(sink, config.dry_run);
            (notifier, Some(task))
        }
        None => (Notifier::disabled(), None),
    };

    let decision = DecisionStage::new(
        HttpRpc::new(&config.rpc_url)?,
        DecisionConfig::from_config(&config),
        TtlSet::new(DEDUP_TTL),
    );
    let decoder = SolDecoder::new(config.copy_wallets.iter().copied());
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let (tx_out, tx_in) = mpsc::channel::<SourceTx>(config.channel_capacity);
    let (intent_out, intent_in) = mpsc::channel::<SolTradeIntent>(config.channel_capacity);
    let (trade_out, trade_in) = mpsc::channel::<SolCopyTrade>(config.channel_capacity);

    let watcher = SolWatcher::new(
        config.grpc_url.clone(),
        config.grpc_x_token.as_ref().map(|t| t.0.clone()),
        config.copy_wallets.clone(),
        tx_out,
        shutdown_rx,
    );
    let executor_workers = config.executor_workers;
    let execution = Arc::new(SolExecutionStage {
        rpc,
        cfg: ExecutorConfig {
            max_slippage_bps: config.max_slippage_bps,
            priority_fee_micro_lamports: config.priority_fee_micro_lamports,
            dry_run: config.dry_run,
        },
        // Moved last: the signer lives only inside the execution stage from here on.
        signer: config.signer,
    });

    let mut watcher_task = tokio::spawn(watcher.run());
    let decoder_task = tokio::spawn(decoder::run_stage(decoder, tx_in, intent_out));
    let decision_task = tokio::spawn(decision::run_stage(
        decision,
        intent_in,
        trade_out,
        notifier.clone(),
    ));
    // The stages own the only `Notifier` clones, so the notify task drains and exits
    // once they finish.
    let executor_task = tokio::spawn(executor::run_stage(
        execution,
        trade_in,
        executor_workers,
        notifier,
    ));

    tokio::select! {
        res = &mut watcher_task => {
            res.context("watcher task panicked")?.context("watcher failed")?;
            return Ok(());
        }
        signal = shutdown_signal() => {
            tracing::info!(event = "shutdown_requested", %signal);
        }
    }

    let _ = shutdown_tx.send(true);
    match tokio::time::timeout(SHUTDOWN_TIMEOUT, async {
        let watcher_res = watcher_task.await;
        // Each stage exits once its upstream sender is dropped, after draining its queue;
        // the executor also waits for in-flight trades.
        let _ = decoder_task.await;
        let _ = decision_task.await;
        let _ = executor_task.await;
        if let Some(task) = notify_task {
            let _ = task.await;
        }
        watcher_res
    })
    .await
    {
        Ok(Ok(Ok(()))) => tracing::info!(event = "shutdown_complete"),
        Ok(Ok(Err(err))) => tracing::error!(event = "shutdown_complete", error = %err),
        Ok(Err(join_err)) => tracing::error!(event = "shutdown_complete", error = %join_err),
        Err(_) => tracing::warn!(
            event = "shutdown_timeout",
            timeout_s = SHUTDOWN_TIMEOUT.as_secs()
        ),
    }
    Ok(())
}

fn log_startup(cfg: &SolConfig) {
    tracing::info!(
        event = "startup",
        chain = "solana",
        bot_wallet = %cfg.signer.pubkey(),
        copy_wallets = ?cfg.copy_wallets.iter().map(ToString::to_string).collect::<Vec<_>>(),
        strategy = ?cfg.strategy,
        sell_ratio_bps = cfg.sell_ratio_bps,
        max_trade_lamports = cfg.max_trade_lamports,
        max_slippage_bps = cfg.max_slippage_bps,
        priority_fee_micro_lamports = cfg.priority_fee_micro_lamports,
        fee_reserve_lamports = cfg.fee_reserve_lamports,
        venues = ?cfg.venues,
        token_blacklist = cfg.token_blacklist.len(),
        trade_cooldown_s = cfg.trade_cooldown.as_secs(),
        channel_capacity = cfg.channel_capacity,
        executor_workers = cfg.executor_workers,
        telegram = cfg.telegram.is_some(),
        rpc = %redact_url(&cfg.rpc_url),
        grpc = %redact_url(&cfg.grpc_url),
        grpc_x_token = cfg.grpc_x_token.is_some(),
        dry_run = cfg.dry_run,
    );
    if !cfg.dry_run {
        tracing::warn!(
            event = "live_mode",
            "SOL_DRY_RUN=false: transactions will be signed and sent with REAL funds"
        );
    }
}
