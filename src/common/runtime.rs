//! Process plumbing shared by both binaries.

use anyhow::Context;
use tracing_subscriber::EnvFilter;

pub fn init_tracing() -> anyhow::Result<()> {
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

pub async fn shutdown_signal() -> &'static str {
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
