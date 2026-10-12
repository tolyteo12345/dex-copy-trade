use alloy::primitives::{Address, address};

/// Public, chain-specific contract addresses (not user config, hence not in env).
#[derive(Debug, Clone)]
pub struct ChainAddresses {
    pub weth: Address,
    pub uniswap_v2_router: Address,
    pub uniswap_v3_swap_router: Address,
    pub uniswap_v3_swap_router02: Address,
    pub uniswap_v3_quoter_v2: Address,
    pub universal_routers: Vec<Address>,
    /// Universal Router with `V4_SWAP` support (also the bot's V4 execution router).
    pub universal_router_v4: Address,
    pub uniswap_v4_quoter: Address,
    pub permit2: Address,
    /// Chainlink ETH/USD aggregator (8 decimals).
    pub eth_usd_feed: Address,
    /// Multicall3 (`getEthBalance` for measuring native ETH received in simulations).
    pub multicall3: Address,
}

/// Only mainnet is mapped so far; other chains must be added here before use.
pub fn chain_addresses(chain_id: u64) -> Option<ChainAddresses> {
    match chain_id {
        1 => Some(ChainAddresses {
            weth: address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"),
            uniswap_v2_router: address!("7a250d5630B4cF539739dF2C5dAcb4c659F2488D"),
            uniswap_v3_swap_router: address!("E592427A0AEce92De3Edee1F18E0157C05861564"),
            uniswap_v3_swap_router02: address!("68b3465833fb72A70ecDF485E0e4C7bD8665Fc45"),
            uniswap_v3_quoter_v2: address!("61fFE014bA17989E743c5F6cB21bF9697530B21e"),
            universal_routers: vec![
                address!("Ef1c6E67703c7BD7107eed8303Fbe6EC2554BF6B"), // v1
                address!("3fC91A3afd70395Cd496C647d5a6CC9D4B2b7FAD"), // v1.2
                address!("66a9893cC07D91D95644AEDD05D03f95e1dBA8Af"), // v2 (V4-capable)
            ],
            universal_router_v4: address!("66a9893cC07D91D95644AEDD05D03f95e1dBA8Af"),
            uniswap_v4_quoter: address!("52F0E24D1c21C8A0cB1e5a5dD6198556BD9E1203"),
            permit2: address!("000000000022D473030F116dDEE9F6B43aC78BA3"),
            eth_usd_feed: address!("5f4eC3Df9cbd43714FE2740f5E3616155c5b8419"),
            multicall3: address!("cA11bde05977b3631167028862bE2a173976CA11"),
        }),
        _ => None,
    }
}
