//! Uniswap V4 execution through the Universal Router (`V4_SWAP`), quotes via V4Quoter.
//! Also hosts the V4 ABI types shared with the decoder.

use alloy::primitives::aliases::{I24, U24};
use alloy::primitives::{Address, Bytes, U256};
use alloy::rpc::types::TransactionRequest;
use alloy::sol;
use alloy::sol_types::{SolCall, SolValue};

use super::{DexAdapter, DexError, SwapParams, check_route};
use crate::decoder::uniswap_v4::{
    ADDRESS_THIS, NATIVE, SETTLE, SETTLE_ALL, SWAP_EXACT_IN, TAKE, TAKE_ALL,
};
use crate::decoder::universal_router::{UNWRAP_WETH, V4_SWAP, WRAP_ETH};
use crate::models::{PoolKind, RouteLeg, TradeSide, V4Path};
use crate::utils::ChainAddresses;

sol! {
    struct PoolKey { address currency0; address currency1; uint24 fee; int24 tickSpacing; address hooks; }
    struct PathKey { address intermediateCurrency; uint24 fee; int24 tickSpacing; address hooks; bytes hookData; }
    struct ExactInputSingleParams { PoolKey poolKey; bool zeroForOne; uint128 amountIn; uint128 amountOutMinimum; bytes hookData; }
    struct ExactInputParams { address currencyIn; PathKey[] path; uint128 amountIn; uint128 amountOutMinimum; }
    struct ExactOutputSingleParams { PoolKey poolKey; bool zeroForOne; uint128 amountOut; uint128 amountInMaximum; bytes hookData; }
    struct ExactOutputParams { address currencyOut; PathKey[] path; uint128 amountOut; uint128 amountInMaximum; }
    struct QuoteExactParams { address exactCurrency; PathKey[] path; uint128 exactAmount; }

    interface IV4Quoter {
        function quoteExactInput(QuoteExactParams params) external returns (uint256 amountOut, uint256 gasEstimate);
    }
    interface IUniversalRouterExec {
        function execute(bytes commands, bytes[] inputs, uint256 deadline) external payable;
    }
}

/// Universal Router v2 for swaps (ERC20 input is pulled through Permit2), V4Quoter for quotes.
pub struct UniswapV4Adapter {
    router: Address,
    quoter: Address,
    permit2: Address,
    weth: Address,
}

impl UniswapV4Adapter {
    pub fn new(addrs: &ChainAddresses) -> Self {
        Self {
            router: addrs.universal_router_v4,
            quoter: addrs.uniswap_v4_quoter,
            permit2: addrs.permit2,
            weth: addrs.weth,
        }
    }
}

fn v4_path(route: &RouteLeg) -> Result<&V4Path, DexError> {
    route
        .v4
        .as_ref()
        .filter(|p| !p.hops.is_empty())
        .ok_or(DexError::InvalidRoute("not a V4 path"))
}

fn path_keys(path: &V4Path) -> Result<Vec<PathKey>, DexError> {
    path.hops
        .iter()
        .map(|h| {
            Ok(PathKey {
                intermediateCurrency: h.currency_out,
                fee: U24::try_from(h.fee).map_err(|_| DexError::InvalidRoute("V4 fee > uint24"))?,
                tickSpacing: I24::try_from(h.tick_spacing)
                    .map_err(|_| DexError::InvalidRoute("V4 tick spacing > int24"))?,
                hooks: h.hooks,
                hookData: h.hook_data.clone(),
            })
        })
        .collect()
}

fn u128_amount(v: U256) -> Result<u128, DexError> {
    u128::try_from(v).map_err(|_| DexError::InvalidRoute("amount exceeds uint128"))
}

impl DexAdapter for UniswapV4Adapter {
    fn pool(&self) -> PoolKind {
        PoolKind::UniswapV4
    }

    /// ERC20 approval target: Permit2 (which then authorises the router).
    fn spender(&self) -> Address {
        self.permit2
    }

    fn permit2_spender(&self) -> Option<Address> {
        Some(self.router)
    }

    fn quote_call(
        &self,
        route: &RouteLeg,
        amount_in: U256,
    ) -> Result<TransactionRequest, DexError> {
        let path = v4_path(route)?;
        let call = IV4Quoter::quoteExactInputCall {
            params: QuoteExactParams {
                exactCurrency: path.currency_in,
                path: path_keys(path)?,
                exactAmount: u128_amount(amount_in)?,
            },
        };
        Ok(TransactionRequest::default()
            .to(self.quoter)
            .input(call.abi_encode().into()))
    }

