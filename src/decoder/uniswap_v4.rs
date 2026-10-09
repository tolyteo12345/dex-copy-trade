//! Uniswap V4 swaps inside a Universal Router `V4_SWAP` command.
//!
//! Input = `abi.encode(bytes actions, bytes[] params)`; each action byte selects how its
//! param is encoded (v4-periphery `Actions.sol`). Swaps create deltas on the PoolManager,
//! `SETTLE*` pays them and `TAKE*` withdraws outputs, so the recipient and real min-output
//! come from the take actions.

use alloy::primitives::{Address, Bytes, U256, address};
use alloy::sol_types::{SolType, sol_data};

use super::{DecodeError, SwapLeg, SwapPlan, Sweep, Unwrap};
use crate::models::{PoolKind, RouteLeg, SwapKind, V4Hop, V4Path};

pub use crate::dex::uniswap_v4::{
    ExactInputParams, ExactInputSingleParams, ExactOutputParams, ExactOutputSingleParams, PathKey,
    PoolKey,
};

pub const SWAP_EXACT_IN_SINGLE: u8 = 0x06;
pub const SWAP_EXACT_IN: u8 = 0x07;
pub const SWAP_EXACT_OUT_SINGLE: u8 = 0x08;
pub const SWAP_EXACT_OUT: u8 = 0x09;
pub const SETTLE: u8 = 0x0b;
pub const SETTLE_ALL: u8 = 0x0c;
const SETTLE_PAIR: u8 = 0x0d;
pub const TAKE: u8 = 0x0e;
pub const TAKE_ALL: u8 = 0x0f;
const TAKE_PORTION: u8 = 0x10;
const CLOSE_CURRENCY: u8 = 0x12;
const CLEAR_OR_TAKE: u8 = 0x13;
const SWEEP: u8 = 0x14;
const WRAP: u8 = 0x15;
const UNWRAP: u8 = 0x16;

/// `ActionConstants`: recipients and amount sentinels (same values as the Universal Router).
pub const MSG_SENDER: Address = address!("0000000000000000000000000000000000000001");
pub const ADDRESS_THIS: Address = address!("0000000000000000000000000000000000000002");
const OPEN_DELTA: u128 = 0;
const CONTRACT_BALANCE: U256 = U256::from_limbs([0, 0, 0, 0x8000_0000_0000_0000]);

/// Native ETH in V4 is the zero address.
pub const NATIVE: Address = Address::ZERO;

pub(crate) type ActionsInput = (sol_data::Bytes, sol_data::Array<sol_data::Bytes>);
pub(crate) type CurrencyAmount = (sol_data::Address, sol_data::Uint<256>);
pub(crate) type SettleInput = (sol_data::Address, sol_data::Uint<256>, sol_data::Bool);
pub(crate) type TakeInput = (sol_data::Address, sol_data::Address, sol_data::Uint<256>);

pub(crate) struct Ctx {
    pub from: Address,
    pub router: Address,
    pub weth: Address,
    pub value: U256,
}

fn bad(action: u8, e: alloy::sol_types::Error) -> DecodeError {
    DecodeError::InvalidCalldata(format!("V4 action 0x{action:02x}: {e}"))
}

fn norm(currency: Address, weth: Address) -> Address {
    if currency == NATIVE { weth } else { currency }
}

fn hop(
    currency_out: Address,
    fee: u32,
    tick_spacing: i32,
    hooks: Address,
    hook_data: Bytes,
) -> V4Hop {
    V4Hop {
        currency_out,
        fee,
        tick_spacing,
        hooks,
        hook_data,
    }
}

fn single(key: &PoolKey, zero_for_one: bool, hook_data: Bytes) -> V4Path {
    let (currency_in, currency_out) = if zero_for_one {
        (key.currency0, key.currency1)
    } else {
        (key.currency1, key.currency0)
    };
    V4Path {
        currency_in,
        hops: vec![hop(
            currency_out,
            key.fee.to(),
            key.tickSpacing.as_i32(),
            key.hooks,
            hook_data,
        )],
    }
}

/// Exact-input path: `path[i].intermediateCurrency` is the output of hop `i`.
fn forward(currency_in: Address, path: &[PathKey]) -> V4Path {
    V4Path {
        currency_in,
        hops: path
            .iter()
            .map(|p| {
                hop(
                    p.intermediateCurrency,
                    p.fee.to(),
                    p.tickSpacing.as_i32(),
                    p.hooks,
                    p.hookData.clone(),
                )
            })
            .collect(),
    }
}

