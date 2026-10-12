//! RPC URLs (and gRPC endpoints) usually embed an API key: never log them verbatim.

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
