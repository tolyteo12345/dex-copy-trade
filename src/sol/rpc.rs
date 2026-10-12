//! Minimal Solana JSON-RPC client over the `reqwest` client already in the tree (the
//! official `solana-rpc-client` would pull in hundreds of crates for six methods).

use std::future::Future;
use std::str::FromStr;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde_json::{Value, json};
use solana_hash::Hash;
use solana_pubkey::Pubkey;
use solana_signature::Signature;
use solana_transaction::versioned::VersionedTransaction;

use crate::common::redact::redact_urls_in;

const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
/// Genesis hash of mainnet-beta: the only cluster the pump addresses are valid on.
pub const MAINNET_GENESIS_HASH: &str = "5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d";

#[derive(Debug, thiserror::Error)]
pub enum RpcError {
    /// Sanitized: reqwest errors embed the URL (API key).
    #[error("rpc transport error: {0}")]
    Transport(String),
    #[error("rpc error {code}: {message}")]
    Rpc { code: i64, message: String },
    #[error("unexpected rpc response: {0}")]
    Decode(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountInfo {
    pub lamports: u64,
    pub owner: Pubkey,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimResult {
    /// `None` = the transaction succeeded.
    pub err: Option<String>,
    pub logs: Vec<String>,
    pub units_consumed: Option<u64>,
    /// Post-simulation state of the requested accounts, in request order.
    pub accounts: Vec<Option<AccountInfo>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SigStatus {
    /// Seen but not yet at `confirmed`.
    Processed,
    Confirmed {
        slot: u64,
    },
    Failed {
        slot: u64,
        err: String,
    },
}

/// RPC surface used by the bot; mocked in tests.
pub trait SolRpc: Send + Sync {
    fn get_multiple_accounts(
        &self,
        keys: &[Pubkey],
    ) -> impl Future<Output = Result<Vec<Option<AccountInfo>>, RpcError>> + Send;

    fn latest_blockhash(&self) -> impl Future<Output = Result<Hash, RpcError>> + Send;

    /// `accounts`: addresses whose post-simulation state is returned.
    fn simulate(
        &self,
        tx: &VersionedTransaction,
        accounts: &[Pubkey],
    ) -> impl Future<Output = Result<SimResult, RpcError>> + Send;

    fn send(
        &self,
        tx: &VersionedTransaction,
    ) -> impl Future<Output = Result<Signature, RpcError>> + Send;

    fn signature_status(
        &self,
        sig: &Signature,
    ) -> impl Future<Output = Result<Option<SigStatus>, RpcError>> + Send;
}

/// The URL embeds the API key: no `Debug`, errors are sanitized.
pub struct HttpRpc {
    client: reqwest::Client,
    url: String,
}

impl HttpRpc {
    pub fn new(url: &str) -> Result<Self, RpcError> {
        let client = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .map_err(|e| RpcError::Transport(e.without_url().to_string()))?;
        Ok(Self {
            client,
            url: url.to_string(),
        })
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        let transport =
            |e: reqwest::Error| RpcError::Transport(redact_urls_in(&e.without_url().to_string()));
        let resp = self
            .client
            .post(&self.url)
            .json(&body)
            .send()
            .await
            .map_err(transport)?;
        let status = resp.status();
        let mut v: Value = resp.json().await.map_err(transport)?;
        if let Some(err) = v.get("error") {
            return Err(RpcError::Rpc {
                code: err["code"].as_i64().unwrap_or_default(),
                message: err["message"].as_str().unwrap_or_default().to_string(),
            });
        }
        match v.get_mut("result") {
            Some(result) => Ok(result.take()),
            None => Err(RpcError::Decode(format!(
                "{method}: no result (HTTP {status})"
            ))),
        }
    }

    pub async fn genesis_hash(&self) -> Result<String, RpcError> {
        let v = self.call("getGenesisHash", json!([])).await?;
        v.as_str()
            .map(str::to_string)
            .ok_or_else(|| RpcError::Decode("getGenesisHash: not a string".into()))
    }
}

fn encode_tx(tx: &VersionedTransaction) -> Result<String, RpcError> {
    let bytes =
        wincode::serialize(tx).map_err(|e| RpcError::Decode(format!("serialize tx: {e}")))?;
    Ok(B64.encode(bytes))
}

/// `{lamports, owner, data: [base64, "base64"]}` or `null`.
fn parse_account(v: &Value) -> Result<Option<AccountInfo>, RpcError> {
    if v.is_null() {
        return Ok(None);
    }
    let bad = |what: &str| RpcError::Decode(format!("account: bad `{what}`"));
    let lamports = v["lamports"].as_u64().ok_or_else(|| bad("lamports"))?;
    let owner = v["owner"]
        .as_str()
        .and_then(|s| Pubkey::from_str(s).ok())
        .ok_or_else(|| bad("owner"))?;
    let data = v["data"][0]
        .as_str()
        .and_then(|s| B64.decode(s).ok())
        .ok_or_else(|| bad("data"))?;
    Ok(Some(AccountInfo {
        lamports,
        owner,
        data,
    }))
}

impl SolRpc for HttpRpc {
    async fn get_multiple_accounts(
        &self,
        keys: &[Pubkey],
    ) -> Result<Vec<Option<AccountInfo>>, RpcError> {
        let keys: Vec<String> = keys.iter().map(Pubkey::to_string).collect();
        // `processed`: the source tx was seen at processed, its effects must be visible.
        let v = self
            .call(
                "getMultipleAccounts",
                json!([keys, { "encoding": "base64", "commitment": "processed" }]),
            )
            .await?;
        v["value"]
            .as_array()
            .ok_or_else(|| RpcError::Decode("getMultipleAccounts: no value".into()))?
            .iter()
            .map(parse_account)
            .collect()
    }

    async fn latest_blockhash(&self) -> Result<Hash, RpcError> {
        // `confirmed`: a processed blockhash may belong to a fork that gets dropped.
        let v = self
            .call("getLatestBlockhash", json!([{ "commitment": "confirmed" }]))
            .await?;
        v["value"]["blockhash"]
            .as_str()
            .and_then(|s| Hash::from_str(s).ok())
            .ok_or_else(|| RpcError::Decode("getLatestBlockhash: bad blockhash".into()))
    }

    async fn simulate(
        &self,
        tx: &VersionedTransaction,
        accounts: &[Pubkey],
    ) -> Result<SimResult, RpcError> {
        let addresses: Vec<String> = accounts.iter().map(Pubkey::to_string).collect();
        // Same blockhash as the tx that will be sent (no replace), signatures not checked.
        let v = self
            .call(
                "simulateTransaction",
                json!([encode_tx(tx)?, {
                    "encoding": "base64",
                    "commitment": "processed",
                    "sigVerify": false,
                    "replaceRecentBlockhash": false,
                    "accounts": { "encoding": "base64", "addresses": addresses },
                }]),
            )
            .await?;
        let value = &v["value"];
        let accounts = match value["accounts"].as_array() {
            Some(a) => a.iter().map(parse_account).collect::<Result<_, _>>()?,
            None => Vec::new(),
        };
        Ok(SimResult {
            err: (!value["err"].is_null()).then(|| value["err"].to_string()),
            logs: value["logs"]
                .as_array()
                .map(|l| {
                    l.iter()
                        .filter_map(|s| s.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
            units_consumed: value["unitsConsumed"].as_u64(),
            accounts,
        })
    }

    async fn send(&self, tx: &VersionedTransaction) -> Result<Signature, RpcError> {
        // Preflight skipped: the exact tx (modulo min_out / CU limit) was just simulated.
        let v = self
            .call(
                "sendTransaction",
                json!([encode_tx(tx)?, { "encoding": "base64", "skipPreflight": true }]),
            )
            .await?;
        v.as_str()
            .and_then(|s| Signature::from_str(s).ok())
            .ok_or_else(|| RpcError::Decode("sendTransaction: bad signature".into()))
    }

    async fn signature_status(&self, sig: &Signature) -> Result<Option<SigStatus>, RpcError> {
        let v = self
            .call("getSignatureStatuses", json!([[sig.to_string()]]))
            .await?;
        let s = &v["value"][0];
        if s.is_null() {
            return Ok(None);
        }
        let slot = s["slot"].as_u64().unwrap_or_default();
        if !s["err"].is_null() {
            return Ok(Some(SigStatus::Failed {
                slot,
                err: s["err"].to_string(),
            }));
        }
        Ok(Some(match s["confirmationStatus"].as_str() {
            Some("confirmed" | "finalized") => SigStatus::Confirmed { slot },
            _ => SigStatus::Processed,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_accounts_and_nulls() {
        let owner = Pubkey::new_unique();
        let v = json!({ "lamports": 5, "owner": owner.to_string(), "data": [B64.encode([1, 2]), "base64"] });
        assert_eq!(
            parse_account(&v).unwrap(),
            Some(AccountInfo {
                lamports: 5,
                owner,
                data: vec![1, 2]
            })
        );
        assert_eq!(parse_account(&Value::Null).unwrap(), None);
        assert!(parse_account(&json!({ "lamports": 1 })).is_err());
    }

    #[tokio::test]
    async fn transport_errors_do_not_leak_the_url() {
        let rpc = HttpRpc::new("http://127.0.0.1:9/SECRET-KEY").unwrap();
        let err = rpc.latest_blockhash().await.unwrap_err();
        assert!(matches!(err, RpcError::Transport(_)));
        assert!(!err.to_string().contains("SECRET"), "{err}");
    }
}