/// Exact-output path is listed from the input side: `path[i].intermediateCurrency` is the
/// input of hop `i`, and hop `i`'s output is `path[i+1].intermediateCurrency` (or
/// `currency_out` for the last hop).
fn backward(currency_out: Address, path: &[PathKey]) -> Option<V4Path> {
    let first = path.first()?;
    let hops = path
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let out = path
                .get(i + 1)
                .map_or(currency_out, |n| n.intermediateCurrency);
            hop(
                out,
                p.fee.to(),
                p.tickSpacing.as_i32(),
                p.hooks,
                p.hookData.clone(),
            )
        })
        .collect();
    Some(V4Path {
        currency_in: first.intermediateCurrency,
        hops,
    })
}

pub(crate) fn v4_leg(
    path: V4Path,
    kind: SwapKind,
    amount_in: Option<U256>,
    amount_out: U256,
    weth: Address,
    router: Address,
) -> Result<SwapLeg, DecodeError> {
    if path.hops.is_empty() {
        return Err(DecodeError::InvalidCalldata("empty V4 path".into()));
    }
    let mut tokens = vec![norm(path.currency_in, weth)];
    tokens.extend(path.hops.iter().map(|h| norm(h.currency_out, weth)));
    Ok(SwapLeg {
        route: RouteLeg {
            pool: PoolKind::UniswapV4,
            tokens,
            fees: path.hops.iter().map(|h| h.fee).collect(),
            v4: Some(path),
        },
        kind,
        amount_in,
        amount_out,
        // Outputs stay as PoolManager deltas until a TAKE; treat them as held by the router.
        recipient: router,
    })
}

struct Take {
    currency: Address,
    recipient: Address,
    min: U256,
}

