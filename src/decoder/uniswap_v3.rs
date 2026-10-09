//! Uniswap V3 `SwapRouter` and `SwapRouter02` (which also exposes V2 swaps without deadline).
//! Calls may be wrapped in `multicall` together with `unwrapWETH9`/`refundETH`/`selfPermit*`.

use alloy::consensus::Transaction as _;
use alloy::network::TransactionResponse;
use alloy::primitives::{Address, U256, address, hex};
use alloy::rpc::types::Transaction;
use alloy::sol;
use alloy::sol_types::SolInterface;

use super::classifier::selector;
use super::{
    DecodeError, DexDecoder, SwapLeg, SwapPlan, Sweep, Unwrap, v2_leg, v3_leg, v3_single_leg,
};
use crate::models::{Dex, SwapKind};
use crate::utils::ChainAddresses;

sol! {
    interface ISwapRouter {
        struct ExactInputSingleParams { address tokenIn; address tokenOut; uint24 fee; address recipient; uint256 deadline; uint256 amountIn; uint256 amountOutMinimum; uint160 sqrtPriceLimitX96; }
        struct ExactInputParams { bytes path; address recipient; uint256 deadline; uint256 amountIn; uint256 amountOutMinimum; }
        struct ExactOutputSingleParams { address tokenIn; address tokenOut; uint24 fee; address recipient; uint256 deadline; uint256 amountOut; uint256 amountInMaximum; uint160 sqrtPriceLimitX96; }
        struct ExactOutputParams { bytes path; address recipient; uint256 deadline; uint256 amountOut; uint256 amountInMaximum; }

        function exactInputSingle(ExactInputSingleParams params) payable;
        function exactInput(ExactInputParams params) payable;
        function exactOutputSingle(ExactOutputSingleParams params) payable;
        function exactOutput(ExactOutputParams params) payable;
        function multicall(bytes[] data) payable;
        function unwrapWETH9(uint256 amountMinimum, address recipient) payable;
        function unwrapWETH9WithFee(uint256 amountMinimum, address recipient, uint256 feeBips, address feeRecipient) payable;
        function refundETH() payable;
        function sweepToken(address token, uint256 amountMinimum, address recipient) payable;
        function sweepTokenWithFee(address token, uint256 amountMinimum, address recipient, uint256 feeBips, address feeRecipient) payable;
        function selfPermit(address token, uint256 value, uint256 deadline, uint8 v, bytes32 r, bytes32 s) payable;
        function selfPermitIfNecessary(address token, uint256 value, uint256 deadline, uint8 v, bytes32 r, bytes32 s) payable;
        function selfPermitAllowed(address token, uint256 nonce, uint256 expiry, uint8 v, bytes32 r, bytes32 s) payable;
        function selfPermitAllowedIfNecessary(address token, uint256 nonce, uint256 expiry, uint8 v, bytes32 r, bytes32 s) payable;
    }
}

