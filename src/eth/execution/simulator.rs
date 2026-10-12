use std::future::Future;

use alloy::primitives::{Bytes, hex};
use alloy::providers::{DynProvider, Provider};
use alloy::rpc::types::TransactionRequest;
use alloy::rpc::types::simulate::{SimBlock, SimulatePayload};
use alloy::sol_types::decode_revert_reason;

use super::SimulationError;
use crate::eth::ethereum::provider::ProviderError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallOutcome {
    pub success: bool,
    pub return_data: Bytes,
    /// Gas charged after refunds.
    pub gas_used: u64,
    /// Peak gas consumed during execution, before refunds: what a gas limit must cover.
    pub peak_gas: u64,
    /// Decoded revert reason when `success == false`.
    pub revert_reason: Option<String>,
}

/// Runs calls sequentially on top of the latest block; each call sees the previous calls'
/// state changes. Nothing is signed or broadcast.
pub trait Simulator: Send + Sync {
    fn simulate(
        &self,
        calls: Vec<TransactionRequest>,
    ) -> impl Future<Output = Result<Vec<CallOutcome>, SimulationError>> + Send;
}

/// `eth_simulateV1` with validation off (no nonce/fee checks; the bot's later real tx gets
/// its own nonce and fees in Phase 5).
pub struct RpcSimulator {
    provider: DynProvider,
}

impl RpcSimulator {
    pub fn new(provider: DynProvider) -> Self {
        Self { provider }
    }
}

impl Simulator for RpcSimulator {
    async fn simulate(
        &self,
        calls: Vec<TransactionRequest>,
    ) -> Result<Vec<CallOutcome>, SimulationError> {
        let expected = calls.len();
        let payload = SimulatePayload {
            block_state_calls: vec![SimBlock {
                block_overrides: None,
                state_overrides: None,
                calls,
            }],
            trace_transfers: false,
            validation: false,
            return_full_transactions: false,
        };
        let blocks = self
            .provider
            .simulate(&payload)
            .await
            .map_err(ProviderError::from)?;
        let results = blocks
            .into_iter()
            .next()
            .map(|b| b.calls)
            .unwrap_or_default();
        if results.len() != expected {
            return Err(SimulationError::ResultCount {
                expected,
                got: results.len(),
            });
        }
        Ok(results
            .into_iter()
            .map(|r| CallOutcome {
                revert_reason: (!r.status).then(|| {
                    let from_data = describe_revert(&r.return_data);
                    match (&r.error, r.return_data.is_empty()) {
                        (Some(e), true) => e.message.clone(),
                        _ => from_data,
                    }
                }),
                success: r.status,
                return_data: r.return_data,
                // Without `maxUsedGas`, undo the largest possible refund (1/5 of gas used).
                peak_gas: r.max_used_gas.unwrap_or(r.gas_used.saturating_mul(5) / 4),
                gas_used: r.gas_used,
            })
            .collect())
    }
}

/// `Error(string)` / `Panic(uint256)` decoded; custom errors shown by selector.
pub fn describe_revert(data: &[u8]) -> String {
    if data.is_empty() {
        return "reverted without data".into();
    }
    if let Some(reason) = decode_revert_reason(data) {
        return reason;
    }
    format!("custom error 0x{}", hex::encode(&data[..data.len().min(4)]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::sol_types::{Revert, SolError};

    #[test]
    fn revert_reasons() {
        let data = Revert::from("UniswapV2Router: INSUFFICIENT_OUTPUT_AMOUNT").abi_encode();
        assert!(describe_revert(&data).contains("INSUFFICIENT_OUTPUT_AMOUNT"));
        assert_eq!(describe_revert(&[]), "reverted without data");
        assert_eq!(
            describe_revert(&hex!("deadbeef00")),
            "custom error 0xdeadbeef"
        );
    }
}
