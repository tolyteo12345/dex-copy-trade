#![forbid(unsafe_code)]

use std::sync::Arc;
use std::time::Duration;

use alloy::primitives::utils::{format_ether, format_units};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use anyhow::Context;
use tokio::sync::{mpsc, watch};
use tracing_subscriber::EnvFilter;

use dex_copy_trade::config::Config;
use dex_copy_trade::decision::{self, DecisionStage};
use dex_copy_trade::decoder::{self, DecoderRegistry};
use dex_copy_trade::ethereum::chain_state::{ChainReader, RpcChainReader};
use dex_copy_trade::ethereum::nonce::NonceManager;
use dex_copy_trade::ethereum::provider;
use dex_copy_trade::execution::broadcast::{Broadcaster, RpcSender};
use dex_copy_trade::execution::executor::{self, ExecutionStage, Executor, ExecutorConfig};
use dex_copy_trade::execution::gas::{CappedEip1559, RpcFeeSource};
use dex_copy_trade::execution::simulator::{RpcSimulator, Simulator};
use dex_copy_trade::models::{CopyTrade, TradeIntent};
use dex_copy_trade::risk::{RiskConfig, RiskEngine};
use dex_copy_trade::storage::{DEDUP_TTL, InMemoryDedup};
use dex_copy_trade::strategy::Sizer;
use dex_copy_trade::utils::chain_addresses;
use dex_copy_trade::watcher::{WalletFilter, WatchedTx, Watcher};

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(15);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // A missing .env is fine (env may come from docker/systemd).
    let _ = dotenvy::dotenv();
    init_tracing()?;

    let config = Config::from_env().context("invalid configuration")?;
    log_startup(&config);
    let addrs = chain_addresses(config.chain_id).with_context(|| {
        format!(
            "CHAIN_ID={} has no router/WETH address table (only mainnet is supported)",
            config.chain_id
        )
    })?;

    let http = provider::connect_http(&config.http_url)?;
    provider::ensure_chain_id(&http, config.chain_id)
        .await
        .context("startup RPC check failed")?;

    // A bot EOA with code is usually an EIP-7702 delegation planted by someone holding the
    // key (a "sweeper" that forwards incoming ETH). Never trade from such a wallet.
    let bot_code = http
        .get_code_at(config.signer.address())
        .await
        .context("reading bot wallet code")?;
    if !bot_code.is_empty() {
        let msg = format!(
            "bot wallet {} has code/delegation ({} bytes): the key may be compromised",
            config.signer.address(),
            bot_code.len()
        );
        if !config.dry_run {
            anyhow::bail!("{msg}; refusing to trade with DRY_RUN=false");
        }
        tracing::warn!(event = "bot_wallet_has_code", "{msg}");
    }

    let simulator = RpcSimulator::new(http.clone());
    simulator
        .simulate(vec![TransactionRequest::default().to(addrs.weth)])
        .await
        .context("RPC must support eth_simulateV1 (used for quotes and pre-trade simulation)")?;
    let broadcaster = (!config.dry_run).then(|| {
        Broadcaster::new(
            RpcSender::new(http.clone()),
            config.signer.clone(),
            NonceManager::new(),
            config.chain_id,
        )
    });
    let execution = Arc::new(ExecutionStage {
        executor: Executor::new(
            RpcChainReader::new(http.clone(), addrs.eth_usd_feed),
            simulator,
            &addrs,
            ExecutorConfig {
                bot: config.signer.address(),
                max_slippage_bps: config.max_slippage_bps,
                max_trade_wei: config.max_trade_wei,
            },
        ),
        gas: CappedEip1559::new(
            RpcFeeSource::new(http.clone()),
            config.max_gas_wei,
            config.gas_cap_mode,
        ),
        broadcaster,
    });

    let reader = RpcChainReader::new(http, addrs.eth_usd_feed);
    let sizer = Sizer::from_config(&config);
    if sizer.needs().eth_usd {
        let price = reader
            .eth_usd()
            .await
            .context("FIXED_USD needs the Chainlink ETH/USD feed")?;
        tracing::info!(event = "eth_usd_price", answer = %price.answer, updated_at = price.updated_at);
    }
    let risk = RiskEngine::new(
        RiskConfig::from_config(&config),
        InMemoryDedup::new(DEDUP_TTL),
    );
    let decision = DecisionStage::new(reader, sizer, risk, config.signer.address(), addrs.weth);

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let (tx_out, tx_in) = mpsc::channel::<WatchedTx>(config.channel_capacity);
    let (intent_out, intent_in) = mpsc::channel::<TradeIntent>(config.channel_capacity);
    let (trade_out, trade_in) = mpsc::channel::<CopyTrade>(config.channel_capacity);
    let registry = DecoderRegistry::new(&addrs, &config.supported_dexes);

    let watcher = Watcher::new(
        config.copy_mode,
        config.ws_url.clone(),
        WalletFilter::new(config.copy_wallets.iter().copied()),
        tx_out,
        shutdown_rx,
    );
    let mut watcher_task = tokio::spawn(watcher.run());
    let decoder_task = tokio::spawn(decoder::run_stage(registry, tx_in, intent_out));
    let decision_task = tokio::spawn(decision::run_stage(decision, intent_in, trade_out));
    let executor_task = tokio::spawn(executor::run_stage(
        execution,
        trade_in,
        config.executor_workers,
    ));

    tokio::select! {
        res = &mut watcher_task => {
            // Watcher only returns early on a fatal startup error.
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

fn init_tracing() -> anyhow::Result<()> {
    let level = std::env::var("LOG_LEVEL").unwrap_or_else(|_| "info".to_string());
    // Alloy transport spans carry the full RPC URL (API key) at debug/trace; these
    // directives are appended last so they win even when LOG_LEVEL=trace.
    let directives = format!("{level},alloy_transport_http=info,alloy_transport_ws=info");
    let filter =
        EnvFilter::try_new(&directives).with_context(|| format!("invalid LOG_LEVEL `{level}`"))?;
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
    Ok(())
}

fn log_startup(cfg: &Config) {
    tracing::info!(
        event = "startup",
        chain_id = cfg.chain_id,
        bot_wallet = %cfg.signer.address(),
        copy_wallets = ?cfg.copy_wallets,
        mode = ?cfg.copy_mode,
        strategy = ?cfg.strategy,
        fixed_trade_eth = %format_ether(cfg.fixed_trade_wei),
        max_trade_eth = %format_ether(cfg.max_trade_wei),
        buy_ratio_bps = cfg.buy_ratio_bps,
        sell_ratio_bps = cfg.sell_ratio_bps,
        max_slippage_bps = cfg.max_slippage_bps,
        max_gas_gwei = cfg.max_gas_wei / 1_000_000_000,
        supported_dexes = ?cfg.supported_dexes,
        fixed_trade_usd = %format_units(cfg.fixed_trade_usd_micro, 6).unwrap_or_default(),
        token_blacklist = cfg.token_blacklist.len(),
        trade_cooldown_s = cfg.trade_cooldown.as_secs(),
        channel_capacity = cfg.channel_capacity,
        executor_workers = cfg.executor_workers,
        ws = %provider::redact_url(&cfg.ws_url),
        http = %provider::redact_url(&cfg.http_url),
        dry_run = cfg.dry_run,
    );
    if !cfg.dry_run {
        tracing::warn!(
            event = "live_mode",
            "DRY_RUN=false: transactions will be signed and broadcast with REAL funds"
        );
    }
}

async fn shutdown_signal() -> &'static str {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut sigterm) => tokio::select! {
                _ = tokio::signal::ctrl_c() => "SIGINT",
                _ = sigterm.recv() => "SIGTERM",
            },
            Err(err) => {
                tracing::warn!(error = %err, "cannot install SIGTERM handler; only SIGINT handled");
                let _ = tokio::signal::ctrl_c().await;
                "SIGINT"
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        "SIGINT"
    }
}
