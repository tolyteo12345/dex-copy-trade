use alloy::consensus::Transaction as _;
use alloy::primitives::{Address, U256};
use alloy::rpc::types::Transaction;
use alloy::sol;
use alloy::sol_types::SolInterface;

use super::{DecodeError, DexDecoder, SwapPlan, Unwrap, v2_leg};
use crate::models::{Dex, SwapKind};
use crate::utils::ChainAddresses;

sol! {
    interface IUniswapV2Router02 {
        function swapExactTokensForTokens(uint256 amountIn, uint256 amountOutMin, address[] path, address to, uint256 deadline);
        function swapTokensForExactTokens(uint256 amountOut, uint256 amountInMax, address[] path, address to, uint256 deadline);
        function swapExactETHForTokens(uint256 amountOutMin, address[] path, address to, uint256 deadline) payable;
        function swapTokensForExactETH(uint256 amountOut, uint256 amountInMax, address[] path, address to, uint256 deadline);
        function swapExactTokensForETH(uint256 amountIn, uint256 amountOutMin, address[] path, address to, uint256 deadline);
        function swapETHForExactTokens(uint256 amountOut, address[] path, address to, uint256 deadline) payable;
        function swapExactTokensForTokensSupportingFeeOnTransferTokens(uint256 amountIn, uint256 amountOutMin, address[] path, address to, uint256 deadline);
        function swapExactETHForTokensSupportingFeeOnTransferTokens(uint256 amountOutMin, address[] path, address to, uint256 deadline) payable;
        function swapExactTokensForETHSupportingFeeOnTransferTokens(uint256 amountIn, uint256 amountOutMin, address[] path, address to, uint256 deadline);
    }
}

use IUniswapV2Router02::IUniswapV2Router02Calls as Call;

pub struct UniswapV2Decoder {
    router: Address,
}

impl UniswapV2Decoder {
    pub fn new(addrs: &ChainAddresses) -> Self {
        Self {
            router: addrs.uniswap_v2_router,
        }
    }
}

impl DexDecoder for UniswapV2Decoder {
    fn dex(&self) -> Dex {
        Dex::UniswapV2
    }

    fn can_decode(&self, tx: &Transaction) -> bool {
        tx.to() == Some(self.router)
            && super::classifier::selector(tx.input()).is_some_and(Call::valid_selector)
    }

