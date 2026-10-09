use alloy::primitives::{Address, U256};
use alloy::rpc::types::TransactionRequest;
use alloy::sol_types::SolCall;

use alloy::primitives::aliases::{U48, U160};

use crate::ethereum::chain_state::{IERC20, IPermit2};

/// Approval txs needed before `spender` can pull `needed` tokens.
///
/// Approves `U256::MAX` on purpose: spenders are only the fixed, immutable Uniswap routers,
/// and an exact approval would add an approve tx (gas + latency) to every SELL. Tokens like
/// USDT reject changing a non-zero allowance, so a non-zero-but-short allowance is reset to
/// 0 first.
pub fn approval_txs(
    token: Address,
    spender: Address,
    current: U256,
    needed: U256,
) -> Vec<(&'static str, TransactionRequest)> {
    if current >= needed {
        return Vec::new();
    }
    let approve = |amount| {
        TransactionRequest::default()
            .to(token)
            .input(IERC20::approveCall { spender, amount }.abi_encode().into())
    };
    let mut txs = Vec::with_capacity(2);
    if !current.is_zero() {
        txs.push(("approve_reset", approve(U256::ZERO)));
    }
    txs.push(("approve", approve(U256::MAX)));
    txs
}

/// Permit2 sub-allowance for `spender` (the Universal Router). Same policy as ERC20: max
/// amount, max expiration, only for fixed Uniswap contracts. Re-approves when the amount is
/// short or the approval expires before `valid_until` (unix secs).
pub fn permit2_approval_txs(
    permit2: Address,
    token: Address,
    spender: Address,
    current: (U256, u64),
    needed: U256,
    valid_until: u64,
) -> Vec<(&'static str, TransactionRequest)> {
    let (amount, expiration) = current;
    if amount >= needed && expiration > valid_until {
        return Vec::new();
    }
    let call = IPermit2::approveCall {
        token,
        spender,
        amount: U160::MAX,
        expiration: U48::MAX,
    };
    vec![(
        "permit2_approve",
        TransactionRequest::default()
            .to(permit2)
            .input(call.abi_encode().into()),
    )]
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;

    const TOKEN: Address = address!("1111111111111111111111111111111111111111");
    const ROUTER: Address = address!("2222222222222222222222222222222222222222");

    fn amounts(txs: &[(&str, TransactionRequest)]) -> Vec<(String, U256)> {
        txs.iter()
            .map(|(step, tx)| {
                let call = IERC20::approveCall::abi_decode(tx.input.input().unwrap()).unwrap();
                assert_eq!(call.spender, ROUTER);
                assert_eq!(tx.to, Some(TOKEN.into()));
                (step.to_string(), call.amount)
            })
            .collect()
    }

    #[test]
    fn permit2_reapproves_when_short_or_expiring() {
        let p2 = Address::repeat_byte(0x22);
        let ok = permit2_approval_txs(
            p2,
            TOKEN,
            ROUTER,
            (U256::from(10), 1_000),
            U256::from(10),
            999,
        );
        assert!(ok.is_empty());
        let short = permit2_approval_txs(
            p2,
            TOKEN,
            ROUTER,
            (U256::from(9), 1_000),
            U256::from(10),
            999,
        );
        let expiring =
            permit2_approval_txs(p2, TOKEN, ROUTER, (U256::MAX, 999), U256::from(10), 999);
        for txs in [short, expiring] {
            assert_eq!(txs.len(), 1);
            assert_eq!(txs[0].0, "permit2_approve");
            assert_eq!(txs[0].1.to, Some(p2.into()));
            let call = IPermit2::approveCall::abi_decode(txs[0].1.input.input().unwrap()).unwrap();
            assert_eq!(
                (call.token, call.spender, call.amount),
                (TOKEN, ROUTER, U160::MAX)
            );
        }
    }

    #[test]
    fn enough_allowance_needs_nothing() {
        assert!(approval_txs(TOKEN, ROUTER, U256::from(10), U256::from(10)).is_empty());
    }

    #[test]
    fn zero_allowance_approves_max() {
        let txs = approval_txs(TOKEN, ROUTER, U256::ZERO, U256::from(10));
        assert_eq!(amounts(&txs), vec![("approve".into(), U256::MAX)]);
    }

    #[test]
    fn short_nonzero_allowance_is_reset_first() {
        let txs = approval_txs(TOKEN, ROUTER, U256::from(3), U256::from(10));
        assert_eq!(
            amounts(&txs),
            vec![
                ("approve_reset".into(), U256::ZERO),
                ("approve".into(), U256::MAX)
            ]
        );
    }
}