sol! {
    interface ISwapRouter02 {
        struct ExactInputSingleParams { address tokenIn; address tokenOut; uint24 fee; address recipient; uint256 amountIn; uint256 amountOutMinimum; uint160 sqrtPriceLimitX96; }
        struct ExactInputParams { bytes path; address recipient; uint256 amountIn; uint256 amountOutMinimum; }
        struct ExactOutputSingleParams { address tokenIn; address tokenOut; uint24 fee; address recipient; uint256 amountOut; uint256 amountInMaximum; uint160 sqrtPriceLimitX96; }
        struct ExactOutputParams { bytes path; address recipient; uint256 amountOut; uint256 amountInMaximum; }

        function exactInputSingle(ExactInputSingleParams params) payable;
        function exactInput(ExactInputParams params) payable;
        function exactOutputSingle(ExactOutputSingleParams params) payable;
        function exactOutput(ExactOutputParams params) payable;
        function swapExactTokensForTokens(uint256 amountIn, uint256 amountOutMin, address[] path, address to) payable;
        function swapTokensForExactTokens(uint256 amountOut, uint256 amountInMax, address[] path, address to) payable;
        function multicall(bytes[] data) payable;
        function multicall(uint256 deadline, bytes[] data) payable;
        function multicall(bytes32 previousBlockhash, bytes[] data) payable;
        function unwrapWETH9(uint256 amountMinimum, address recipient) payable;
        function unwrapWETH9(uint256 amountMinimum) payable;
        function unwrapWETH9WithFee(uint256 amountMinimum, address recipient, uint256 feeBips, address feeRecipient) payable;
        function unwrapWETH9WithFee(uint256 amountMinimum, uint256 feeBips, address feeRecipient) payable;
        function wrapETH(uint256 value) payable;
        function refundETH() payable;
        function sweepToken(address token, uint256 amountMinimum, address recipient) payable;
        function sweepToken(address token, uint256 amountMinimum) payable;
        function selfPermit(address token, uint256 value, uint256 deadline, uint8 v, bytes32 r, bytes32 s) payable;
        function selfPermitIfNecessary(address token, uint256 value, uint256 deadline, uint8 v, bytes32 r, bytes32 s) payable;
        function selfPermitAllowed(address token, uint256 nonce, uint256 expiry, uint8 v, bytes32 r, bytes32 s) payable;
        function selfPermitAllowedIfNecessary(address token, uint256 nonce, uint256 expiry, uint8 v, bytes32 r, bytes32 s) payable;
    }
}

use ISwapRouter::ISwapRouterCalls as V1;
use ISwapRouter02::ISwapRouter02Calls as V02;

/// SwapRouter02 recipient sentinels (`Constants.MSG_SENDER` / `Constants.ADDRESS_THIS`).
const MSG_SENDER: Address = address!("0000000000000000000000000000000000000001");
const ADDRESS_THIS: Address = address!("0000000000000000000000000000000000000002");

pub struct UniswapV3Decoder {
    router_v1: Address,
    router_02: Address,
}

#[derive(Default)]
struct Acc {
    legs: Vec<SwapLeg>,
    unwrap: Option<Unwrap>,
    sweeps: Vec<Sweep>,
}

impl Acc {
    fn sweep(&mut self, token: Address, recipient: Address, amount_min: U256) {
        self.sweeps.push(Sweep {
            token,
            recipient,
            amount_min,
        });
    }

    fn set_unwrap(&mut self, recipient: Address, amount_min: U256) -> Result<(), DecodeError> {
        let unwrap = Unwrap {
            recipient,
            amount_min,
        };
        if self.unwrap.replace(unwrap).is_some() {
            return Err(DecodeError::UnsupportedRoute("multiple WETH unwraps"));
        }
        Ok(())
    }
}

fn unsupported_selector(data: &[u8]) -> DecodeError {
    DecodeError::UnsupportedCommand(format!(
        "selector 0x{}",
        hex::encode(data.get(..4).unwrap_or(data))
    ))
}

fn invalid(e: alloy::sol_types::Error) -> DecodeError {
    DecodeError::InvalidCalldata(e.to_string())
}

impl UniswapV3Decoder {
    pub fn new(addrs: &ChainAddresses) -> Self {
        Self {
            router_v1: addrs.uniswap_v3_swap_router,
            router_02: addrs.uniswap_v3_swap_router02,
        }
    }

