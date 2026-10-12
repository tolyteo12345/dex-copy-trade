use alloy::primitives::{Address, Bytes, U256, address};
use alloy::rpc::types::TransactionRequest;
use alloy::sol;
use alloy::sol_types::SolCall;

use super::{DexAdapter, DexError, SwapParams, check_route};
use crate::eth::models::{PoolKind, RouteLeg, TradeSide};
use crate::eth::utils::ChainAddresses;

sol! {
    interface IQuoterV2 {
        function quoteExactInput(bytes path, uint256 amountIn) external returns (uint256 amountOut, uint160[] sqrtPriceX96AfterList, uint32[] initializedTicksCrossedList, uint256 gasEstimate);
    }
    interface ISwapRouter02Exec {
        struct ExactInputParams { bytes path; address recipient; uint256 amountIn; uint256 amountOutMinimum; }
        function exactInput(ExactInputParams params) external payable returns (uint256 amountOut);
        function multicall(uint256 deadline, bytes[] data) external payable returns (bytes[] results);
        function unwrapWETH9(uint256 amountMinimum, address recipient) external payable;
    }
}

use ISwapRouter02Exec as R;

/// SwapRouter02 `Constants.ADDRESS_THIS`.
const ADDRESS_THIS: Address = address!("0000000000000000000000000000000000000002");

/// SwapRouter02 for swaps, QuoterV2 for quotes.
pub struct UniswapV3Adapter {
    router: Address,
    quoter: Address,
    weth: Address,
}

impl UniswapV3Adapter {
    pub fn new(addrs: &ChainAddresses) -> Self {
        Self {
            router: addrs.uniswap_v3_swap_router02,
            quoter: addrs.uniswap_v3_quoter_v2,
            weth: addrs.weth,
        }
    }
}

/// `token (20) | fee (3) | token (20) | ...` in swap direction.
pub fn encode_v3_path(route: &RouteLeg) -> Result<Bytes, DexError> {
    if route.pool != PoolKind::UniswapV3 {
        return Err(DexError::InvalidRoute("not a V3 path"));
    }
    if route.tokens.len() < 2 || route.fees.len() != route.tokens.len() - 1 {
        return Err(DexError::InvalidRoute("V3 path needs one fee per hop"));
    }
    let mut out = Vec::with_capacity(20 + 23 * route.fees.len());
    out.extend_from_slice(route.tokens[0].as_slice());
    for (fee, token) in route.fees.iter().zip(&route.tokens[1..]) {
        if *fee >= 1 << 24 {
            return Err(DexError::InvalidRoute("V3 fee does not fit uint24"));
        }
        out.extend_from_slice(&fee.to_be_bytes()[1..]);
        out.extend_from_slice(token.as_slice());
    }
    Ok(out.into())
}

impl DexAdapter for UniswapV3Adapter {
    fn pool(&self) -> PoolKind {
        PoolKind::UniswapV3
    }

    fn spender(&self) -> Address {
        self.router
    }

    fn quote_call(
        &self,
        route: &RouteLeg,
        amount_in: U256,
    ) -> Result<TransactionRequest, DexError> {
        let call = IQuoterV2::quoteExactInputCall {
            path: encode_v3_path(route)?,
            amountIn: amount_in,
        };
        Ok(TransactionRequest::default()
            .to(self.quoter)
            .input(call.abi_encode().into()))
    }

    fn decode_quote(&self, return_data: &[u8]) -> Result<U256, DexError> {
        IQuoterV2::quoteExactInputCall::abi_decode_returns(return_data)
            .map(|r| r.amountOut)
            .map_err(|e| DexError::BadQuote(e.to_string()))
    }

