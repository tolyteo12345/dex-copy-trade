//! End-to-end on an Anvil mainnet fork: a source wallet's *pending* Uniswap swap is decoded,
//! sized, risk-checked, quoted on top of the pending source tx, signed, broadcast, mined and
//! its receipt tracked — then the same for the copy SELL (approve + swap).
//!
//! Needs `anvil` (Foundry) and a mainnet RPC to fork:
//! `ETH_HTTP_URL=https://... cargo test --test anvil_e2e -- --ignored --nocapture`

use std::collections::HashSet;
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use alloy::primitives::{Address, B256, Bytes, U256, address, utils::parse_ether};
use alloy::providers::{DynProvider, Provider};
use alloy::rpc::types::TransactionRequest;
use alloy::sol;
use alloy::sol_types::SolCall;
use dex_copy_trade::eth::config::GasCapMode;
use dex_copy_trade::eth::decision::DecisionStage;
use dex_copy_trade::eth::decoder::{DecodeOutcome, DecoderRegistry};
use dex_copy_trade::eth::dex::uniswap_v4::UniswapV4Adapter;
use dex_copy_trade::eth::dex::{DexAdapter, SwapParams};
use dex_copy_trade::eth::ethereum::chain_state::RpcChainReader;
use dex_copy_trade::eth::ethereum::nonce::NonceManager;
use dex_copy_trade::eth::ethereum::provider;
use dex_copy_trade::eth::ethereum::signer::BotSigner;
use dex_copy_trade::eth::execution::broadcast::{Broadcaster, RpcSender};
use dex_copy_trade::eth::execution::executor::ExecutionPlan;
use dex_copy_trade::eth::execution::executor::{Executor, ExecutorConfig};
use dex_copy_trade::eth::execution::gas::{CappedEip1559, GasStrategy, RpcFeeSource};
use dex_copy_trade::eth::execution::simulator::RpcSimulator;
use dex_copy_trade::eth::models::{Dex, PoolKind, RouteLeg, TradeIntent, TradeSide, V4Hop, V4Path};
use dex_copy_trade::eth::risk::{RiskConfig, RiskEngine};
use dex_copy_trade::eth::storage::{DEDUP_TTL, InMemoryDedup};
use dex_copy_trade::eth::strategy::Sizer;
use dex_copy_trade::eth::strategy::copy_strategy::FixedEth;
use dex_copy_trade::eth::utils::{ChainAddresses, chain_addresses};
use dex_copy_trade::eth::watcher::{TxOrigin, WatchedTx};

sol! {
    interface IV2Router {
        function swapExactETHForTokens(uint256 amountOutMin, address[] path, address to, uint256 deadline) payable;
        function swapExactTokensForETH(uint256 amountIn, uint256 amountOutMin, address[] path, address to, uint256 deadline);
    }
    interface IPermit2Approve {
        function approve(address token, address spender, uint160 amount, uint48 expiration);
    }
    interface IToken {
        function approve(address spender, uint256 amount) returns (bool);
        function balanceOf(address owner) view returns (uint256);
    }
}

// Anvil default accounts (public dev keys, fork-only).
const BOT_KEY: &str = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
const BOT: Address = address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
const SOURCE: Address = address!("70997970C51812dc3A010C7d01b50e0d17dc79C8");
const USDC: Address = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
const WETH_ADDR: Address = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");

struct Anvil {
    child: Child,
    url: String,
}