    fn visit_v1(&self, data: &[u8], acc: &mut Acc, nested: bool) -> Result<(), DecodeError> {
        if !selector(data).is_some_and(V1::valid_selector) {
            return Err(unsupported_selector(data));
        }
        // SwapRouter v1 treats recipient == address(0) as address(this).
        let to = |a: Address| if a.is_zero() { self.router_v1 } else { a };
        let (exact_in, exact_out) = (SwapKind::ExactIn, SwapKind::ExactOut);
        match V1::abi_decode(data).map_err(invalid)? {
            V1::exactInputSingle(c) => {
                let p = c.params;
                acc.legs.push(v3_single_leg(
                    p.tokenIn,
                    p.tokenOut,
                    p.fee.to(),
                    exact_in,
                    Some(p.amountIn),
                    p.amountOutMinimum,
                    to(p.recipient),
                ));
            }
            V1::exactInput(c) => {
                let p = c.params;
                acc.legs.push(v3_leg(
                    &p.path,
                    exact_in,
                    Some(p.amountIn),
                    p.amountOutMinimum,
                    to(p.recipient),
                )?);
            }
            V1::exactOutputSingle(c) => {
                let p = c.params;
                acc.legs.push(v3_single_leg(
                    p.tokenIn,
                    p.tokenOut,
                    p.fee.to(),
                    exact_out,
                    Some(p.amountInMaximum),
                    p.amountOut,
                    to(p.recipient),
                ));
            }
            V1::exactOutput(c) => {
                let p = c.params;
                acc.legs.push(v3_leg(
                    &p.path,
                    exact_out,
                    Some(p.amountInMaximum),
                    p.amountOut,
                    to(p.recipient),
                )?);
            }
            V1::multicall(c) if !nested => {
                for inner in &c.data {
                    self.visit_v1(inner, acc, true)?;
                }
            }
            V1::multicall(_) => {
                return Err(DecodeError::UnsupportedCommand("nested multicall".into()));
            }
            V1::unwrapWETH9(c) => acc.set_unwrap(c.recipient, c.amountMinimum)?,
            V1::unwrapWETH9WithFee(c) => acc.set_unwrap(c.recipient, c.amountMinimum)?,
            V1::sweepToken(c) => acc.sweep(c.token, c.recipient, c.amountMinimum),
            V1::sweepTokenWithFee(c) => acc.sweep(c.token, c.recipient, c.amountMinimum),
            V1::refundETH(_)
            | V1::selfPermit(_)
            | V1::selfPermitIfNecessary(_)
            | V1::selfPermitAllowed(_)
            | V1::selfPermitAllowedIfNecessary(_) => {}
        }
        Ok(())
    }

    fn visit_02(
        &self,
        data: &[u8],
        from: Address,
        acc: &mut Acc,
        nested: bool,
    ) -> Result<(), DecodeError> {
        if !selector(data).is_some_and(V02::valid_selector) {
            return Err(unsupported_selector(data));
        }
        let to = |a: Address| match a {
            MSG_SENDER => from,
            ADDRESS_THIS => self.router_02,
            other => other,
        };
        // SwapRouter02 `Constants.CONTRACT_BALANCE == 0`: spend the router's own balance.
        let amount = |a: U256| (!a.is_zero()).then_some(a);
        let (exact_in, exact_out) = (SwapKind::ExactIn, SwapKind::ExactOut);
        let visit_all = |calls: &[alloy::primitives::Bytes], acc: &mut Acc| {
            if nested {
                return Err(DecodeError::UnsupportedCommand("nested multicall".into()));
            }
            calls
                .iter()
                .try_for_each(|inner| self.visit_02(inner, from, acc, true))
        };
        match V02::abi_decode(data).map_err(invalid)? {
            V02::exactInputSingle(c) => {
                let p = c.params;
                acc.legs.push(v3_single_leg(
                    p.tokenIn,
                    p.tokenOut,
                    p.fee.to(),
                    exact_in,
                    amount(p.amountIn),
                    p.amountOutMinimum,
                    to(p.recipient),
                ));
            }
            V02::exactInput(c) => {
                let p = c.params;
                acc.legs.push(v3_leg(
                    &p.path,
                    exact_in,
                    amount(p.amountIn),
                    p.amountOutMinimum,
                    to(p.recipient),
                )?);
            }
            V02::exactOutputSingle(c) => {
                let p = c.params;
                acc.legs.push(v3_single_leg(
                    p.tokenIn,
                    p.tokenOut,
                    p.fee.to(),
                    exact_out,
                    Some(p.amountInMaximum),
                    p.amountOut,
                    to(p.recipient),
                ));
            }
            V02::exactOutput(c) => {
                let p = c.params;
                acc.legs.push(v3_leg(
                    &p.path,
                    exact_out,
                    Some(p.amountInMaximum),
                    p.amountOut,
                    to(p.recipient),
                )?);
            }
            V02::swapExactTokensForTokens(c) => {
                acc.legs.push(v2_leg(
                    c.path,
                    exact_in,
                    amount(c.amountIn),
                    c.amountOutMin,
                    to(c.to),
                )?);
            }
            V02::swapTokensForExactTokens(c) => {
                acc.legs.push(v2_leg(
                    c.path,
                    exact_out,
                    Some(c.amountInMax),
                    c.amountOut,
                    to(c.to),
                )?);
            }
            V02::multicall_0(c) => visit_all(&c.data, acc)?,
            V02::multicall_1(c) => visit_all(&c.data, acc)?,
            V02::multicall_2(c) => visit_all(&c.data, acc)?,
            V02::unwrapWETH9_0(c) => acc.set_unwrap(to(c.recipient), c.amountMinimum)?,
            V02::unwrapWETH9_1(c) => acc.set_unwrap(from, c.amountMinimum)?,
            V02::unwrapWETH9WithFee_0(c) => acc.set_unwrap(to(c.recipient), c.amountMinimum)?,
            V02::unwrapWETH9WithFee_1(c) => acc.set_unwrap(from, c.amountMinimum)?,
            V02::sweepToken_0(c) => acc.sweep(c.token, to(c.recipient), c.amountMinimum),
            V02::sweepToken_1(c) => acc.sweep(c.token, from, c.amountMinimum),
            V02::wrapETH(_)
            | V02::refundETH(_)
            | V02::selfPermit(_)
            | V02::selfPermitIfNecessary(_)
            | V02::selfPermitAllowed(_)
            | V02::selfPermitAllowedIfNecessary(_) => {}
        }
        Ok(())
    }
}