    fn swap_tx(&self, route: &RouteLeg, p: &SwapParams) -> Result<TransactionRequest, DexError> {
        check_route(route, PoolKind::UniswapV3, p.side, self.weth)?;
        let path = encode_v3_path(route)?;
        let swap = |recipient| {
            R::exactInputCall {
                params: R::ExactInputParams {
                    path: path.clone(),
                    recipient,
                    amountIn: p.amount_in,
                    amountOutMinimum: p.min_out,
                },
            }
            .abi_encode()
            .into()
        };
        let (calls, value) = match p.side {
            // Router wraps msg.value into WETH and pays the pool.
            TradeSide::Buy => (vec![swap(p.recipient)], Some(p.amount_in)),
            // WETH lands on the router, then is unwrapped to native ETH for the recipient.
            TradeSide::Sell => (
                vec![
                    swap(ADDRESS_THIS),
                    R::unwrapWETH9Call {
                        amountMinimum: p.min_out,
                        recipient: p.recipient,
                    }
                    .abi_encode()
                    .into(),
                ],
                None,
            ),
        };
        let input = R::multicallCall {
            deadline: p.deadline,
            data: calls,
        }
        .abi_encode();
        let tx = TransactionRequest::default()
            .to(self.router)
            .input(input.into());
        Ok(match value {
            Some(v) => tx.value(v),
            None => tx,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eth::models::test_util::{TOKEN, WETH};
    use crate::eth::utils::chain_addresses;

    const BOT: Address = address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266");

    fn v3(tokens: Vec<Address>, fees: Vec<u32>) -> RouteLeg {
        RouteLeg {
            pool: PoolKind::UniswapV3,
            tokens,
            fees,
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
    fn path_encoding() {
        let p = encode_v3_path(&v3(vec![WETH, TOKEN], vec![3000])).unwrap();
        assert_eq!(p.len(), 43);
        assert_eq!(&p[..20], WETH.as_slice());
        assert_eq!(&p[20..23], &[0x00, 0x0b, 0xb8]);
        assert_eq!(&p[23..], TOKEN.as_slice());
        assert!(encode_v3_path(&v3(vec![WETH, TOKEN], vec![])).is_err());
        assert!(encode_v3_path(&v3(vec![WETH, TOKEN], vec![1 << 24])).is_err());
    }

    #[test]
    fn buy_sends_value_to_router_and_pays_bot() {
        let a = UniswapV3Adapter::new(&chain_addresses(1).unwrap());
        let tx = a
            .swap_tx(&v3(vec![WETH, TOKEN], vec![500]), &params(TradeSide::Buy))
            .unwrap();
        assert_eq!(tx.value, Some(U256::from(1000)));
        let mc = R::multicallCall::abi_decode(tx.input.input().unwrap()).unwrap();
        assert_eq!(mc.deadline, U256::from(42));
        assert_eq!(mc.data.len(), 1);
        let swap = R::exactInputCall::abi_decode(&mc.data[0]).unwrap();
        assert_eq!(swap.params.recipient, BOT);
        assert_eq!(swap.params.amountOutMinimum, U256::from(900));
    }

    #[test]
    fn sell_unwraps_weth_to_bot() {
        let a = UniswapV3Adapter::new(&chain_addresses(1).unwrap());
        let tx = a
            .swap_tx(&v3(vec![TOKEN, WETH], vec![3000]), &params(TradeSide::Sell))
            .unwrap();
        assert_eq!(tx.value, None);
        let mc = R::multicallCall::abi_decode(tx.input.input().unwrap()).unwrap();
        let swap = R::exactInputCall::abi_decode(&mc.data[0]).unwrap();
        assert_eq!(swap.params.recipient, ADDRESS_THIS);
        let unwrap = R::unwrapWETH9Call::abi_decode(&mc.data[1]).unwrap();
        assert_eq!(
            (unwrap.recipient, unwrap.amountMinimum),
            (BOT, U256::from(900))
        );
    }

    #[test]
    fn quote_decodes_amount_out() {
        let a = UniswapV3Adapter::new(&chain_addresses(1).unwrap());
        let ret =
            IQuoterV2::quoteExactInputCall::abi_encode_returns(&IQuoterV2::quoteExactInputReturn {
                amountOut: U256::from(5),
                sqrtPriceX96AfterList: vec![],
                initializedTicksCrossedList: vec![],
                gasEstimate: U256::ZERO,
            });
        assert_eq!(a.decode_quote(&ret).unwrap(), U256::from(5));
    }
}
