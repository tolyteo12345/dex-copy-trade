//! Uniswap Universal Router `execute(commands, inputs[, deadline])`.
//! Each command byte selects how the matching `inputs[i]` is ABI-encoded.

use alloy::consensus::Transaction as _;
use alloy::network::TransactionResponse;
use alloy::primitives::{Address, U256, address};
use alloy::rpc::types::Transaction;
use alloy::sol;
use alloy::sol_types::{SolInterface, SolType, sol_data};

use super::classifier::selector;
use super::{DecodeError, DexDecoder, SwapPlan, Sweep, Unwrap, uniswap_v4, v2_leg, v3_leg};
use crate::eth::models::{Dex, SwapKind};
use crate::eth::utils::ChainAddresses;

sol! {
    interface IUniversalRouter {
        function execute(bytes commands, bytes[] inputs, uint256 deadline) payable;
        function execute(bytes commands, bytes[] inputs) payable;
    }
}

use IUniversalRouter::IUniversalRouterCalls as Call;

const COMMAND_TYPE_MASK: u8 = 0x3f;

const V3_SWAP_EXACT_IN: u8 = 0x00;
const V3_SWAP_EXACT_OUT: u8 = 0x01;
const PERMIT2_TRANSFER_FROM: u8 = 0x02;
const PERMIT2_PERMIT_BATCH: u8 = 0x03;
const SWEEP: u8 = 0x04;
const TRANSFER: u8 = 0x05;
const PAY_PORTION: u8 = 0x06;
const V2_SWAP_EXACT_IN: u8 = 0x08;
const V2_SWAP_EXACT_OUT: u8 = 0x09;
const PERMIT2_PERMIT: u8 = 0x0a;
pub const WRAP_ETH: u8 = 0x0b;
pub const UNWRAP_WETH: u8 = 0x0c;
const PERMIT2_TRANSFER_FROM_BATCH: u8 = 0x0d;
const BALANCE_CHECK_ERC20: u8 = 0x0e;
pub const V4_SWAP: u8 = 0x10;

const MSG_SENDER: Address = address!("0000000000000000000000000000000000000001");
const ADDRESS_THIS: Address = address!("0000000000000000000000000000000000000002");
/// `ActionConstants.CONTRACT_BALANCE`: spend the router's whole balance of the token.
const CONTRACT_BALANCE: U256 = U256::from_limbs([0, 0, 0, 0x8000_0000_0000_0000]);

// (recipient, amountIn|amountOut, amountOutMin|amountInMax, path, payerIsUser)
type V3SwapInput = (
    sol_data::Address,
    sol_data::Uint<256>,
    sol_data::Uint<256>,
    sol_data::Bytes,
    sol_data::Bool,
);
type V2SwapInput = (
    sol_data::Address,
    sol_data::Uint<256>,
    sol_data::Uint<256>,
    sol_data::Array<sol_data::Address>,
    sol_data::Bool,
);
// (recipient, amount)
type WrapInput = (sol_data::Address, sol_data::Uint<256>);
// (token, recipient, amountMin) — amountMin is uint160 on newer routers; same ABI word.
type SweepInput = (sol_data::Address, sol_data::Address, sol_data::Uint<256>);

pub struct UniversalRouterDecoder {
    routers: Vec<Address>,
    /// Only this router version understands `V4_SWAP` (0x10 means something else on v1.x).
    v4_router: Address,
    v4_enabled: bool,
    weth: Address,
}

impl UniversalRouterDecoder {
    pub fn new(addrs: &ChainAddresses, v4_enabled: bool) -> Self {
        Self {
            routers: addrs.universal_routers.clone(),
            v4_router: addrs.universal_router_v4,
            v4_enabled,
            weth: addrs.weth,
        }
    }
}

fn invalid(cmd: u8, e: alloy::sol_types::Error) -> DecodeError {
    DecodeError::InvalidCalldata(format!("UR command 0x{cmd:02x}: {e}"))
}

impl DexDecoder for UniversalRouterDecoder {
    fn dex(&self) -> Dex {
        Dex::UniswapUniversalRouter
    }

    fn can_decode(&self, tx: &Transaction) -> bool {
        tx.to().is_some_and(|to| self.routers.contains(&to))
            && selector(tx.input()).is_some_and(Call::valid_selector)
    }