impl Drop for Anvil {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn anvil_bin() -> String {
    if let Ok(bin) = std::env::var("ANVIL_BIN") {
        return bin;
    }
    let home_bin = format!(
        "{}/.foundry/bin/anvil",
        std::env::var("HOME").unwrap_or_default()
    );
    if std::path::Path::new(&home_bin).exists() {
        home_bin
    } else {
        "anvil".into()
    }
}

async fn start_anvil(fork_url: &str) -> (Anvil, DynProvider) {
    // Load-balanced public RPCs serve the newest block from some backends only; forking a
    // few blocks behind head keeps every lazy state fetch answerable by any backend.
    let upstream = provider::connect_http(fork_url).unwrap();
    let fork_block = upstream.get_block_number().await.unwrap() - 5;
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let child = Command::new(anvil_bin())
        .args([
            "--fork-url",
            fork_url,
            "--fork-block-number",
            &fork_block.to_string(),
            "--retries",
            "10",
            "--chain-id",
            "1",
            "--port",
            &port.to_string(),
            "--silent",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("anvil not found: install Foundry or set ANVIL_BIN");
    let anvil = Anvil {
        child,
        url: format!("http://127.0.0.1:{port}"),
    };
    let http = provider::connect_http(&anvil.url).unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    while http.get_chain_id().await.is_err() {
        assert!(Instant::now() < deadline, "anvil did not start");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    (anvil, http)
}

async fn usdc_balance(http: &DynProvider, who: Address) -> U256 {
    let req = TransactionRequest::default()
        .to(USDC)
        .input(IToken::balanceOfCall { owner: who }.abi_encode().into());
    U256::from_be_slice(&http.call(req).await.unwrap())
}

/// Sends from an unlocked Anvil account; stays pending while automine is off. Gas limit is
/// explicit: Anvil's auto-estimate is occasionally a little short, and an out-of-gas source
/// tx is (correctly) not copied by the bot.
async fn send_from_source(http: &DynProvider, to: Address, value: U256, input: Vec<u8>) -> B256 {
    let tx = TransactionRequest::default()
        .from(SOURCE)
        .to(to)
        .value(value)
        .input(input.into())
        .gas_limit(300_000);
    *http.send_transaction(tx).await.unwrap().tx_hash()
}

async fn mine(http: &DynProvider) {
    let _: serde_json::Value = http.raw_request("evm_mine".into(), ()).await.unwrap();
}

fn deadline() -> U256 {
    U256::from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 600,
    )
}

async fn decode_pending(http: &DynProvider, addrs: &ChainAddresses, hash: B256) -> TradeIntent {
    let tx = http
        .get_transaction_by_hash(hash)
        .await
        .unwrap()
        .expect("pending tx visible");
    assert!(tx.block_number.is_none(), "source must still be pending");
    let item = WatchedTx {
        tx,
        origin: TxOrigin::Pending,
        detected_at: Instant::now(),
    };
    match DecoderRegistry::new(addrs, &DEXES).process(&item) {
        DecodeOutcome::Trade(i) => *i,
        other => panic!("source swap not decoded: {other:?}"),
    }
}

const DEXES: [Dex; 3] = [Dex::UniswapV2, Dex::UniswapUniversalRouter, Dex::UniswapV4];

/// Bot pipeline wired to an Anvil fork, exactly as `main` wires it (minus the watcher).
struct Harness {
    _anvil: Anvil,
    http: DynProvider,
    addrs: ChainAddresses,
    decision: DecisionStage<RpcChainReader, InMemoryDedup>,
    executor: Executor<RpcChainReader, RpcSimulator>,
    gas: CappedEip1559<RpcFeeSource>,
    broadcaster: Broadcaster<RpcSender>,
}

async fn harness() -> Harness {
    let fork = std::env::var("ETH_HTTP_URL").expect("set ETH_HTTP_URL (mainnet RPC to fork)");
    let (anvil, http) = start_anvil(&fork).await;
    let addrs = chain_addresses(1).unwrap();
    let _: serde_json::Value = http
        .raw_request("evm_setAutomine".into(), (false,))
        .await
        .unwrap();
    // The public Anvil dev accounts carry EIP-7702 "sweeper" delegations on mainnet that
    // forward any incoming ETH; strip them on the fork so the accounts behave like EOAs.
    for who in [BOT, SOURCE] {
        let _: serde_json::Value = http
            .raw_request("anvil_setCode".into(), (who, "0x"))
            .await
            .unwrap();
    }
    let signer = BotSigner::from_hex(BOT_KEY).unwrap();
    assert_eq!(signer.address(), BOT);
    let reader = || RpcChainReader::new(http.clone(), addrs.eth_usd_feed);
    let decision = DecisionStage::new(
        reader(),
        Sizer::new(
            Box::new(FixedEth {
                amount_wei: parse_ether("0.05").unwrap(),
            }),
            10_000,
            parse_ether("0.5").unwrap(),
        ),
        RiskEngine::new(
            RiskConfig {
                chain_id: 1,
                supported_dexes: DEXES.to_vec(),
                token_blacklist: HashSet::new(),
                max_trade_wei: parse_ether("0.5").unwrap(),
                max_gas_wei: 500_000_000_000,
                enforce_gas_cap: true,
                cooldown: Duration::ZERO,
            },
            InMemoryDedup::new(DEDUP_TTL),
        ),
        BOT,
        addrs.weth,
    );
    let executor = Executor::new(
        reader(),
        RpcSimulator::new(http.clone()),
        &addrs,
        ExecutorConfig {
            bot: BOT,
            max_slippage_bps: 300,
            max_trade_wei: parse_ether("0.5").unwrap(),
        },
    );
    let gas = CappedEip1559::new(
        RpcFeeSource::new(http.clone()),
        500_000_000_000,
        GasCapMode::Skip,
    );
    let broadcaster =
        Broadcaster::new(RpcSender::new(http.clone()), signer, NonceManager::new(), 1);
    Harness {
        _anvil: anvil,
        http,
        addrs,
        decision,
        executor,
        gas,
        broadcaster,
    }
}

impl Harness {
    /// Pending source tx -> decode -> decide -> plan (on top of the source) -> sign ->
    /// broadcast -> mine -> receipt. Returns the plan and the bot's copied amount.
    async fn copy(&mut self, source: B256, side: TradeSide, steps: &[&str]) -> ExecutionPlan {
        let intent = decode_pending(&self.http, &self.addrs, source).await;
        assert_eq!(intent.side, side);
        let trade = self.decision.decide(intent).await.expect("trade approved");
        let fees = self.gas.fees().await.unwrap();
        let plan = self.executor.plan(trade).await.expect("trade planned");
        assert!(
            plan.after_source,
            "quoted on top of the pending source swap"
        );
        assert_eq!(plan.txs.iter().map(|t| t.step).collect::<Vec<_>>(), steps);
        let sent = self.broadcaster.send(&plan, fees).await.expect("broadcast");
        for pair in sent.txs.windows(2) {
            assert_eq!(pair[1].nonce, pair[0].nonce + 1, "consecutive nonces");
        }
        mine(&self.http).await;
        let receipt = self.broadcaster.track(&sent).await.expect("receipt");
        assert!(receipt.success, "{side:?} swap succeeded on-chain");
        plan
    }
}

#[tokio::test]
#[ignore]
async fn copy_buy_then_sell_on_mainnet_fork() {
    let mut h = harness().await;
    let (http, addrs) = (h.http.clone(), h.addrs.clone());

    // BUY: source buys USDC with 1 ETH via V2 (pending) -> bot copies with 0.05 ETH.
    let buy = IV2Router::swapExactETHForTokensCall {
        amountOutMin: U256::ZERO,
        path: vec![addrs.weth, USDC],
        to: SOURCE,
        deadline: deadline(),
    };
    let src = send_from_source(
        &http,
        addrs.uniswap_v2_router,
        parse_ether("1").unwrap(),
        buy.abi_encode(),
    )
    .await;
    let plan = h.copy(src, TradeSide::Buy, &["swap"]).await;
    let usdc = usdc_balance(&http, BOT).await;
    assert!(
        usdc >= plan.min_out,
        "received {usdc} < min_out {}",
        plan.min_out
    );
    eprintln!(
        "V2 BUY ok: 0.05 ETH -> {usdc} USDC-wei (min_out {})",
        plan.min_out
    );

    // SELL: source sells its USDC (pending) -> bot sells 100% (approve + swap).
    let src_usdc = usdc_balance(&http, SOURCE).await;
    let approve = IToken::approveCall {
        spender: addrs.uniswap_v2_router,
        amount: U256::MAX,
    };
    send_from_source(&http, USDC, U256::ZERO, approve.abi_encode()).await;
    mine(&http).await;
    let sell = IV2Router::swapExactTokensForETHCall {
        amountIn: src_usdc,
        amountOutMin: U256::ZERO,
        path: vec![USDC, addrs.weth],
        to: SOURCE,
        deadline: deadline(),
    };
    let src = send_from_source(
        &http,
        addrs.uniswap_v2_router,
        U256::ZERO,
        sell.abi_encode(),
    )
    .await;
    let eth_before = http.get_balance(BOT).await.unwrap();
    h.copy(src, TradeSide::Sell, &["approve", "swap"]).await;
    assert_eq!(usdc_balance(&http, BOT).await, U256::ZERO);
    let eth_after = http.get_balance(BOT).await.unwrap();
    assert!(eth_after > eth_before, "bot received ETH net of gas");
    eprintln!(
        "V2 SELL ok: {usdc} USDC-wei -> +{} wei ETH net of gas",
        eth_after - eth_before
    );
}

/// ETH/USDC Uniswap V4 pool (native ETH, 0.05%, tick spacing 10, no hooks).
fn v4_eth_usdc(side: TradeSide) -> RouteLeg {
    let (currency_in, currency_out, tokens) = match side {
        TradeSide::Buy => (Address::ZERO, USDC, vec![WETH_ADDR, USDC]),
        TradeSide::Sell => (USDC, Address::ZERO, vec![USDC, WETH_ADDR]),
    };
    RouteLeg {
        pool: PoolKind::UniswapV4,
        tokens,
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

/// Source swap calldata through the Universal Router's V4_SWAP (TAKE_ALL pays msg.sender).
fn v4_source_tx(addrs: &ChainAddresses, side: TradeSide, amount_in: U256) -> TransactionRequest {
    UniswapV4Adapter::new(addrs)
        .swap_tx(
            &v4_eth_usdc(side),
            &SwapParams {
                side,
                amount_in,
                min_out: U256::ZERO,
                recipient: SOURCE,
                deadline: deadline(),
            },
        )
        .unwrap()
}

#[tokio::test]
#[ignore]
async fn copy_v4_buy_then_sell_on_mainnet_fork() {
    let mut h = harness().await;
    let (http, addrs) = (h.http.clone(), h.addrs.clone());

    // BUY: source buys USDC with 1 ETH in a native-ETH V4 pool (pending).
    let tx = v4_source_tx(&addrs, TradeSide::Buy, parse_ether("1").unwrap());
    let src = send_from_source(
        &http,
        addrs.universal_router_v4,
        parse_ether("1").unwrap(),
        tx.input.input().unwrap().to_vec(),
    )
    .await;
    let plan = h.copy(src, TradeSide::Buy, &["swap"]).await;
    assert_eq!(plan.route.pool, PoolKind::UniswapV4);
    let usdc = usdc_balance(&http, BOT).await;
    assert!(
        usdc >= plan.min_out,
        "received {usdc} < min_out {}",
        plan.min_out
    );
    eprintln!(
        "V4 BUY ok: 0.05 ETH -> {usdc} USDC-wei (min_out {})",
        plan.min_out
    );

    // SELL: source sets up Permit2 (token -> Permit2 -> router), then sells (pending).
    let src_usdc = usdc_balance(&http, SOURCE).await;
    let to_permit2 = IToken::approveCall {
        spender: addrs.permit2,
        amount: U256::MAX,
    };
    send_from_source(&http, USDC, U256::ZERO, to_permit2.abi_encode()).await;
    let to_router = IPermit2Approve::approveCall {
        token: USDC,
        spender: addrs.universal_router_v4,
        amount: alloy::primitives::aliases::U160::MAX,
        expiration: alloy::primitives::aliases::U48::MAX,
    };
    send_from_source(&http, addrs.permit2, U256::ZERO, to_router.abi_encode()).await;
    mine(&http).await;
    let tx = v4_source_tx(&addrs, TradeSide::Sell, src_usdc);
    let src = send_from_source(
        &http,
        addrs.universal_router_v4,
        U256::ZERO,
        tx.input.input().unwrap().to_vec(),
    )
    .await;

    let eth_before = http.get_balance(BOT).await.unwrap();
    h.copy(
        src,
        TradeSide::Sell,
        &["approve", "permit2_approve", "swap"],
    )
    .await;
    assert_eq!(usdc_balance(&http, BOT).await, U256::ZERO);
    let eth_after = http.get_balance(BOT).await.unwrap();
    assert!(eth_after > eth_before, "bot received native ETH net of gas");
    eprintln!(
        "V4 SELL ok: {usdc} USDC-wei -> +{} wei ETH net of gas",
        eth_after - eth_before
    );
}