    fn decode(&self, tx: &Transaction) -> Result<Option<SwapPlan>, DecodeError> {
        let call = Call::abi_decode(tx.input())
            .map_err(|e| DecodeError::InvalidCalldata(e.to_string()))?;
        let value = tx.value();
        let (exact_in, exact_out) = (SwapKind::ExactIn, SwapKind::ExactOut);

        // (leg, native ETH in, native ETH out)
        let (leg, native_in, native_out) = match call {
            Call::swapExactTokensForTokens(c) => (
                v2_leg(c.path, exact_in, Some(c.amountIn), c.amountOutMin, c.to)?,
                false,
                false,
            ),
            Call::swapExactTokensForTokensSupportingFeeOnTransferTokens(c) => (
                v2_leg(c.path, exact_in, Some(c.amountIn), c.amountOutMin, c.to)?,
                false,
                false,
            ),
            Call::swapTokensForExactTokens(c) => (
                v2_leg(c.path, exact_out, Some(c.amountInMax), c.amountOut, c.to)?,
                false,
                false,
            ),
            Call::swapExactETHForTokens(c) => (
                v2_leg(c.path, exact_in, Some(value), c.amountOutMin, c.to)?,
                true,
                false,
            ),
            Call::swapExactETHForTokensSupportingFeeOnTransferTokens(c) => (
                v2_leg(c.path, exact_in, Some(value), c.amountOutMin, c.to)?,
                true,
                false,
            ),
            Call::swapETHForExactTokens(c) => (
                v2_leg(c.path, exact_out, Some(value), c.amountOut, c.to)?,
                true,
                false,
            ),
            Call::swapExactTokensForETH(c) => (
                v2_leg(c.path, exact_in, Some(c.amountIn), c.amountOutMin, c.to)?,
                false,
                true,
            ),
            Call::swapExactTokensForETHSupportingFeeOnTransferTokens(c) => (
                v2_leg(c.path, exact_in, Some(c.amountIn), c.amountOutMin, c.to)?,
                false,
                true,
            ),
            Call::swapTokensForExactETH(c) => (
                v2_leg(c.path, exact_out, Some(c.amountInMax), c.amountOut, c.to)?,
                false,
                true,
            ),
        };

        // V2 router unwraps internally and pays native ETH straight to `to`.
        let unwrap = native_out.then_some(Unwrap {
            recipient: leg.recipient,
            amount_min: leg.amount_out,
        });
        Ok(Some(SwapPlan {
            router: self.router,
            wrapped_eth: native_in.then_some(value).filter(|v| *v > U256::ZERO),
            unwrap,
            sweeps: Vec::new(),
            legs: vec![leg],
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decoder::test_util::tx;
    use crate::utils::chain_addresses;
    use alloy::primitives::address;
    use alloy::sol_types::SolCall;

    const WETH: Address = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
    const TOKEN: Address = address!("1111111111111111111111111111111111111111");
    const USER: Address = address!("2222222222222222222222222222222222222222");

    fn decoder() -> (UniswapV2Decoder, Address) {
        let addrs = chain_addresses(1).unwrap();
        (UniswapV2Decoder::new(&addrs), addrs.uniswap_v2_router)
    }

    #[test]
    fn decodes_exact_eth_for_tokens() {
        let (d, router) = decoder();
        let input = IUniswapV2Router02::swapExactETHForTokensCall {
            amountOutMin: U256::from(123),
            path: vec![WETH, TOKEN],
            to: USER,
            deadline: U256::from(1),
        }
        .abi_encode();
        let t = tx(USER, Some(router), U256::from(10u64.pow(18)), &input);
        assert!(d.can_decode(&t));
        let plan = d.decode(&t).unwrap().unwrap();
        assert_eq!(plan.wrapped_eth, Some(U256::from(10u64.pow(18))));
        assert_eq!(plan.unwrap, None);
        let leg = &plan.legs[0];
        assert_eq!(leg.route.tokens, vec![WETH, TOKEN]);
        assert_eq!(leg.amount_in, Some(U256::from(10u64.pow(18))));
        assert_eq!(leg.amount_out, U256::from(123));
        assert_eq!(leg.kind, SwapKind::ExactIn);
    }

    #[test]
    fn decodes_tokens_for_exact_eth() {
        let (d, router) = decoder();
        let input = IUniswapV2Router02::swapTokensForExactETHCall {
            amountOut: U256::from(5),
            amountInMax: U256::from(900),
            path: vec![TOKEN, WETH],
            to: USER,
            deadline: U256::from(1),
        }
        .abi_encode();
        let plan = d
            .decode(&tx(USER, Some(router), U256::ZERO, &input))
            .unwrap()
            .unwrap();
        assert_eq!(
            plan.unwrap,
            Some(Unwrap {
                recipient: USER,
                amount_min: U256::from(5)
            })
        );
        assert_eq!(plan.legs[0].kind, SwapKind::ExactOut);
        assert_eq!(plan.legs[0].amount_in, Some(U256::from(900)));
        assert_eq!(plan.legs[0].amount_out, U256::from(5));
    }

    #[test]
    fn ignores_other_routers_and_selectors() {
        let (d, router) = decoder();
        let input = IUniswapV2Router02::swapExactETHForTokensCall {
            amountOutMin: U256::ZERO,
            path: vec![WETH, TOKEN],
            to: USER,
            deadline: U256::ZERO,
        }
        .abi_encode();
        assert!(!d.can_decode(&tx(USER, Some(TOKEN), U256::ZERO, &input)));
        // addLiquidityETH selector is not a swap
        assert!(!d.can_decode(&tx(
            USER,
            Some(router),
            U256::ZERO,
            &[0xf3, 0x05, 0xd7, 0x19]
        )));
    }

    #[test]
    fn rejects_truncated_calldata() {
        let (d, router) = decoder();
        let input = IUniswapV2Router02::swapExactTokensForETHCall::SELECTOR;
        let t = tx(USER, Some(router), U256::ZERO, &input);
        assert!(d.can_decode(&t));
        assert!(matches!(d.decode(&t), Err(DecodeError::InvalidCalldata(_))));
    }
}
