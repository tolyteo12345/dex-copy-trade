use alloy::consensus::Transaction as _;
use alloy::rpc::types::Transaction;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxClass {
    Swap,
    Transfer,
    Approve,
    ContractCall,
    ContractDeploy,
    Unknown,
}

const ERC20_TRANSFER: [u8; 4] = [0xa9, 0x05, 0x9c, 0xbb];
const ERC20_TRANSFER_FROM: [u8; 4] = [0x23, 0xb8, 0x72, 0xdd];
const ERC20_APPROVE: [u8; 4] = [0x09, 0x5e, 0xa7, 0xb3];
const ERC20_INCREASE_ALLOWANCE: [u8; 4] = [0x39, 0x50, 0x93, 0x51];
const PERMIT2_APPROVE: [u8; 4] = [0x87, 0x51, 0x7c, 0x45];

/// `is_known_swap` = a registered `DexDecoder` claimed the tx.
pub fn classify(tx: &Transaction, is_known_swap: bool) -> TxClass {
    if tx.to().is_none() {
        return TxClass::ContractDeploy;
    }
    if is_known_swap {
        return TxClass::Swap;
    }
    let input = tx.input();
    if input.is_empty() {
        return if tx.value().is_zero() {
            TxClass::Unknown
        } else {
            TxClass::Transfer
        };
    }
    match selector(input) {
        Some(ERC20_TRANSFER | ERC20_TRANSFER_FROM) => TxClass::Transfer,
        Some(ERC20_APPROVE | ERC20_INCREASE_ALLOWANCE | PERMIT2_APPROVE) => TxClass::Approve,
        _ => TxClass::ContractCall,
    }
}

pub fn selector(input: &[u8]) -> Option<[u8; 4]> {
    input.get(..4)?.try_into().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eth::decoder::test_util::tx;
    use alloy::primitives::{Address, U256, address};

    const TO: Address = address!("1111111111111111111111111111111111111111");

    #[test]
    fn classifies_basic_kinds() {
        let from = address!("2222222222222222222222222222222222222222");
        let deploy = tx(from, None, U256::ZERO, &[0x60, 0x80]);
        assert_eq!(classify(&deploy, false), TxClass::ContractDeploy);

        let eth = tx(from, Some(TO), U256::from(1), &[]);
        assert_eq!(classify(&eth, false), TxClass::Transfer);

        let noop = tx(from, Some(TO), U256::ZERO, &[]);
        assert_eq!(classify(&noop, false), TxClass::Unknown);

        let erc20 = tx(from, Some(TO), U256::ZERO, &[0xa9, 0x05, 0x9c, 0xbb, 0, 0]);
        assert_eq!(classify(&erc20, false), TxClass::Transfer);

        let approve = tx(from, Some(TO), U256::ZERO, &[0x09, 0x5e, 0xa7, 0xb3, 0]);
        assert_eq!(classify(&approve, false), TxClass::Approve);

        let call = tx(from, Some(TO), U256::ZERO, &[0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(classify(&call, false), TxClass::ContractCall);
        assert_eq!(classify(&call, true), TxClass::Swap);
    }
}
