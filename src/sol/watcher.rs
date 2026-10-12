//! Yellowstone (Geyser) gRPC subscription: successful, non-vote transactions that mention a
//! watched wallet, at `processed` commitment (the earliest point a tx is visible on Solana,
//! which has no public mempool).
//!
//! Intentionally no backfill: txs executed while disconnected are not replayed (the ETH
//! watcher backfills ≤32 blocks). Copying minutes-old pump trades is rarely wanted; add a
//! `getSignaturesForAddress` catch-up here if it ever is.

use std::collections::HashMap;
use std::str::FromStr;
use std::time::{Duration, Instant};

use futures::{SinkExt, StreamExt};
use solana_pubkey::Pubkey;
use solana_signature::Signature;
use tokio::sync::{mpsc, watch};
use yellowstone_grpc_client::{ClientTlsConfig, GeyserGrpcClient};
use yellowstone_grpc_proto::prelude::{
    CommitmentLevel, SubscribeRequest, SubscribeRequestFilterTransactions, SubscribeRequestPing,
    SubscribeUpdateTransaction, TokenBalance as ProtoTokenBalance, subscribe_update::UpdateOneof,
};

use crate::common::backoff::Backoff;
use crate::common::redact::{redact_url, redact_urls_in};
use crate::sol::models::{Ix, SourceTx, TokenBalance};

const BACKOFF_INITIAL: Duration = Duration::from_millis(500);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Large blocks of inner instructions/logs can exceed tonic's 4 MiB default.
const MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum WatcherError {
    /// Sanitized: tonic errors may contain the endpoint (API key).
    #[error("grpc: {0}")]
    Grpc(String),
    #[error("grpc stream ended")]
    StreamEnded,
}

fn grpc_err(e: impl std::fmt::Display) -> WatcherError {
    WatcherError::Grpc(redact_urls_in(&e.to_string()))
}

pub struct SolWatcher {
    endpoint: String,
    x_token: Option<String>,
    wallets: Vec<Pubkey>,
    out: mpsc::Sender<SourceTx>,
    shutdown: watch::Receiver<bool>,
}

impl SolWatcher {
    pub fn new(
        endpoint: String,
        x_token: Option<String>,
        wallets: Vec<Pubkey>,
        out: mpsc::Sender<SourceTx>,
        shutdown: watch::Receiver<bool>,
    ) -> Self {
        Self {
            endpoint,
            x_token,
            wallets,
            out,
            shutdown,
        }
    }

    /// Reconnects forever with backoff; returns on shutdown or when the pipeline is gone.
    pub async fn run(mut self) -> Result<(), WatcherError> {
        let endpoint = redact_url(&self.endpoint);
        let mut backoff = Backoff::new(BACKOFF_INITIAL, BACKOFF_MAX);
        loop {
            if *self.shutdown.borrow() {
                return Ok(());
            }
            match self.stream_once(&mut backoff).await {
                Ok(()) => return Ok(()),
                Err(e) => tracing::warn!(event = "sol_watcher_disconnected", %endpoint, error = %e),
            }
            let delay = backoff.next_delay();
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                _ = self.shutdown.changed() => return Ok(()),
            }
        }
    }

    /// `Ok` = stop for good (shutdown or downstream closed); `Err` = reconnect.
    async fn stream_once(&mut self, backoff: &mut Backoff) -> Result<(), WatcherError> {
        let mut builder = GeyserGrpcClient::build_from_shared(self.endpoint.clone())
            .map_err(grpc_err)?
            .x_token(self.x_token.clone())
            .map_err(|_| WatcherError::Grpc("invalid SOL_GRPC_X_TOKEN".into()))?
            .connect_timeout(CONNECT_TIMEOUT)
            .max_decoding_message_size(MAX_MESSAGE_BYTES);
        if self.endpoint.starts_with("https://") {
            builder = builder
                .tls_config(ClientTlsConfig::new().with_native_roots())
                .map_err(grpc_err)?;
        }
        let mut client = builder.connect().await.map_err(grpc_err)?;
        let (mut sink, mut stream) = client
            .subscribe_with_request(Some(self.request()))
            .await
            .map_err(grpc_err)?;
        tracing::info!(
            event = "sol_watcher_subscribed",
            endpoint = %redact_url(&self.endpoint),
            wallets = self.wallets.len(),
        );
        backoff.reset();

        loop {
            let update = tokio::select! {
                _ = self.shutdown.changed() => return Ok(()),
                msg = stream.next() => match msg {
                    Some(Ok(update)) => update,
                    Some(Err(status)) => return Err(grpc_err(status)),
                    None => return Err(WatcherError::StreamEnded),
                },
            };
            match update.update_oneof {
                Some(UpdateOneof::Transaction(tx)) => {
                    let Some(source) = source_tx_from_update(tx, Instant::now()) else {
                        continue;
                    };
                    if self.out.send(source).await.is_err() {
                        return Ok(());
                    }
                }
                // Some providers' load balancers drop streams without client pings.
                Some(UpdateOneof::Ping(_)) => {
                    let ping = SubscribeRequest {
                        ping: Some(SubscribeRequestPing { id: 1 }),
                        ..Default::default()
                    };
                    if let Err(e) = sink.send(ping).await {
                        return Err(grpc_err(e));
                    }
                }
                _ => {}
            }
        }
    }

    fn request(&self) -> SubscribeRequest {
        let filter = SubscribeRequestFilterTransactions {
            vote: Some(false),
            failed: Some(false),
            account_include: self.wallets.iter().map(Pubkey::to_string).collect(),
            ..Default::default()
        };
        SubscribeRequest {
            transactions: HashMap::from([("copy_wallets".to_string(), filter)]),
            commitment: Some(CommitmentLevel::Processed as i32),
            ..Default::default()
        }
    }
}

