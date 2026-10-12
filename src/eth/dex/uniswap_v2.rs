use alloy::primitives::{Address, U256};
use alloy::rpc::types::TransactionRequest;
use alloy::sol;
use alloy::sol_types::SolCall;

use super::{DexAdapter, DexError, SwapParams, check_route};
use crate::eth::models::{PoolKind, RouteLeg, TradeSide};
use crate::eth::utils::ChainAddresses;

sol! {
    interface IUniswapV2Router02Exec {
        function getAmountsOut(uint256 amountIn, address[] path) external view returns (uint256[] amounts);
        function swapExactETHForTokensSupportingFeeOnTransferTokens(uint256 amountOutMin, address[] path, address to, uint256 deadline) external payable;
        function swapExactTokensForETHSupportingFeeOnTransferTokens(uint256 amountIn, uint256 amountOutMin, address[] path, address to, uint256 deadline) external;
    }
}

use IUniswapV2Router02Exec as R;

/// Router02 with the fee-on-transfer-safe swap variants (work for normal tokens too).
pub struct UniswapV2Adapter {
    router: Address,
    weth: Address,
}

impl UniswapV2Adapter {
    pub fn new(addrs: &ChainAddresses) -> Self {
        Self {
            router: addrs.uniswap_v2_router,
            weth: addrs.weth,
        }
    }
}

impl DexAdapter for UniswapV2Adapter {
    fn pool(&self) -> PoolKind {
        PoolKind::UniswapV2
    }

    fn spender(&self) -> Address {
        self.router
    }

    fn quote_call(
        &self,
        route: &RouteLeg,
        amount_in: U256,
    ) -> Result<TransactionRequest, DexError> {
        if route.pool != PoolKind::UniswapV2 || route.tokens.len() < 2 {
            return Err(DexError::InvalidRoute("not a V2 path"));
        }
        let call = R::getAmountsOutCall {
            amountIn: amount_in,
            path: route.tokens.clone(),
        };
        Ok(TransactionRequest::default()
            .to(self.router)
            .input(call.abi_encode().into()))
    }

    fn decode_quote(&self, return_data: &[u8]) -> Result<U256, DexError> {
        let amounts = R::getAmountsOutCall::abi_decode_returns(return_data)
            .map_err(|e| DexError::BadQuote(e.to_string()))?;
        amounts
            .last()
            .copied()
            .ok_or_else(|| DexError::BadQuote("empty amounts".into()))
    }

    fn swap_tx(&self, route: &RouteLeg, p: &SwapParams) -> Result<TransactionRequest, DexError> {
        check_route(route, PoolKind::UniswapV2, p.side, self.weth)?;
        let path = route.tokens.clone();
        let tx = TransactionRequest::default().to(self.router);
        Ok(match p.side {
            TradeSide::Buy => tx.value(p.amount_in).input(
                R::swapExactETHForTokensSupportingFeeOnTransferTokensCall {
                    amountOutMin: p.min_out,
                    path,
                    to: p.recipient,
                    deadline: p.deadline,
                }
                .abi_encode()
                .into(),
            ),
            TradeSide::Sell => tx.input(
                R::swapExactTokensForETHSupportingFeeOnTransferTokensCall {
                    amountIn: p.amount_in,
                    amountOutMin: p.min_out,
                    path,
                    to: p.recipient,
                    deadline: p.deadline,
                }
                .abi_encode()
                .into(),
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eth::models::test_util::{TOKEN, WETH};
    use crate::eth::utils::chain_addresses;
    use alloy::primitives::address;

    const BOT: Address = address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266");

    fn v2(tokens: Vec<Address>) -> RouteLeg {
        RouteLeg {
            pool: PoolKind::UniswapV2,
            tokens,
            fees: vec![],
            v4: None,
        }
    }

    fn params(side: TradeSide) -> SwapParams {
        SwapParams {
            side,
            amount_in: U256::from(1000),
            min_out: U256::from(900),
            recipient: BOT,
            deadline: U256::from(42),
        }
    }

    #[test]
    fn buy_pays_native_eth_and_uses_fot_variant() {
        let a = UniswapV2Adapter::new(&chain_addresses(1).unwrap());
        let tx = a
            .swap_tx(&v2(vec![WETH, TOKEN]), &params(TradeSide::Buy))
            .unwrap();
        assert_eq!(tx.value, Some(U256::from(1000)));
        let input = tx.input.input().unwrap();
        let call =
            R::swapExactETHForTokensSupportingFeeOnTransferTokensCall::abi_decode(input).unwrap();
        assert_eq!(call.amountOutMin, U256::from(900));
        assert_eq!(call.to, BOT);
        assert_eq!(call.path, vec![WETH, TOKEN]);
    }

    #[test]
    fn sell_receives_native_eth() {
        let a = UniswapV2Adapter::new(&chain_addresses(1).unwrap());
        let tx = a
            .swap_tx(&v2(vec![TOKEN, WETH]), &params(TradeSide::Sell))
            .unwrap();
        assert_eq!(tx.value, None);
        let call = R::swapExactTokensForETHSupportingFeeOnTransferTokensCall::abi_decode(
            tx.input.input().unwrap(),
        )
        .unwrap();
        assert_eq!(call.amountIn, U256::from(1000));
        assert_eq!(a.spender(), chain_addresses(1).unwrap().uniswap_v2_router);
    }

    #[test]
    fn rejects_wrong_direction() {
        let a = UniswapV2Adapter::new(&chain_addresses(1).unwrap());
        assert!(
            a.swap_tx(&v2(vec![TOKEN, WETH]), &params(TradeSide::Buy))
                .is_err()
        );
        assert!(a.swap_tx(&v2(vec![WETH]), &params(TradeSide::Buy)).is_err());
    }

    #[test]
    fn quote_roundtrip() {
        let a = UniswapV2Adapter::new(&chain_addresses(1).unwrap());
        assert!(a.quote_call(&v2(vec![WETH, TOKEN]), U256::from(1)).is_ok());
        let ret = R::getAmountsOutCall::abi_encode_returns(&vec![U256::from(1), U256::from(77)]);
        assert_eq!(a.decode_quote(&ret).unwrap(), U256::from(77));
        assert!(a.decode_quote(&[]).is_err());
    }
}
