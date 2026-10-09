//! Live mainnet checks for `RpcChainReader`. Needs network access:
//! `ETH_HTTP_URL=https://... cargo test --test rpc_chain_reader -- --ignored`

use std::time::{SystemTime, UNIX_EPOCH};

use alloy::eips::BlockId;
use alloy::primitives::{I256, U256, address};
use alloy::providers::Provider;
use dex_copy_trade::ethereum::chain_state::{ChainReader, RpcChainReader};
use dex_copy_trade::ethereum::provider;
use dex_copy_trade::utils::chain_addresses;

fn reader() -> (RpcChainReader, alloy::providers::DynProvider) {
    let url = std::env::var("ETH_HTTP_URL").expect("set ETH_HTTP_URL to a mainnet RPC");
    let http = provider::connect_http(&url).unwrap();
    let addrs = chain_addresses(1).unwrap();
    (RpcChainReader::new(http.clone(), addrs.eth_usd_feed), http)
}

#[tokio::test]
#[ignore]
async fn reads_mainnet_state() {
    let (r, http) = reader();
    let weth = chain_addresses(1).unwrap().weth;

    assert!(r.gas_price().await.unwrap() > 0);
    assert!(r.is_contract(weth).await.unwrap());
    // Vitalik's EOA carries an EIP-7702 delegation (0xef0100 code): still not a contract.
    let eoa = address!("d8dA6BF26964aF9D7eEd9e03E53415D37aA96045");
    assert!(!r.is_contract(eoa).await.unwrap());
    let plain_eoa = address!("000000000000000000000000000000000000dEaD");
    assert!(!r.is_contract(plain_eoa).await.unwrap());

    // The WETH contract holds the ETH backing its supply.
    let head = http.get_block_number().await.unwrap();
    let at = BlockId::number(head - 1);
    assert!(r.eth_balance(weth, at).await.unwrap() > U256::ZERO);
    let _ = r.token_balance(weth, eoa, at).await.unwrap();

    // balanceOf on an EOA returns no data -> decode error, not a silent zero.
    assert!(r.token_balance(eoa, weth, at).await.is_err());

    let price = r.eth_usd().await.unwrap();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    assert!(price.answer > I256::ZERO);
    assert!(now - price.updated_at < 2 * 3600, "feed updated within 2h");
}