    fn decode_quote(&self, return_data: &[u8]) -> Result<U256, DexError> {
        IV4Quoter::quoteExactInputCall::abi_decode_returns(return_data)
            .map(|r| r.amountOut)
            .map_err(|e| DexError::BadQuote(e.to_string()))
    }

    /// Outputs are taken by `msg.sender` (`TAKE_ALL`), so `p.recipient` must be the sender.
    fn swap_tx(&self, route: &RouteLeg, p: &SwapParams) -> Result<TransactionRequest, DexError> {
        check_route(route, PoolKind::UniswapV4, p.side, self.weth)?;
        let path = v4_path(route)?;
        let currency_in = path.currency_in;
        let currency_out = path.hops[path.hops.len() - 1].currency_out;
        let swap = ExactInputParams {
            currencyIn: currency_in,
            path: path_keys(path)?,
            amountIn: u128_amount(p.amount_in)?,
            amountOutMinimum: u128_amount(p.min_out)?,
        }
        .abi_encode();

        let mut commands: Vec<u8> = Vec::new();
        let mut inputs: Vec<Bytes> = Vec::new();
        let mut actions: Vec<(u8, Vec<u8>)> = vec![(SWAP_EXACT_IN, swap)];
        let mut after: Option<(u8, Vec<u8>)> = None;
        let value = match p.side {
            TradeSide::Buy => {
                if currency_in == NATIVE {
                    // The router settles native ETH from msg.value.
                    actions.push((SETTLE_ALL, (NATIVE, p.amount_in).abi_encode_params()));
                } else if currency_in == self.weth {
                    commands.push(WRAP_ETH);
                    inputs.push((ADDRESS_THIS, p.amount_in).abi_encode_params().into());
                    // payerIsUser = false: pay the pool from the router's freshly wrapped WETH.
                    actions.push((SETTLE, (self.weth, p.amount_in, false).abi_encode_params()));
                } else {
                    return Err(DexError::InvalidRoute("V4 BUY must start from ETH/WETH"));
                }
                actions.push((TAKE_ALL, (currency_out, p.min_out).abi_encode_params()));
                Some(p.amount_in)
            }
            TradeSide::Sell => {
                // Token is pulled from the sender through Permit2.
                actions.push((SETTLE_ALL, (currency_in, p.amount_in).abi_encode_params()));
                if currency_out == NATIVE {
                    actions.push((TAKE_ALL, (NATIVE, p.min_out).abi_encode_params()));
                } else if currency_out == self.weth {
                    // Take WETH to the router (amount 0 = whole open delta), then unwrap to ETH.
                    actions.push((
                        TAKE,
                        (self.weth, ADDRESS_THIS, U256::ZERO).abi_encode_params(),
                    ));
                    after = Some((UNWRAP_WETH, (p.recipient, p.min_out).abi_encode_params()));
                } else {
                    return Err(DexError::InvalidRoute("V4 SELL must end in ETH/WETH"));
                }
                None
            }
        };
        let (codes, params): (Vec<u8>, Vec<Bytes>) = actions
            .into_iter()
            .map(|(a, p)| (a, Bytes::from(p)))
            .unzip();
        commands.push(V4_SWAP);
        inputs.push((Bytes::from(codes), params).abi_encode_params().into());
        if let Some((cmd, input)) = after {
            commands.push(cmd);
            inputs.push(input.into());
        }

        let input = IUniversalRouterExec::executeCall {
            commands: commands.into(),
            inputs,
            deadline: p.deadline,
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
    use crate::decoder::uniswap_v4::{Ctx, decode_v4_swap};
    use crate::decoder::{SwapPlan, Unwrap};
    use crate::models::V4Hop;
    use crate::models::test_util::{TOKEN, WETH};
    use crate::utils::chain_addresses;
    use alloy::primitives::address;
    use alloy::sol_types::{SolType, sol_data};

    const BOT: Address = address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266");

    fn route(currency_in: Address, currency_out: Address, side_tokens: [Address; 2]) -> RouteLeg {
        RouteLeg {
            pool: PoolKind::UniswapV4,
            tokens: side_tokens.to_vec(),
            fees: vec![500],
            v4: Some(V4Path {
                currency_in,
                hops: vec![V4Hop {
                    currency_out,
                    fee: 500,
                    tick_spacing: 10,
                    hooks: Address::ZERO,
                    hook_data: Bytes::new(),
                }],
            }),
        }
    }

    fn params(side: TradeSide) -> SwapParams {
        SwapParams {
            side,
            amount_in: U256::from(1_000),
            min_out: U256::from(900),
            recipient: BOT,
            deadline: U256::from(42),
        }
    }

    /// Round-trips the adapter's calldata through our own V4 decoder.
    fn decode(tx: &TransactionRequest) -> (Vec<u8>, SwapPlan) {
        let addrs = chain_addresses(1).unwrap();
        let call =
            IUniversalRouterExec::executeCall::abi_decode(tx.input.input().unwrap()).unwrap();
        assert_eq!(call.deadline, U256::from(42));
        let mut plan = SwapPlan {
            router: addrs.universal_router_v4,
            legs: vec![],
            wrapped_eth: None,
            unwrap: None,
            sweeps: vec![],
        };
        let ctx = Ctx {
            from: BOT,
            router: addrs.universal_router_v4,
            weth: addrs.weth,
            value: tx.value.unwrap_or_default(),
        };
        let v4 = call.commands.iter().position(|c| *c == V4_SWAP).unwrap();
        decode_v4_swap(&call.inputs[v4], &ctx, &mut plan).unwrap();
        (call.commands.to_vec(), plan)
    }

    #[test]
    fn buy_native_pool_pays_msg_value() {
        let a = UniswapV4Adapter::new(&chain_addresses(1).unwrap());
        let tx = a
            .swap_tx(
                &route(NATIVE, TOKEN, [WETH, TOKEN]),
                &params(TradeSide::Buy),
            )
            .unwrap();
        assert_eq!(tx.value, Some(U256::from(1_000)));
        let (cmds, plan) = decode(&tx);
        assert_eq!(cmds, vec![V4_SWAP]);
        assert_eq!(plan.legs[0].amount_in, Some(U256::from(1_000)));
        assert_eq!(plan.legs[0].amount_out, U256::from(900));
        assert_eq!(plan.sweeps[0].recipient, BOT);
        assert_eq!(plan.sweeps[0].amount_min, U256::from(900));
    }

    #[test]
    fn buy_weth_pool_wraps_first() {
        let a = UniswapV4Adapter::new(&chain_addresses(1).unwrap());
        let tx = a
            .swap_tx(&route(WETH, TOKEN, [WETH, TOKEN]), &params(TradeSide::Buy))
            .unwrap();
        let call =
            IUniversalRouterExec::executeCall::abi_decode(tx.input.input().unwrap()).unwrap();
        assert_eq!(call.commands.to_vec(), vec![WRAP_ETH, V4_SWAP]);
        let (recipient, amount) =
            <(sol_data::Address, sol_data::Uint<256>)>::abi_decode_params(&call.inputs[0]).unwrap();
        assert_eq!((recipient, amount), (ADDRESS_THIS, U256::from(1_000)));
    }

    #[test]
    fn sell_to_native_and_to_weth() {
        let a = UniswapV4Adapter::new(&chain_addresses(1).unwrap());
        let tx = a
            .swap_tx(
                &route(TOKEN, NATIVE, [TOKEN, WETH]),
                &params(TradeSide::Sell),
            )
            .unwrap();
        assert_eq!(tx.value, None);
        let (cmds, plan) = decode(&tx);
        assert_eq!(cmds, vec![V4_SWAP]);
        assert_eq!(
            plan.unwrap,
            Some(Unwrap {
                recipient: BOT,
                amount_min: U256::from(900)
            })
        );

        let tx = a
            .swap_tx(&route(TOKEN, WETH, [TOKEN, WETH]), &params(TradeSide::Sell))
            .unwrap();
        let call =
            IUniversalRouterExec::executeCall::abi_decode(tx.input.input().unwrap()).unwrap();
        assert_eq!(call.commands.to_vec(), vec![V4_SWAP, UNWRAP_WETH]);
        let (recipient, min) =
            <(sol_data::Address, sol_data::Uint<256>)>::abi_decode_params(&call.inputs[1]).unwrap();
        assert_eq!((recipient, min), (BOT, U256::from(900)));
    }

    #[test]
    fn approvals_go_through_permit2() {
        let addrs = chain_addresses(1).unwrap();
        let a = UniswapV4Adapter::new(&addrs);
        assert_eq!(a.spender(), addrs.permit2);
        assert_eq!(a.permit2_spender(), Some(addrs.universal_router_v4));
    }

    #[test]
    fn rejects_non_v4_or_wrong_direction() {
        let a = UniswapV4Adapter::new(&chain_addresses(1).unwrap());
        let mut r = route(NATIVE, TOKEN, [WETH, TOKEN]);
        assert!(a.swap_tx(&r, &params(TradeSide::Sell)).is_err());
        r.v4 = None;
        assert!(a.quote_call(&r, U256::from(1)).is_err());
    }
}
