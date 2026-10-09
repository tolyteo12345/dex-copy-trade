use alloy::providers::{DynProvider, Provider, ProviderBuilder, WsConnect};

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

/// RPC URLs usually embed an API key in the path/query; keep only scheme + host for logs.
pub fn redact_url(url: &str) -> String {
    match url.split_once("://") {
        Some((scheme, rest)) => {
            let host = rest.split(['/', '?']).next().unwrap_or_default();
            let host = host.rsplit('@').next().unwrap_or_default();
            format!("{scheme}://{host}/***")
        }
        None => "***".to_string(),
    }
}

/// Redacts every `scheme://...` URL embedded in free text (e.g. error messages).
pub fn redact_urls_in(text: &str) -> String {
    const SCHEMES: [&str; 4] = ["https://", "http://", "wss://", "ws://"];
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = SCHEMES.iter().filter_map(|s| rest.find(s)).min() {
        out.push_str(&rest[..start]);
        let tail = &rest[start..];
        let end = tail
            .find(|c: char| c.is_whitespace() || matches!(c, ')' | '"' | '\'' | ',' | '>'))
            .unwrap_or(tail.len());
        out.push_str(&redact_url(&tail[..end]));
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_api_keys_from_urls() {
        assert_eq!(
            redact_url("wss://eth-mainnet.g.alchemy.com/v2/SECRETKEY"),
            "wss://eth-mainnet.g.alchemy.com/***"
        );
        assert_eq!(
            redact_url("https://user:pass@node.example.com:8545?key=abc"),
            "https://node.example.com:8545/***"
        );
        assert_eq!(redact_url("garbage"), "***");
    }

    #[test]
    fn redacts_urls_inside_error_text() {
        let msg = "error sending request for url (http://127.0.0.1:1/SECRET?k=v): \
                   ws failed wss://h.example/v2/KEY2, done";
        let out = redact_urls_in(msg);
        assert!(!out.contains("SECRET") && !out.contains("KEY2") && !out.contains("k=v"));
        assert_eq!(
            out,
            "error sending request for url (http://127.0.0.1:1/***): \
             ws failed wss://h.example/***, done"
        );
        assert_eq!(redact_urls_in("no urls here"), "no urls here");
    }
}