    fn decode(&self, tx: &Transaction) -> Result<Option<SwapPlan>, DecodeError> {
        let Some(router) = tx.to() else {
            return Ok(None);
        };
        let (commands, inputs) = match Call::abi_decode(tx.input())
            .map_err(|e| DecodeError::InvalidCalldata(e.to_string()))?
        {
            Call::execute_0(c) => (c.commands, c.inputs),
            Call::execute_1(c) => (c.commands, c.inputs),
        };
        if commands.len() != inputs.len() {
            return Err(DecodeError::InvalidCalldata(format!(
                "{} commands but {} inputs",
                commands.len(),
                inputs.len()
            )));
        }

        let from = tx.from();
        let to = |a: Address| match a {
            MSG_SENDER => from,
            ADDRESS_THIS => router,
            other => other,
        };
        let amount = |a: U256| (a != CONTRACT_BALANCE).then_some(a);

        let mut plan = SwapPlan {
            router,
            legs: Vec::new(),
            wrapped_eth: None,
            unwrap: None,
            sweeps: Vec::new(),
        };
        for (&raw_cmd, input) in commands.iter().zip(inputs.iter()) {
            let cmd = raw_cmd & COMMAND_TYPE_MASK;
            match cmd {
                V3_SWAP_EXACT_IN | V3_SWAP_EXACT_OUT => {
                    let (recipient, a, b, path, _payer_is_user) =
                        V3SwapInput::abi_decode_params(input).map_err(|e| invalid(cmd, e))?;
                    plan.legs.push(if cmd == V3_SWAP_EXACT_IN {
                        v3_leg(&path, SwapKind::ExactIn, amount(a), b, to(recipient))?
                    } else {
                        v3_leg(&path, SwapKind::ExactOut, Some(b), a, to(recipient))?
                    });
                }
                V2_SWAP_EXACT_IN | V2_SWAP_EXACT_OUT => {
                    let (recipient, a, b, path, _payer_is_user) =
                        V2SwapInput::abi_decode_params(input).map_err(|e| invalid(cmd, e))?;
                    plan.legs.push(if cmd == V2_SWAP_EXACT_IN {
                        v2_leg(path, SwapKind::ExactIn, amount(a), b, to(recipient))?
                    } else {
                        v2_leg(path, SwapKind::ExactOut, Some(b), a, to(recipient))?
                    });
                }
                // A wrap after the swaps re-wraps leftover ETH as a refund, not an input.
                WRAP_ETH if !plan.legs.is_empty() => {}
                WRAP_ETH => {
                    let (_recipient, amount_min) =
                        WrapInput::abi_decode_params(input).map_err(|e| invalid(cmd, e))?;
                    let wrapped = amount(amount_min).unwrap_or(tx.value());
                    plan.wrapped_eth =
                        Some(plan.wrapped_eth.unwrap_or_default().saturating_add(wrapped));
                }
                UNWRAP_WETH => {
                    let (recipient, amount_min) =
                        WrapInput::abi_decode_params(input).map_err(|e| invalid(cmd, e))?;
                    // Unwrapping to the router itself converts input WETH to ETH (e.g. to pay
                    // a native-ETH V4 pool): not a payout.
                    if to(recipient) == router {
                        continue;
                    }
                    let unwrap = Unwrap {
                        recipient: to(recipient),
                        amount_min,
                    };
                    if plan.unwrap.replace(unwrap).is_some() {
                        return Err(DecodeError::UnsupportedRoute("multiple WETH unwraps"));
                    }
                }
                SWEEP => {
                    let (token, recipient, amount_min) =
                        SweepInput::abi_decode_params(input).map_err(|e| invalid(cmd, e))?;
                    plan.sweeps.push(Sweep {
                        token,
                        recipient: to(recipient),
                        amount_min,
                    });
                }
                PERMIT2_TRANSFER_FROM
                | PERMIT2_PERMIT_BATCH
                | TRANSFER
                | PAY_PORTION
                | PERMIT2_PERMIT
                | PERMIT2_TRANSFER_FROM_BATCH
                | BALANCE_CHECK_ERC20 => {}
                V4_SWAP if router == self.v4_router && self.v4_enabled => {
                    let ctx = uniswap_v4::Ctx {
                        from,
                        router,
                        weth: self.weth,
                        value: tx.value(),
                    };
                    uniswap_v4::decode_v4_swap(input, &ctx, &mut plan)?;
                }
                V4_SWAP if router == self.v4_router => {
                    return Err(DecodeError::UnsupportedCommand(
                        "UR command 0x10 (V4_SWAP; UNISWAP_V4 not in SUPPORTED_DEXES)".into(),
                    ));
                }
                other => {
                    return Err(DecodeError::UnsupportedCommand(format!(
                        "UR command 0x{other:02x}"
                    )));
                }
            }
        }
        Ok((!plan.legs.is_empty()).then_some(plan))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eth::decoder::test_util::tx;
    use crate::eth::utils::chain_addresses;
    use alloy::primitives::Bytes;
    use alloy::sol_types::{SolCall, SolValue};

    const WETH: Address = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
    const TOKEN: Address = address!("1111111111111111111111111111111111111111");
    const USER: Address = address!("2222222222222222222222222222222222222222");

    fn setup() -> (UniversalRouterDecoder, Address) {
        let addrs = chain_addresses(1).unwrap();
        let router = addrs.universal_routers[2];
        (UniversalRouterDecoder::new(&addrs, true), router)
    }

    fn execute(commands: &[u8], inputs: Vec<Vec<u8>>) -> Vec<u8> {
        IUniversalRouter::execute_0Call {
            commands: Bytes::copy_from_slice(commands),
            inputs: inputs.into_iter().map(Bytes::from).collect(),
            deadline: U256::MAX,
        }
        .abi_encode()
    }

    fn v3_path(a: Address, fee: u32, b: Address) -> Bytes {
        let mut p = a.to_vec();
        p.extend_from_slice(&fee.to_be_bytes()[1..]);
        p.extend_from_slice(b.as_slice());
        p.into()
    }

    #[test]
    fn wrap_then_v3_buy_with_contract_balance() {
        let (d, router) = setup();
        let wrap = (ADDRESS_THIS, CONTRACT_BALANCE).abi_encode_params();
        let swap = (
            MSG_SENDER,
            CONTRACT_BALANCE,
            U256::from(5),
            v3_path(WETH, 3000, TOKEN),
            false,
        )
            .abi_encode_params();
        let input = execute(&[WRAP_ETH, V3_SWAP_EXACT_IN], vec![wrap, swap]);
        let t = tx(USER, Some(router), U256::from(1000), &input);
        assert!(d.can_decode(&t));
        let plan = d.decode(&t).unwrap().unwrap();
        assert_eq!(plan.wrapped_eth, Some(U256::from(1000)));
        assert_eq!(plan.legs[0].amount_in, None);
        assert_eq!(plan.legs[0].recipient, USER);
    }

    #[test]
    fn v2_sell_then_unwrap() {
        let (d, router) = setup();
        let swap = (
            ADDRESS_THIS,
            U256::from(700),
            U256::ZERO,
            vec![TOKEN, WETH],
            true,
        )
            .abi_encode_params();
        let unwrap = (MSG_SENDER, U256::from(9)).abi_encode_params();
        // 0x80 flag (allow revert) must be masked off.
        let input = execute(
            &[V2_SWAP_EXACT_IN, PAY_PORTION | 0x80, UNWRAP_WETH],
            vec![swap, vec![], unwrap],
        );
        let plan = d
            .decode(&tx(USER, Some(router), U256::ZERO, &input))
            .unwrap()
            .unwrap();
        assert_eq!(plan.legs[0].route.tokens, vec![TOKEN, WETH]);
        assert_eq!(plan.legs[0].recipient, router);
        assert_eq!(
            plan.unwrap,
            Some(Unwrap {
                recipient: USER,
                amount_min: U256::from(9)
            })
        );
    }

    #[test]
    fn v4_disabled_or_old_router_and_unknown_commands_are_reported() {
        let (_, router) = setup();
        let addrs = chain_addresses(1).unwrap();
        let disabled = UniversalRouterDecoder::new(&addrs, false);
        let input = execute(&[V4_SWAP], vec![vec![]]);
        let err = disabled
            .decode(&tx(USER, Some(router), U256::ZERO, &input))
            .unwrap_err();
        assert!(matches!(err, DecodeError::UnsupportedCommand(ref s) if s.contains("UNISWAP_V4")));
        // 0x10 on UR v1.2 is not V4_SWAP.
        let old = addrs.universal_routers[1];
        assert!(matches!(
            disabled.decode(&tx(USER, Some(old), U256::ZERO, &input)),
            Err(DecodeError::UnsupportedCommand(_))
        ));
        let (d, router) = setup();

        let input = execute(&[0x13], vec![vec![]]);
        assert!(matches!(
            d.decode(&tx(USER, Some(router), U256::ZERO, &input)),
            Err(DecodeError::UnsupportedCommand(_))
        ));
    }

    #[test]
    fn mismatched_lengths_and_no_swaps() {
        let (d, router) = setup();
        let input = execute(&[WRAP_ETH, SWEEP], vec![vec![]]);
        assert!(matches!(
            d.decode(&tx(USER, Some(router), U256::ZERO, &input)),
            Err(DecodeError::InvalidCalldata(_))
        ));

        let sweep = (TOKEN, MSG_SENDER, U256::from(1)).abi_encode_params();
        let sweep_only = execute(&[SWEEP], vec![sweep]);
        assert_eq!(
            d.decode(&tx(USER, Some(router), U256::ZERO, &sweep_only))
                .unwrap(),
            None
        );
    }
}