/// Converts a gRPC transaction update; `None` for failed or malformed txs.
pub fn source_tx_from_update(
    update: SubscribeUpdateTransaction,
    seen_at: Instant,
) -> Option<SourceTx> {
    let info = update.transaction?;
    let meta = info.meta?;
    if meta.err.is_some() || info.is_vote {
        return None;
    }
    let msg = info.transaction?.message?;
    let key = |b: &Vec<u8>| Pubkey::try_from(b.as_slice()).ok();
    let account_keys = msg
        .account_keys
        .iter()
        .chain(&meta.loaded_writable_addresses)
        .chain(&meta.loaded_readonly_addresses)
        .map(key)
        .collect::<Option<Vec<_>>>()?;

    let mut instructions: Vec<Ix> = msg
        .instructions
        .into_iter()
        .map(|ix| Ix {
            program_id_index: ix.program_id_index as usize,
            accounts: ix.accounts,
            data: ix.data,
        })
        .collect();
    for group in meta.inner_instructions {
        instructions.extend(group.instructions.into_iter().map(|ix| Ix {
            program_id_index: ix.program_id_index as usize,
            accounts: ix.accounts,
            data: ix.data,
        }));
    }
    let balances = |list: Vec<ProtoTokenBalance>| -> Vec<TokenBalance> {
        list.into_iter()
            .filter_map(|b| {
                Some(TokenBalance {
                    account_index: b.account_index as usize,
                    mint: Pubkey::from_str(&b.mint).ok()?,
                    owner: Pubkey::from_str(&b.owner).ok(),
                    amount: b.ui_token_amount?.amount.parse().ok()?,
                })
            })
            .collect()
    };
    Some(SourceTx {
        signature: Signature::try_from(info.signature.as_slice()).ok()?,
        slot: update.slot,
        account_keys,
        num_signers: msg.header?.num_required_signatures as usize,
        instructions,
        pre_token_balances: balances(meta.pre_token_balances),
        post_token_balances: balances(meta.post_token_balances),
        seen_at,
    })
}

#[cfg(test)]
mod tests {
    use yellowstone_grpc_proto::prelude::{
        CompiledInstruction, InnerInstruction, InnerInstructions, Message, MessageHeader,
        SubscribeUpdateTransactionInfo, Transaction, TransactionError, TransactionStatusMeta,
        UiTokenAmount,
    };

    use super::*;

    fn update(err: bool) -> (SubscribeUpdateTransaction, Pubkey, Pubkey) {
        let (signer, loaded) = (Pubkey::new_unique(), Pubkey::new_unique());
        let mint = Pubkey::new_unique();
        let meta = TransactionStatusMeta {
            err: err.then(|| TransactionError { err: vec![1] }),
            loaded_writable_addresses: vec![loaded.to_bytes().to_vec()],
            inner_instructions: vec![InnerInstructions {
                index: 0,
                instructions: vec![InnerInstruction {
                    program_id_index: 1,
                    accounts: vec![0],
                    data: vec![9, 9],
                    stack_height: Some(2),
                }],
            }],
            post_token_balances: vec![ProtoTokenBalance {
                account_index: 1,
                mint: mint.to_string(),
                owner: signer.to_string(),
                program_id: String::new(),
                ui_token_amount: Some(UiTokenAmount {
                    amount: "12345".into(),
                    ..Default::default()
                }),
            }],
            ..Default::default()
        };
        let msg = Message {
            header: Some(MessageHeader {
                num_required_signatures: 1,
                ..Default::default()
            }),
            account_keys: vec![signer.to_bytes().to_vec()],
            instructions: vec![CompiledInstruction {
                program_id_index: 1,
                accounts: vec![0],
                data: vec![1],
            }],
            ..Default::default()
        };
        let u = SubscribeUpdateTransaction {
            transaction: Some(SubscribeUpdateTransactionInfo {
                signature: vec![4u8; 64],
                is_vote: false,
                transaction: Some(Transaction {
                    signatures: vec![vec![4u8; 64]],
                    message: Some(msg),
                }),
                meta: Some(meta),
                index: 0,
            }),
            slot: 77,
            bank_id: 0,
        };
        (u, signer, mint)
    }

    #[test]
    fn converts_update_with_loaded_keys_inner_ixs_and_balances() {
        let (u, signer, mint) = update(false);
        let tx = source_tx_from_update(u, Instant::now()).unwrap();
        assert_eq!(tx.slot, 77);
        assert_eq!(tx.signature, Signature::from([4u8; 64]));
        assert_eq!(tx.account_keys.len(), 2, "static + loaded");
        assert_eq!(tx.account_keys[0], signer);
        assert_eq!(tx.num_signers, 1);
        assert_eq!(tx.instructions.len(), 2, "top-level + inner");
        assert_eq!(tx.instructions[1].data, vec![9, 9]);
        let b = &tx.post_token_balances[0];
        assert_eq!((b.mint, b.owner, b.amount), (mint, Some(signer), 12_345));
    }

    #[test]
    fn failed_tx_is_dropped() {
        assert!(source_tx_from_update(update(true).0, Instant::now()).is_none());
    }
}