/// Decodes one `V4_SWAP` input, appending legs and payouts to `plan`.
pub(crate) fn decode_v4_swap(
    input: &[u8],
    ctx: &Ctx,
    plan: &mut SwapPlan,
) -> Result<(), DecodeError> {
    let (actions, params) = ActionsInput::abi_decode_params(input)
        .map_err(|e| DecodeError::InvalidCalldata(format!("V4_SWAP input: {e}")))?;
    if actions.len() != params.len() {
        return Err(DecodeError::InvalidCalldata(format!(
            "V4_SWAP: {} actions but {} params",
            actions.len(),
            params.len()
        )));
    }
    let to = |a: Address| match a {
        MSG_SENDER => ctx.from,
        ADDRESS_THIS => ctx.router,
        other => other,
    };
    let amount = |a: u128| (a != OPEN_DELTA).then(|| U256::from(a));
    let first_leg = plan.legs.len();
    let mut paths: Vec<V4Path> = Vec::new();
    let mut takes: Vec<Take> = Vec::new();
    let mut settled: Vec<(Address, U256)> = Vec::new();

    for (&action, p) in actions.iter().zip(params.iter()) {
        let (path, kind, amount_in, amount_out) = match action {
            SWAP_EXACT_IN_SINGLE => {
                let s = ExactInputSingleParams::abi_decode(p).map_err(|e| bad(action, e))?;
                let path = single(&s.poolKey, s.zeroForOne, s.hookData);
                (
                    path,
                    SwapKind::ExactIn,
                    amount(s.amountIn),
                    U256::from(s.amountOutMinimum),
                )
            }
            SWAP_EXACT_IN => {
                let s = ExactInputParams::abi_decode(p).map_err(|e| bad(action, e))?;
                let path = forward(s.currencyIn, &s.path);
                (
                    path,
                    SwapKind::ExactIn,
                    amount(s.amountIn),
                    U256::from(s.amountOutMinimum),
                )
            }
            SWAP_EXACT_OUT_SINGLE => {
                let s = ExactOutputSingleParams::abi_decode(p).map_err(|e| bad(action, e))?;
                let path = single(&s.poolKey, s.zeroForOne, s.hookData);
                (
                    path,
                    SwapKind::ExactOut,
                    Some(U256::from(s.amountInMaximum)),
                    U256::from(s.amountOut),
                )
            }
            SWAP_EXACT_OUT => {
                let s = ExactOutputParams::abi_decode(p).map_err(|e| bad(action, e))?;
                let path = backward(s.currencyOut, &s.path)
                    .ok_or_else(|| DecodeError::InvalidCalldata("empty V4 path".into()))?;
                (
                    path,
                    SwapKind::ExactOut,
                    Some(U256::from(s.amountInMaximum)),
                    U256::from(s.amountOut),
                )
            }
            SETTLE => {
                let (currency, amt, _payer_is_user) =
                    SettleInput::abi_decode_params(p).map_err(|e| bad(action, e))?;
                if !amt.is_zero() && amt != CONTRACT_BALANCE {
                    settled.push((currency, amt));
                }
                continue;
            }
            TAKE_ALL => {
                let (currency, min) =
                    CurrencyAmount::abi_decode_params(p).map_err(|e| bad(action, e))?;
                takes.push(Take {
                    currency,
                    recipient: ctx.from,
                    min,
                });
                continue;
            }
            TAKE => {
                let (currency, recipient, _amount) =
                    TakeInput::abi_decode_params(p).map_err(|e| bad(action, e))?;
                takes.push(Take {
                    currency,
                    recipient: to(recipient),
                    min: U256::ZERO,
                });
                continue;
            }
            // Payment caps, fees, refunds and router-balance housekeeping: not part of intent.
            SETTLE_ALL | SETTLE_PAIR | TAKE_PORTION | CLOSE_CURRENCY | CLEAR_OR_TAKE | SWEEP
            | WRAP | UNWRAP => continue,
            other => {
                return Err(DecodeError::UnsupportedCommand(format!(
                    "V4 action 0x{other:02x}"
                )));
            }
        };
        paths.push(path.clone());
        plan.legs.push(v4_leg(
            path, kind, amount_in, amount_out, ctx.weth, ctx.router,
        )?);
    }

    // OPEN_DELTA swaps spend what an earlier explicit SETTLE credited.
    for leg in &mut plan.legs[first_leg..] {
        let currency_in = leg.route.v4.as_ref().map(|p| p.currency_in);
        if leg.amount_in.is_none() {
            leg.amount_in = settled
                .iter()
                .find(|(c, _)| Some(*c) == currency_in)
                .map(|(_, amt)| *amt);
        }
    }
    // Native ETH paid in: the router settles it from msg.value.
    if paths.iter().any(|p| p.currency_in == NATIVE)
        && !ctx.value.is_zero()
        && plan.wrapped_eth.is_none()
    {
        plan.wrapped_eth = Some(ctx.value);
    }
    // Only takes of a swap *output* are payouts (others refund unused input).
    let outputs: Vec<Address> = paths
        .iter()
        .filter_map(|p| p.hops.last().map(|h| h.currency_out))
        .collect();
    for t in takes {
        if !outputs.contains(&t.currency) || t.recipient == ctx.router {
            continue;
        }
        if t.currency == NATIVE {
            let unwrap = Unwrap {
                recipient: t.recipient,
                amount_min: t.min,
            };
            if plan.unwrap.replace(unwrap).is_some() {
                return Err(DecodeError::UnsupportedRoute("multiple native ETH payouts"));
            }
        } else {
            plan.sweeps.push(Sweep {
                token: t.currency,
                recipient: t.recipient,
                amount_min: t.min,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use alloy::primitives::aliases::{I24, U24};
    use alloy::sol_types::SolValue;

    pub const WETH: Address = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
    pub const TOKEN: Address = address!("1111111111111111111111111111111111111111");
    pub const MID: Address = address!("3333333333333333333333333333333333333333");
    const USER: Address = address!("2222222222222222222222222222222222222222");
    const ROUTER: Address = address!("66a9893cC07D91D95644AEDD05D03f95e1dBA8Af");

    pub fn key(c0: Address, c1: Address) -> PoolKey {
        PoolKey {
            currency0: c0,
            currency1: c1,
            fee: U24::from(3000),
            tickSpacing: I24::try_from(60).unwrap(),
            hooks: Address::ZERO,
        }
    }

    pub fn actions(list: Vec<(u8, Vec<u8>)>) -> Vec<u8> {
        let (a, p): (Vec<u8>, Vec<Bytes>) = list.into_iter().map(|(a, p)| (a, p.into())).unzip();
        (Bytes::from(a), p).abi_encode_params()
    }

    fn ctx(value: u64) -> Ctx {
        Ctx {
            from: USER,
            router: ROUTER,
            weth: WETH,
            value: U256::from(value),
        }
    }

    fn empty_plan() -> SwapPlan {
        SwapPlan {
            router: ROUTER,
            legs: vec![],
            wrapped_eth: None,
            unwrap: None,
            sweeps: vec![],
        }
    }

    #[test]
    fn native_eth_buy_single() {
        let swap = ExactInputSingleParams {
            poolKey: key(NATIVE, TOKEN),
            zeroForOne: true,
            amountIn: 1_000,
            amountOutMinimum: 7,
            hookData: Bytes::new(),
        };
        let input = actions(vec![
            (SWAP_EXACT_IN_SINGLE, swap.abi_encode()),
            (SETTLE_ALL, (NATIVE, U256::from(1_000)).abi_encode_params()),
            (TAKE_ALL, (TOKEN, U256::from(9)).abi_encode_params()),
        ]);
        let mut plan = empty_plan();
        decode_v4_swap(&input, &ctx(1_000), &mut plan).unwrap();
        let leg = &plan.legs[0];
        assert_eq!(leg.route.pool, PoolKind::UniswapV4);
        assert_eq!(
            leg.route.tokens,
            vec![WETH, TOKEN],
            "native normalised to WETH"
        );
        let v4 = leg.route.v4.as_ref().unwrap();
        assert_eq!(v4.currency_in, NATIVE);
        assert_eq!((v4.hops[0].fee, v4.hops[0].tick_spacing), (3000, 60));
        assert_eq!(leg.amount_in, Some(U256::from(1_000)));
        assert_eq!(plan.wrapped_eth, Some(U256::from(1_000)));
        assert_eq!(
            plan.sweeps,
            vec![Sweep {
                token: TOKEN,
                recipient: USER,
                amount_min: U256::from(9)
            }]
        );
    }

    #[test]
    fn sell_to_native_with_open_delta_settle() {
        let swap = ExactInputSingleParams {
            poolKey: key(NATIVE, TOKEN),
            zeroForOne: false,
            amountIn: 0, // OPEN_DELTA: spend what SETTLE credited
            amountOutMinimum: 5,
            hookData: Bytes::new(),
        };
        let input = actions(vec![
            (SETTLE, (TOKEN, U256::from(500), true).abi_encode_params()),
            (SWAP_EXACT_IN_SINGLE, swap.abi_encode()),
            (TAKE_ALL, (NATIVE, U256::from(5)).abi_encode_params()),
        ]);
        let mut plan = empty_plan();
        decode_v4_swap(&input, &ctx(0), &mut plan).unwrap();
        assert_eq!(plan.legs[0].route.tokens, vec![TOKEN, WETH]);
        assert_eq!(plan.legs[0].amount_in, Some(U256::from(500)));
        assert_eq!(
            plan.unwrap,
            Some(Unwrap {
                recipient: USER,
                amount_min: U256::from(5)
            })
        );
        assert_eq!(plan.wrapped_eth, None);
    }

    #[test]
    fn multi_hop_exact_in_and_exact_out_paths() {
        let pk = |c: Address| PathKey {
            intermediateCurrency: c,
            fee: U24::from(500),
            tickSpacing: I24::try_from(10).unwrap(),
            hooks: Address::ZERO,
            hookData: Bytes::new(),
        };
        let exact_in = ExactInputParams {
            currencyIn: NATIVE,
            path: vec![pk(MID), pk(TOKEN)],
            amountIn: 100,
            amountOutMinimum: 1,
        };
        // Exact-out lists the path from the input side.
        let exact_out = ExactOutputParams {
            currencyOut: TOKEN,
            path: vec![pk(NATIVE), pk(MID)],
            amountOut: 50,
            amountInMaximum: 200,
        };
        for (action, p) in [
            (SWAP_EXACT_IN, exact_in.abi_encode()),
            (SWAP_EXACT_OUT, exact_out.abi_encode()),
        ] {
            let input = actions(vec![
                (action, p),
                (TAKE, (TOKEN, MSG_SENDER, U256::ZERO).abi_encode_params()),
            ]);
            let mut plan = empty_plan();
            decode_v4_swap(&input, &ctx(200), &mut plan).unwrap();
            let leg = &plan.legs[0];
            assert_eq!(
                leg.route.tokens,
                vec![WETH, MID, TOKEN],
                "action 0x{action:02x}"
            );
            let hops: Vec<_> = leg
                .route
                .v4
                .as_ref()
                .unwrap()
                .hops
                .iter()
                .map(|h| h.currency_out)
                .collect();
            assert_eq!(hops, vec![MID, TOKEN]);
            assert_eq!(plan.sweeps[0].recipient, USER, "TAKE to MSG_SENDER");
        }
    }

    #[test]
    fn refund_takes_are_not_payouts_and_unknown_actions_fail() {
        let swap = ExactOutputSingleParams {
            poolKey: key(NATIVE, TOKEN),
            zeroForOne: true,
            amountOut: 10,
            amountInMaximum: 100,
            hookData: Bytes::new(),
        };
        let input = actions(vec![
            (SWAP_EXACT_OUT_SINGLE, swap.abi_encode()),
            (SETTLE_ALL, (NATIVE, U256::from(100)).abi_encode_params()),
            (TAKE_ALL, (TOKEN, U256::from(10)).abi_encode_params()),
            (TAKE_ALL, (NATIVE, U256::ZERO).abi_encode_params()), // unused ETH refund
        ]);
        let mut plan = empty_plan();
        decode_v4_swap(&input, &ctx(100), &mut plan).unwrap();
        assert_eq!(plan.unwrap, None, "ETH refund is not a native payout");
        assert_eq!(plan.legs[0].kind, SwapKind::ExactOut);

        let mint = actions(vec![(0x02, vec![])]);
        assert!(matches!(
            decode_v4_swap(&mint, &ctx(0), &mut empty_plan()),
            Err(DecodeError::UnsupportedCommand(_))
        ));
    }
}