impl DexDecoder for UniswapV3Decoder {
    fn dex(&self) -> Dex {
        Dex::UniswapV3
    }

    fn can_decode(&self, tx: &Transaction) -> bool {
        let Some(sel) = selector(tx.input()) else {
            return false;
        };
        match tx.to() {
            Some(to) if to == self.router_v1 => V1::valid_selector(sel),
            Some(to) if to == self.router_02 => V02::valid_selector(sel),
            _ => false,
        }
    }

    fn decode(&self, tx: &Transaction) -> Result<Option<SwapPlan>, DecodeError> {
        let mut acc = Acc::default();
        let router = match tx.to() {
            Some(to) if to == self.router_v1 => {
                self.visit_v1(tx.input(), &mut acc, false)?;
                self.router_v1
            }
            Some(to) if to == self.router_02 => {
                self.visit_02(tx.input(), tx.from(), &mut acc, false)?;
                self.router_02
            }
            _ => return Ok(None),
        };
        if acc.legs.is_empty() {
            return Ok(None);
        }
        let value = tx.value();
        Ok(Some(SwapPlan {
            router,
            legs: acc.legs,
            wrapped_eth: (!value.is_zero()).then_some(value),
            unwrap: acc.unwrap,
            sweeps: acc.sweeps,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decoder::test_util::tx;
    use crate::utils::chain_addresses;
    use alloy::primitives::aliases::{U24, U160};
    use alloy::sol_types::SolCall;

    const WETH: Address = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
    const TOKEN: Address = address!("1111111111111111111111111111111111111111");
    const USER: Address = address!("2222222222222222222222222222222222222222");

    fn setup() -> (UniswapV3Decoder, ChainAddresses) {
        let addrs = chain_addresses(1).unwrap();
        (UniswapV3Decoder::new(&addrs), addrs)
    }

    #[test]
    fn v1_multicall_sell_with_unwrap() {
        let (d, a) = setup();
        let swap = ISwapRouter::exactInputSingleCall {
            params: ISwapRouter::ExactInputSingleParams {
                tokenIn: TOKEN,
                tokenOut: WETH,
                fee: U24::from(3000),
                recipient: Address::ZERO,
                deadline: U256::MAX,
                amountIn: U256::from(1000),
                amountOutMinimum: U256::from(9),
                sqrtPriceLimitX96: U160::ZERO,
            },
        }
        .abi_encode();
        let unwrap = ISwapRouter::unwrapWETH9Call {
            amountMinimum: U256::from(9),
            recipient: USER,
        }
        .abi_encode();
        let input = ISwapRouter::multicallCall {
            data: vec![swap.into(), unwrap.into()],
        }
        .abi_encode();
        let t = tx(USER, Some(a.uniswap_v3_swap_router), U256::ZERO, &input);
        assert!(d.can_decode(&t));
        let plan = d.decode(&t).unwrap().unwrap();
        assert_eq!(
            plan.unwrap,
            Some(Unwrap {
                recipient: USER,
                amount_min: U256::from(9)
            })
        );
        assert_eq!(plan.wrapped_eth, None);
        assert_eq!(plan.legs.len(), 1);
        assert_eq!(
            plan.legs[0].recipient, a.uniswap_v3_swap_router,
            "address(0) => router"
        );
        assert_eq!(plan.legs[0].route.fees, vec![3000]);
    }

    #[test]
    fn router02_multicall_deadline_buy_with_eth() {
        let (d, a) = setup();
        let mut path = WETH.to_vec();
        path.extend_from_slice(&[0x00, 0x01, 0xf4]); // 500
        path.extend_from_slice(TOKEN.as_slice());
        let swap = ISwapRouter02::exactInputCall {
            params: ISwapRouter02::ExactInputParams {
                path: path.into(),
                recipient: MSG_SENDER,
                amountIn: U256::from(77),
                amountOutMinimum: U256::from(1),
            },
        }
        .abi_encode();
        let input = ISwapRouter02::multicall_1Call {
            deadline: U256::MAX,
            data: vec![swap.into()],
        }
        .abi_encode();
        let t = tx(
            USER,
            Some(a.uniswap_v3_swap_router02),
            U256::from(77),
            &input,
        );
        let plan = d.decode(&t).unwrap().unwrap();
        assert_eq!(plan.wrapped_eth, Some(U256::from(77)));
        assert_eq!(
            plan.legs[0].recipient, USER,
            "MSG_SENDER sentinel => tx.from"
        );
        assert_eq!(plan.legs[0].route.tokens, vec![WETH, TOKEN]);
        assert_eq!(plan.legs[0].route.fees, vec![500]);
    }

    #[test]
    fn router02_contract_balance_sentinel_is_none() {
        let (d, a) = setup();
        let input = ISwapRouter02::swapExactTokensForTokensCall {
            amountIn: U256::ZERO,
            amountOutMin: U256::from(1),
            path: vec![TOKEN, WETH],
            to: ADDRESS_THIS,
        }
        .abi_encode();
        let plan = d
            .decode(&tx(
                USER,
                Some(a.uniswap_v3_swap_router02),
                U256::ZERO,
                &input,
            ))
            .unwrap()
            .unwrap();
        assert_eq!(plan.legs[0].amount_in, None);
        assert_eq!(plan.legs[0].recipient, a.uniswap_v3_swap_router02);
    }

    #[test]
    fn unknown_inner_call_is_reported() {
        let (d, a) = setup();
        let input = ISwapRouter::multicallCall {
            data: vec![vec![0xde, 0xad, 0xbe, 0xef].into()],
        }
        .abi_encode();
        let err = d
            .decode(&tx(
                USER,
                Some(a.uniswap_v3_swap_router),
                U256::ZERO,
                &input,
            ))
            .unwrap_err();
        assert_eq!(
            err,
            DecodeError::UnsupportedCommand("selector 0xdeadbeef".into())
        );
    }

    #[test]
    fn multicall_without_swap_is_not_a_trade() {
        let (d, a) = setup();
        let input = ISwapRouter::multicallCall {
            data: vec![ISwapRouter::refundETHCall {}.abi_encode().into()],
        }
        .abi_encode();
        assert_eq!(
            d.decode(&tx(
                USER,
                Some(a.uniswap_v3_swap_router),
                U256::ZERO,
                &input
            ))
            .unwrap(),
            None
        );
    }
}
