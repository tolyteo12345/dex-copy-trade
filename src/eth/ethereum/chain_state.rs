use std::future::Future;

use alloy::eips::BlockId;
use alloy::primitives::{Address, U256};
use alloy::providers::{DynProvider, Provider};
use alloy::rpc::types::TransactionRequest;
use alloy::sol;
use alloy::sol_types::SolCall;

use super::provider::ProviderError;
use crate::eth::models::EthUsdPrice;

sol! {
    interface IERC20 {
        function balanceOf(address owner) external view returns (uint256);
        function allowance(address owner, address spender) external view returns (uint256);
        function approve(address spender, uint256 amount) external returns (bool);
    }
    interface IPermit2 {
        function allowance(address user, address token, address spender) external view returns (uint160 amount, uint48 expiration, uint48 nonce);
        function approve(address token, address spender, uint160 amount, uint48 expiration) external;
    }
    interface IChainlinkAggregator {
        function latestRoundData() external view returns (uint80 roundId, int256 answer, uint256 startedAt, uint256 updatedAt, uint80 answeredInRound);
    }
}

/// Read-only chain access used for trade decisions; mocked in tests.
pub trait ChainReader: Send + Sync {
    fn gas_price(&self) -> impl Future<Output = Result<u128, ProviderError>> + Send;
    fn eth_balance(
        &self,
        who: Address,
        at: BlockId,
    ) -> impl Future<Output = Result<U256, ProviderError>> + Send;
    fn token_balance(
        &self,
        token: Address,
        who: Address,
        at: BlockId,
    ) -> impl Future<Output = Result<U256, ProviderError>> + Send;
    /// Deployed contract code (an EIP-7702 delegated EOA does not count).
    fn is_contract(
        &self,
        addr: Address,
    ) -> impl Future<Output = Result<bool, ProviderError>> + Send;
    fn eth_usd(&self) -> impl Future<Output = Result<EthUsdPrice, ProviderError>> + Send;
    fn allowance(
        &self,
        token: Address,
        owner: Address,
        spender: Address,
    ) -> impl Future<Output = Result<U256, ProviderError>> + Send;
    /// Permit2 allowance `(amount, expiration unix secs)` of `owner`'s `token` for `spender`.
    fn permit2_allowance(
        &self,
        permit2: Address,
        owner: Address,
        token: Address,
        spender: Address,
    ) -> impl Future<Output = Result<(U256, u64), ProviderError>> + Send;
    /// Count of mined txs from `who` (= next nonce at latest block).
    fn mined_nonce(&self, who: Address) -> impl Future<Output = Result<u64, ProviderError>> + Send;
}

/// EIP-7702: an EOA with a delegation carries exactly `0xef0100 || address` (23 bytes).
const DELEGATION_PREFIX: [u8; 3] = [0xef, 0x01, 0x00];

pub fn is_contract_code(code: &[u8]) -> bool {
    let delegated_eoa = code.len() == 23 && code.starts_with(&DELEGATION_PREFIX);
    !code.is_empty() && !delegated_eoa
}

pub struct RpcChainReader {
    provider: DynProvider,
    eth_usd_feed: Address,
}

impl RpcChainReader {
    pub fn new(provider: DynProvider, eth_usd_feed: Address) -> Self {
        Self {
            provider,
            eth_usd_feed,
        }
    }

    async fn call<C: SolCall>(
        &self,
        to: Address,
        call: &C,
        at: BlockId,
    ) -> Result<C::Return, ProviderError> {
        let req = TransactionRequest::default()
            .to(to)
            .input(call.abi_encode().into());
        let out = self.provider.call(req).block(at).await?;
        C::abi_decode_returns(&out)
            .map_err(|e| ProviderError::BadResponse(format!("{} on {to}: {e}", C::SIGNATURE)))
    }
}

impl ChainReader for RpcChainReader {
    async fn gas_price(&self) -> Result<u128, ProviderError> {
        Ok(self.provider.get_gas_price().await?)
    }

    async fn eth_balance(&self, who: Address, at: BlockId) -> Result<U256, ProviderError> {
        Ok(self.provider.get_balance(who).block_id(at).await?)
    }

    async fn token_balance(
        &self,
        token: Address,
        who: Address,
        at: BlockId,
    ) -> Result<U256, ProviderError> {
        self.call(token, &IERC20::balanceOfCall { owner: who }, at)
            .await
    }

    async fn is_contract(&self, addr: Address) -> Result<bool, ProviderError> {
        Ok(is_contract_code(&self.provider.get_code_at(addr).await?))
    }

    async fn eth_usd(&self) -> Result<EthUsdPrice, ProviderError> {
        let round = self
            .call(
                self.eth_usd_feed,
                &IChainlinkAggregator::latestRoundDataCall {},
                BlockId::latest(),
            )
            .await?;
        Ok(EthUsdPrice {
            answer: round.answer,
            updated_at: round.updatedAt.saturating_to(),
        })
    }

    async fn allowance(
        &self,
        token: Address,
        owner: Address,
        spender: Address,
    ) -> Result<U256, ProviderError> {
        self.call(
            token,
            &IERC20::allowanceCall { owner, spender },
            BlockId::latest(),
        )
        .await
    }

    async fn permit2_allowance(
        &self,
        permit2: Address,
        owner: Address,
        token: Address,
        spender: Address,
    ) -> Result<(U256, u64), ProviderError> {
        let r = self
            .call(
                permit2,
                &IPermit2::allowanceCall {
                    user: owner,
                    token,
                    spender,
                },
                BlockId::latest(),
            )
            .await?;
        Ok((U256::from(r.amount), r.expiration.to()))
    }

    async fn mined_nonce(&self, who: Address) -> Result<u64, ProviderError> {
        Ok(self
            .provider
            .get_transaction_count(who)
            .block_id(BlockId::latest())
            .await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::hex;

    #[test]
    fn delegated_eoa_is_not_a_contract() {
        let delegated = hex!("ef01005a7fc11397e9a8ad41bf10bf13f22b0a63f96f6d");
        assert!(!is_contract_code(&delegated));
        assert!(!is_contract_code(&[]));
        assert!(is_contract_code(&hex!("6080604052")));
        // ef0100 prefix with any other length is not a delegation designator
        assert!(is_contract_code(&hex!("ef010000")));
    }
}
