pub mod classifier;
pub mod uniswap_v2;
pub mod uniswap_v3;
pub mod uniswap_v4;
pub mod universal_router;

use std::time::Instant;

use alloy::consensus::Transaction as _;
use alloy::network::TransactionResponse;
use alloy::primitives::utils::format_ether;
use alloy::primitives::{Address, U256};
use alloy::rpc::types::Transaction;
use tokio::sync::mpsc;

use crate::models::{Dex, PoolKind, RouteLeg, SourceCall, SwapKind, TradeIntent, TradeSide};
use crate::utils::ChainAddresses;
use crate::watcher::WatchedTx;
use classifier::TxClass;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DecodeError {
    #[error("malformed calldata: {0}")]
    InvalidCalldata(String),
    #[error("unsupported call/command: {0}")]
    UnsupportedCommand(String),
    #[error("unsupported route: {0}")]
    UnsupportedRoute(&'static str),
}

/// Extension point for new DEXes: implement this and register it in `DecoderRegistry::new`.
pub trait DexDecoder: Send + Sync {
    fn dex(&self) -> Dex;
    /// Cheap check (router address + selector) — no ABI decoding.
    fn can_decode(&self, tx: &Transaction) -> bool;
    /// `Ok(None)`: a recognised router call that contains no swap (e.g. only a sweep).
    fn decode(&self, tx: &Transaction) -> Result<Option<SwapPlan>, DecodeError>;
}

/// Router-level view of a swap tx, before ETH/WETH normalisation into a `TradeIntent`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwapPlan {
    pub router: Address,
    pub legs: Vec<SwapLeg>,
    /// Native ETH paid in and wrapped by the router.
    pub wrapped_eth: Option<U256>,
    /// Router-side WETH unwrap: the source receives native ETH.
    pub unwrap: Option<Unwrap>,
    /// Router-side token payouts (e.g. after the interface takes a fee on the output).
    pub sweeps: Vec<Sweep>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unwrap {
    pub recipient: Address,
    /// Often the real slippage bound when the swap leg itself uses min-out 0.
    pub amount_min: U256,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sweep {
    pub token: Address,
    pub recipient: Address,
    pub amount_min: U256,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwapLeg {
    pub route: RouteLeg,
    pub kind: SwapKind,
    /// ExactIn: amount in; ExactOut: max in. `None` = router-balance sentinel.
    pub amount_in: Option<U256>,
    /// ExactIn: min out; ExactOut: exact out.
    pub amount_out: U256,
    /// Already resolved from router sentinels (msg.sender / address(this)).
    pub recipient: Address,
}

impl SwapLeg {
    fn token_in(&self) -> Address {
        self.route.tokens[0]
    }
    fn token_out(&self) -> Address {
        self.route.tokens[self.route.tokens.len() - 1]
    }
}

pub(crate) fn v2_leg(
    path: Vec<Address>,
    kind: SwapKind,
    amount_in: Option<U256>,
    amount_out: U256,
    recipient: Address,
) -> Result<SwapLeg, DecodeError> {
    if path.len() < 2 {
        return Err(DecodeError::InvalidCalldata(
            "V2 path shorter than 2".into(),
        ));
    }
    Ok(SwapLeg {
        route: RouteLeg {
            pool: PoolKind::UniswapV2,
            tokens: path,
            fees: Vec::new(),
            v4: None,
        },
        kind,
        amount_in,
        amount_out,
        recipient,
    })
}

/// Encoded V3 path: `token (20) | fee (3) | token (20) | ...`. Exact-output paths are
/// encoded reversed (output token first); returned legs are always in swap direction.
pub(crate) fn v3_leg(
    path: &[u8],
    kind: SwapKind,
    amount_in: Option<U256>,
    amount_out: U256,
    recipient: Address,
) -> Result<SwapLeg, DecodeError> {
    let (mut tokens, mut fees) = decode_v3_path(path)?;
    if kind == SwapKind::ExactOut {
        tokens.reverse();
        fees.reverse();
    }
    Ok(SwapLeg {
        route: RouteLeg {
            pool: PoolKind::UniswapV3,
            tokens,
            fees,
            v4: None,
        },
        kind,
        amount_in,
        amount_out,
        recipient,
    })
}

pub(crate) fn v3_single_leg(
    token_in: Address,
    token_out: Address,
    fee: u32,
    kind: SwapKind,
    amount_in: Option<U256>,
    amount_out: U256,
    recipient: Address,
) -> SwapLeg {
    SwapLeg {
        route: RouteLeg {
            pool: PoolKind::UniswapV3,
            tokens: vec![token_in, token_out],
            fees: vec![fee],
            v4: None,
        },
        kind,
        amount_in,
        amount_out,
        recipient,
    }
}

fn decode_v3_path(path: &[u8]) -> Result<(Vec<Address>, Vec<u32>), DecodeError> {
    const ADDR: usize = 20;
    const HOP: usize = 23;
    if path.len() < ADDR + HOP || (path.len() - ADDR) % HOP != 0 {
        return Err(DecodeError::InvalidCalldata(format!(
            "V3 path length {} is not 20 + 23*n",
            path.len()
        )));
    }
    let mut tokens = vec![Address::from_slice(&path[..ADDR])];
    let mut fees = Vec::new();
    for hop in path[ADDR..].chunks_exact(HOP) {
        fees.push(u32::from_be_bytes([0, hop[0], hop[1], hop[2]]));
        tokens.push(Address::from_slice(&hop[3..]));
    }
    Ok((tokens, fees))
}

/// Normalises a router-level plan into a `TradeIntent`.
///
/// Handles split routes (several legs, same in/out token: amounts summed) and chained
/// routes (leg output sent to the router and consumed by a later leg, e.g. a V3 hop then
/// a V2 hop in one Universal Router tx). Anything else is rejected as unsupported.
pub fn assemble_intent(
    plan: &SwapPlan,
    tx: &Transaction,
    dex: Dex,
    weth: Address,
    detected_at: Instant,
) -> Result<TradeIntent, DecodeError> {
    use DecodeError::UnsupportedRoute;
    let legs = &plan.legs;
    let first = legs.first().ok_or(UnsupportedRoute("no swap legs"))?;
    let kind = first.kind;
    if legs.iter().any(|l| l.kind != kind) {
        return Err(UnsupportedRoute("mixed exact-in and exact-out legs"));
    }

    let feeds = |from: usize, to: usize| {
        from < to
            && legs[from].recipient == plan.router
            && legs[from].token_out() == legs[to].token_in()
    };
    let n = legs.len();
    let entries: Vec<&SwapLeg> = (0..n)
        .filter(|&i| !(0..i).any(|j| feeds(j, i)))
        .map(|i| &legs[i])
        .collect();
    let exits: Vec<&SwapLeg> = (0..n)
        .filter(|&j| !(j + 1..n).any(|i| feeds(j, i)))
        .map(|j| &legs[j])
        .collect();

    let token_in = unique(entries.iter().map(|l| l.token_in()))
        .ok_or(UnsupportedRoute("legs spend different input tokens"))?;
    let token_out = unique(exits.iter().map(|l| l.token_out()))
        .ok_or(UnsupportedRoute("legs produce different output tokens"))?;

    if plan.wrapped_eth.is_some() && token_in != weth {
        return Err(UnsupportedRoute("ETH sent but input token is not WETH"));
    }
    // An unwrap when the output isn't WETH returns leftover input ETH (exact-out refund).
    let unwrap = plan.unwrap.filter(|_| token_out == weth);
    let side = TradeSide::classify(token_in, token_out, weth)
        .ok_or(UnsupportedRoute("token-to-token swap (no ETH/WETH side)"))?;

    let amount_in = sum_inputs(&entries, plan.wrapped_eth)?;
    if amount_in.is_zero() {
        return Err(DecodeError::InvalidCalldata("zero input amount".into()));
    }
    let legs_out = exits
        .iter()
        .try_fold(U256::ZERO, |acc, l| acc.checked_add(l.amount_out))
        .ok_or_else(|| DecodeError::InvalidCalldata("output amount overflow".into()))?;
    // When the router holds the output, the final unwrap/sweep carries the real
    // recipient and slippage bound.
    let payout = unwrap.map(|u| (u.recipient, u.amount_min)).or_else(|| {
        plan.sweeps
            .iter()
            .find(|s| s.token == token_out)
            .map(|s| (s.recipient, s.amount_min))
    });
    let amount_out = legs_out.max(payout.map_or(U256::ZERO, |(_, min)| min));

    Ok(TradeIntent {
        source_wallet: tx.from(),
        source_tx_hash: tx.tx_hash(),
        source_block: tx.block_number,
        dex,
        router: plan.router,
        side,
        token_in,
        token_out,
        native_in: plan.wrapped_eth.is_some(),
        native_out: unwrap.is_some(),
        swap_kind: kind,
        amount_in,
        min_amount_out: (!amount_out.is_zero()).then_some(amount_out),
        recipient: payout.map_or(exits[0].recipient, |(to, _)| to),
        route: legs.iter().map(|l| l.route.clone()).collect(),
        source_call: SourceCall {
            to: plan.router,
            value: tx.value(),
            input: tx.input().clone(),
            gas_limit: tx.gas_limit(),
            nonce: tx.nonce(),
        },
        detected_at,
        decoded_at: Instant::now(),
    })
}

/// Sums entry-leg inputs; one router-balance sentinel is resolved from the wrapped ETH.
fn sum_inputs(entries: &[&SwapLeg], wrapped_eth: Option<U256>) -> Result<U256, DecodeError> {
    let overflow = || DecodeError::InvalidCalldata("input amount overflow".into());
    let known = entries
        .iter()
        .filter_map(|l| l.amount_in)
        .try_fold(U256::ZERO, |acc, a| acc.checked_add(a))
        .ok_or_else(overflow)?;
    match entries.iter().filter(|l| l.amount_in.is_none()).count() {
        0 => Ok(known),
        1 => {
            let wrapped = wrapped_eth.ok_or(DecodeError::UnsupportedRoute(
                "input amount is the router's balance (not derivable from calldata)",
            ))?;
            // The sentinel leg spends whatever wrapped ETH the explicit legs left over,
            // so the total input is the wrapped amount.
            Ok(wrapped.max(known))
        }
        _ => Err(DecodeError::UnsupportedRoute(
            "several legs spend the router's balance",
        )),
    }
}

fn unique(mut it: impl Iterator<Item = Address>) -> Option<Address> {
    let first = it.next()?;
    it.all(|a| a == first).then_some(first)
}

#[derive(Debug)]
pub enum DecodeOutcome {
    Trade(Box<TradeIntent>),
    NotSwap(TxClass),
    Ignored { dex: Dex, reason: DecodeError },
}

pub struct DecoderRegistry {
    decoders: Vec<Box<dyn DexDecoder>>,
    weth: Address,
}

impl DecoderRegistry {
    /// Registers only decoders whose DEX is enabled in `SUPPORTED_DEXES`.
    pub fn new(addrs: &ChainAddresses, supported: &[Dex]) -> Self {
        let all: Vec<Box<dyn DexDecoder>> = vec![
            Box::new(uniswap_v2::UniswapV2Decoder::new(addrs)),
            Box::new(uniswap_v3::UniswapV3Decoder::new(addrs)),
            Box::new(universal_router::UniversalRouterDecoder::new(
                addrs,
                supported.contains(&Dex::UniswapV4),
            )),
        ];
        Self {
            decoders: all
                .into_iter()
                .filter(|d| supported.contains(&d.dex()))
                .collect(),
            weth: addrs.weth,
        }
    }

    pub fn process(&self, item: &WatchedTx) -> DecodeOutcome {
        let tx = &item.tx;
        let decoder = self.decoders.iter().find(|d| d.can_decode(tx));
        let class = classifier::classify(tx, decoder.is_some());
        let Some(decoder) = decoder.filter(|_| class == TxClass::Swap) else {
            return DecodeOutcome::NotSwap(class);
        };
        let dex = decoder.dex();
        let plan = match decoder.decode(tx) {
            Ok(Some(plan)) => plan,
            Ok(None) => return DecodeOutcome::NotSwap(TxClass::ContractCall),
            Err(reason) => return DecodeOutcome::Ignored { dex, reason },
        };
        match assemble_intent(&plan, tx, dex, self.weth, item.detected_at) {
            Ok(intent) => DecodeOutcome::Trade(Box::new(intent)),
            Err(reason) => DecodeOutcome::Ignored { dex, reason },
        }
    }
}

/// Decoding is microseconds of CPU per tx, so a single task keeps up with any realistic
/// watched-wallet volume; shard by wallet if that ever stops being true.
pub async fn run_stage(
    registry: DecoderRegistry,
    mut rx: mpsc::Receiver<WatchedTx>,
    out: mpsc::Sender<TradeIntent>,
) {
    while let Some(item) = rx.recv().await {
        let source_tx = item.tx.tx_hash();
        let source_wallet = item.tx.from();
        match registry.process(&item) {
            DecodeOutcome::Trade(intent) => {
                tracing::info!(
                    event = "trade_decoded",
                    %source_wallet,
                    %source_tx,
                    dex = %intent.dex,
                    side = ?intent.side,
                    token_in = %intent.token_in,
                    token_out = %intent.token_out,
                    native_in = intent.native_in,
                    native_out = intent.native_out,
                    swap_kind = ?intent.swap_kind,
                    amount_in = %intent.amount_in,
                    amount_in_eth = (intent.side == TradeSide::Buy).then(|| format_ether(intent.amount_in)),
                    min_amount_out = ?intent.min_amount_out,
                    legs = intent.route.len(),
                    detection_to_decode_ms = ms(intent.decoded_at - intent.detected_at),
                );
                if out.send(*intent).await.is_err() {
                    break;
                }
            }
            DecodeOutcome::Ignored { dex, reason } => tracing::info!(
                event = "trade_ignored",
                %source_wallet,
                %source_tx,
                %dex,
                reason = %reason,
            ),
            DecodeOutcome::NotSwap(class) => tracing::info!(
                event = "tx_ignored",
                %source_wallet,
                %source_tx,
                class = ?class,
            ),
        }
    }
}

fn ms(d: std::time::Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

#[cfg(test)]
pub(crate) mod test_util {
    use alloy::primitives::{Address, U256};
    use alloy::rpc::types::Transaction;

    /// Builds an RPC transaction the same way fixtures are loaded (JSON), so tests
    /// exercise the real deserialisation path.
    pub fn tx(from: Address, to: Option<Address>, value: U256, input: &[u8]) -> Transaction {
        let json = serde_json::json!({
            "type": "0x2",
            "chainId": "0x1",
            "nonce": "0x0",
            "gas": "0x7a120",
            "maxFeePerGas": "0x3b9aca00",
            "maxPriorityFeePerGas": "0x1",
            "to": to,
            "value": format!("{value:#x}"),
            "input": format!("0x{}", alloy::primitives::hex::encode(input)),
            "accessList": [],
            "r": "0x1",
            "s": "0x1",
            "yParity": "0x0",
            "v": "0x0",
            "hash": format!("{:#x}", alloy::primitives::keccak256(input)),
            "from": from,
            "blockHash": null,
            "blockNumber": null,
            "transactionIndex": null
        });
        serde_json::from_value(json).expect("valid test tx json")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;

    const WETH: Address = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
    const USDC: Address = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
    const TOKEN: Address = address!("1111111111111111111111111111111111111111");
    const ROUTER: Address = address!("9999999999999999999999999999999999999999");
    const USER: Address = address!("2222222222222222222222222222222222222222");

    fn leg(tokens: &[Address], amount_in: Option<u64>, amount_out: u64, to: Address) -> SwapLeg {
        v2_leg(
            tokens.to_vec(),
            SwapKind::ExactIn,
            amount_in.map(U256::from),
            U256::from(amount_out),
            to,
        )
        .unwrap()
    }

    fn plan(legs: Vec<SwapLeg>, wrapped: Option<u64>, unwrap: Option<(Address, u64)>) -> SwapPlan {
        SwapPlan {
            router: ROUTER,
            legs,
            wrapped_eth: wrapped.map(U256::from),
            unwrap: unwrap.map(|(recipient, min)| Unwrap {
                recipient,
                amount_min: U256::from(min),
            }),
            sweeps: Vec::new(),
        }
    }

    fn assemble(p: &SwapPlan) -> Result<TradeIntent, DecodeError> {
        let tx = test_util::tx(USER, Some(ROUTER), U256::ZERO, &[1, 2, 3, 4]);
        assemble_intent(p, &tx, Dex::UniswapV2, WETH, Instant::now())
    }

    #[test]
    fn v3_path_roundtrip_and_validation() {
        let mut path = WETH.to_vec();
        path.extend_from_slice(&[0x00, 0x0b, 0xb8]); // 3000
        path.extend_from_slice(TOKEN.as_slice());
        let (tokens, fees) = decode_v3_path(&path).unwrap();
        assert_eq!(tokens, vec![WETH, TOKEN]);
        assert_eq!(fees, vec![3000]);

        let out = v3_leg(&path, SwapKind::ExactOut, None, U256::ZERO, USER).unwrap();
        assert_eq!(
            out.route.tokens,
            vec![TOKEN, WETH],
            "exact-out path is reversed"
        );

        assert!(decode_v3_path(&path[..30]).is_err());
        assert!(decode_v3_path(WETH.as_slice()).is_err());
    }

    #[test]
    fn single_leg_buy_with_native_eth() {
        let p = plan(
            vec![leg(&[WETH, TOKEN], Some(100), 7, USER)],
            Some(100),
            None,
        );
        let i = assemble(&p).unwrap();
        assert_eq!(i.side, TradeSide::Buy);
        assert_eq!((i.token_in, i.token_out), (WETH, TOKEN));
        assert_eq!(i.token(), TOKEN);
        assert!(i.native_in && !i.native_out);
        assert_eq!(i.amount_in, U256::from(100));
        assert_eq!(i.min_amount_out, Some(U256::from(7)));
        assert_eq!(i.recipient, USER);
        assert_eq!(i.source_wallet, USER);
    }

    #[test]
    fn sell_to_native_eth_uses_unwrap_recipient() {
        let p = plan(
            vec![leg(&[TOKEN, WETH], Some(500), 0, ROUTER)],
            None,
            Some((USER, 0)),
        );
        let i = assemble(&p).unwrap();
        assert_eq!(i.side, TradeSide::Sell);
        assert!(i.native_out);
        assert_eq!(i.recipient, USER);
        assert_eq!(i.min_amount_out, None, "zero min-out means no bound");

        let p = plan(
            vec![leg(&[TOKEN, WETH], Some(500), 0, ROUTER)],
            None,
            Some((USER, 33)),
        );
        assert_eq!(
            assemble(&p).unwrap().min_amount_out,
            Some(U256::from(33)),
            "unwrap minimum is the effective bound"
        );
    }

    #[test]
    fn sweep_of_output_token_sets_recipient_and_bound() {
        let mut p = plan(
            vec![leg(&[WETH, TOKEN], Some(100), 0, ROUTER)],
            Some(100),
            None,
        );
        p.sweeps = vec![
            Sweep {
                token: WETH, // leftover-input refund: must not be taken as the payout
                recipient: ROUTER,
                amount_min: U256::from(1),
            },
            Sweep {
                token: TOKEN,
                recipient: USER,
                amount_min: U256::from(77),
            },
        ];
        let i = assemble(&p).unwrap();
        assert_eq!(i.recipient, USER);
        assert_eq!(i.min_amount_out, Some(U256::from(77)));
    }

    #[test]
    fn split_route_sums_amounts() {
        let p = plan(
            vec![
                leg(&[WETH, TOKEN], Some(60), 6, USER),
                leg(&[WETH, USDC, TOKEN], Some(40), 4, USER),
            ],
            Some(100),
            None,
        );
        let i = assemble(&p).unwrap();
        assert_eq!(i.amount_in, U256::from(100));
        assert_eq!(i.min_amount_out, Some(U256::from(10)));
        assert_eq!(i.route.len(), 2);
    }

    #[test]
    fn chained_route_uses_first_input_and_last_output() {
        // TOKEN -> USDC delivered to the router, then router-balance USDC -> WETH.
        let p = plan(
            vec![
                leg(&[TOKEN, USDC], Some(1000), 0, ROUTER),
                leg(&[USDC, WETH], None, 55, USER),
            ],
            None,
            None,
        );
        let i = assemble(&p).unwrap();
        assert_eq!((i.token_in, i.token_out), (TOKEN, WETH));
        assert_eq!(i.amount_in, U256::from(1000));
        assert_eq!(i.min_amount_out, Some(U256::from(55)));
    }

    #[test]
    fn router_balance_input_resolved_from_wrapped_eth() {
        let p = plan(vec![leg(&[WETH, TOKEN], None, 1, USER)], Some(42), None);
        assert_eq!(assemble(&p).unwrap().amount_in, U256::from(42));

        let p = plan(vec![leg(&[TOKEN, WETH], None, 1, USER)], None, None);
        assert!(matches!(
            assemble(&p),
            Err(DecodeError::UnsupportedRoute(_))
        ));
    }

    #[test]
    fn unwrap_is_a_refund_when_output_is_not_weth() {
        // Exact-out BUY paid in ETH: leftover ETH is unwrapped back to the user.
        let mut l = leg(&[WETH, TOKEN], Some(100), 7, USER);
        l.kind = SwapKind::ExactOut;
        let p = plan(vec![l], Some(100), Some((USER, 0)));
        let i = assemble(&p).unwrap();
        assert_eq!(i.side, TradeSide::Buy);
        assert!(i.native_in && !i.native_out);
        assert_eq!(i.recipient, USER);
    }

    #[test]
    fn rejects_unsupported_shapes() {
        let token_to_token = plan(vec![leg(&[TOKEN, USDC], Some(1), 1, USER)], None, None);
        assert!(matches!(
            assemble(&token_to_token),
            Err(DecodeError::UnsupportedRoute(_))
        ));

        let diverging = plan(
            vec![
                leg(&[WETH, TOKEN], Some(1), 1, USER),
                leg(&[WETH, USDC], Some(1), 1, USER),
            ],
            None,
            None,
        );
        assert!(assemble(&diverging).is_err());

        let eth_but_not_weth = plan(vec![leg(&[TOKEN, WETH], Some(1), 1, USER)], Some(1), None);
        assert!(assemble(&eth_but_not_weth).is_err());

        let zero = plan(vec![leg(&[WETH, TOKEN], Some(0), 1, USER)], None, None);
        assert!(matches!(
            assemble(&zero),
            Err(DecodeError::InvalidCalldata(_))
        ));

        let mut mixed = leg(&[WETH, TOKEN], Some(1), 1, USER);
        mixed.kind = SwapKind::ExactOut;
        let mixed = plan(
            vec![leg(&[WETH, TOKEN], Some(1), 1, USER), mixed],
            None,
            None,
        );
        assert!(assemble(&mixed).is_err());
    }
}
