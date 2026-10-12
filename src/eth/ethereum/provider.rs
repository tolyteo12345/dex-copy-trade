use alloy::providers::{DynProvider, Provider, ProviderBuilder, WsConnect};

pub use crate::common::redact::{redact_url, redact_urls_in};

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("invalid RPC url: {0}")]
    InvalidUrl(String),
    /// Stored as a sanitized string: alloy/reqwest errors embed the full RPC URL (API key).
    #[error("rpc transport error: {0}")]
    Transport(String),
    #[error("chain id mismatch: CHAIN_ID={expected}, rpc reports {actual}")]
    ChainIdMismatch { expected: u64, actual: u64 },
    #[error("unexpected call result: {0}")]
    BadResponse(String),
}

impl From<alloy::transports::TransportError> for ProviderError {
    fn from(err: alloy::transports::TransportError) -> Self {
        let mut msg = err.to_string();
        let mut source = std::error::Error::source(&err);
        while let Some(cause) = source {
            msg.push_str(": ");
            msg.push_str(&cause.to_string());
            source = cause.source();
        }
        ProviderError::Transport(redact_urls_in(&msg))
    }
}

pub fn connect_http(url: &str) -> Result<DynProvider, ProviderError> {
    let url = url
        .parse()
        .map_err(|_| ProviderError::InvalidUrl(redact_url(url)))?;
    Ok(ProviderBuilder::new().connect_http(url).erased())
}

pub async fn connect_ws(url: &str) -> Result<DynProvider, ProviderError> {
    let provider = ProviderBuilder::new()
        .connect_ws(WsConnect::new(url))
        .await?;
    Ok(provider.erased())
}

pub async fn ensure_chain_id(provider: &DynProvider, expected: u64) -> Result<(), ProviderError> {
    let actual = provider.get_chain_id().await?;
    if actual != expected {
        return Err(ProviderError::ChainIdMismatch { expected, actual });
    }
    Ok(())
}
