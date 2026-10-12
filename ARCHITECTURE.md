# Architecture — Copy Trading Bot (Ethereum + Solana)

## 1. Mục tiêu hệ thống

Theo dõi real-time một tập wallet (`COPY_WALLETS`), nhận diện giao dịch SWAP trên các
DEX được hỗ trợ, trích xuất ý định giao dịch (`TradeIntent`) độc lập với DEX, áp dụng
chiến lược copy + risk check, rồi tự build/sign/broadcast một giao dịch mới từ ví của bot.

Nguyên tắc cốt lõi: **không replay calldata gốc** — mọi giao dịch copy đều được build lại
từ `TradeIntent` đã chuẩn hoá, qua router/DEX adapter riêng của bot.

## 2. Luồng dữ liệu (pipeline)

```
Watcher (pending|confirmed)
   │  RawTxEvent (alloy Transaction)
   ▼  mpsc (bounded)
Decoder (classifier -> DexDecoder impls)
   │  TradeIntent
   ▼  mpsc (bounded)
Strategy (CopyStrategy: FIXED_ETH | FIXED_USD | RATIO | PROPORTIONAL)
   │  CopyTrade (sized intent)
   ▼
RiskEngine (pre-execution checks)
   │  CopyTrade (approved) | reject+log
   ▼  mpsc (bounded, N workers)
Executor (quote -> allowance -> build -> simulate -> sign -> broadcast -> receipt)
   │
   ▼
ReceiptTracker / metrics / structured logs
```

Mỗi mũi tên `mpsc` là một **bounded channel**; số lượng executor worker được cấu hình
(`EXECUTOR_WORKERS`), tránh spawn task không giới hạn. Watcher không bao giờ block vì
executor chậm — nếu channel đầy, event cũ nhất có thể bị drop với cảnh báo log (back-pressure
policy sẽ quyết định ở Phase 2+ sau khi đo latency thực tế).

## 3. Module layout

Một crate, hai binary, mỗi chain một process. Code chia theo thư mục: `eth/` và `sol/` không
import lẫn nhau, chỉ cùng dùng `common/`. Thiết kế bot Solana xem [SOLANA.md](SOLANA.md).

```
src/
  lib.rs                   pub mod common; pub mod eth; #[cfg(feature = "sol")] pub mod sol;
  bin/
    eth-copy-trade.rs      ETH: wiring, channel setup, graceful shutdown orchestration
    sol-copy-trade.rs      SOL: tương tự, process riêng (feature `sol`)
  common/                  dùng chung, không phụ thuộc eth/sol
    backoff.rs              exponential backoff
    dedup.rs                TtlSet<K>: "đã xử lý" in-memory có TTL (ETH: DedupKey, SOL: Signature)
    env.rs                  parse env (bool strict, ratio→bps, scheme URL, Telegram)
    redact.rs               che API key trong URL / thông báo lỗi
    runtime.rs              init_tracing, shutdown_signal
    telegram.rs             Notifier<N: Render> + hàng đợi + TelegramSink (retry, 429)
  eth/
    config/mod.rs            load + validate env config
    ethereum/
      provider.rs            alloy Provider (WS + HTTP fallback)
      signer.rs              PrivateKeySigner wrapper (zeroize-aware)
      nonce.rs               NonceManager (atomic, concurrency-safe)
      chain_state.rs         ChainReader
    watcher/
      mod.rs                 Watcher trait + mode dispatch
      pending.rs             pending tx subscription
      blocks.rs              new block subscription + tx scan
    decoder/
      mod.rs                 DexDecoder trait, registry
      classifier.rs          SWAP/TRANSFER/APPROVE/CONTRACT_CALL/DEPLOY/UNKNOWN
      uniswap_v2.rs, uniswap_v3.rs, universal_router.rs, uniswap_v4.rs
    dex/
      mod.rs                 Dex enum, DexAdapter trait (quote + build swap)
      uniswap_v2.rs, uniswap_v3.rs, uniswap_v4.rs
    strategy/                CopyStrategy: FIXED_ETH/FIXED_USD/RATIO/PROPORTIONAL
    risk/                    RiskEngine + RiskRule checks
    execution/               executor, simulator (eth_simulateV1), allowance, gas, broadcast
    models/                  TradeIntent, CopyTrade, ChainSnapshot
    notify/                  Notification (ETH) + render; giao Telegram ở common/telegram
    storage/                 DedupStore trait (Redis/PG-ready) trên common::dedup
    utils/                   chain_addresses(1)
    decision.rs              strategy + risk stage
  sol/
    config.rs                SOL_* env
    signer.rs                SolSigner (Keypair, Debug chỉ in pubkey)
    watcher.rs               Yellowstone gRPC → SourceTx
    decoder.rs               event pump → SolTradeIntent
    pump.rs                  địa chỉ, PDA, layout account, event, build instruction
    decision.rs              precheck + 1× getMultipleAccounts + sizing + risk
    executor.rs              build → simulate → min_out → [live] gửi → theo dõi
    rpc.rs                   SolRpc trait + JSON-RPC client (reqwest)
    notify.rs                SolNotification + render (solscan)
    models.rs                SourceTx, SolTradeIntent, SolCopyTrade
```

Feature `sol` (mặc định bật) gắn toàn bộ dependency Solana và gRPC. `cargo build
--no-default-features --bin eth-copy-trade` cho binary ETH không chứa crate Solana nào. Dù
build cả hai thì binary ETH cũng không link code `sol/`: hai process không chia sẻ tài nguyên
lúc chạy.

### Dependency direction

`models` không phụ thuộc module nào khác. `decoder`/`dex` chỉ phụ thuộc `models` + `ethereum`.
`strategy`/`risk` chỉ phụ thuộc `models`. `execution` phụ thuộc `models`, `dex`, `ethereum`,
`risk`. `watcher` chỉ phụ thuộc `ethereum` + `models` (raw tx event), không biết gì về
decoder/strategy — tách rời qua channel. Điều này cho phép thêm DEX mới (1inch, Curve, …)
chỉ bằng cách thêm 1 file trong `decoder/` + `dex/` và đăng ký vào registry, không sửa
`execution`/`strategy`/`risk`.

## 4. Các trait cốt lõi

```rust
trait DexDecoder: Send + Sync {
    fn dex(&self) -> Dex;
    fn can_decode(&self, tx: &Transaction) -> bool;
    fn decode(&self, tx: &Transaction) -> Result<Option<SwapPlan>, DecodeError>;
}

// BUY sizing only; SELL = SELL_RATIO x bot's own token balance (shared, in `Sizer`).
trait CopyStrategy: Send + Sync {
    fn needs(&self) -> DataNeeds; // which optional chain data to fetch
    fn buy_amount(&self, intent: &TradeIntent, chain: &ChainSnapshot) -> Result<U256, StrategyError>;
}

// Read-only chain access for decisions (mocked in tests).
trait ChainReader: Send + Sync {
    async fn gas_price(&self) -> Result<u128, ProviderError>;
    async fn eth_balance(&self, who: Address, at: BlockId) -> Result<U256, ProviderError>;
    async fn token_balance(&self, token: Address, who: Address, at: BlockId) -> Result<U256, ProviderError>;
    async fn is_contract(&self, addr: Address) -> Result<bool, ProviderError>;
    async fn eth_usd(&self) -> Result<EthUsdPrice, ProviderError>;
}

trait DexAdapter: Send + Sync {
    fn dex(&self) -> Dex;
    async fn quote(&self, trade: &CopyTrade) -> Result<Quote, ExecutionError>;
    async fn build_swap(&self, trade: &CopyTrade, quote: &Quote) -> Result<TypedTransaction, ExecutionError>;
}

trait GasStrategy: Send + Sync {
    async fn fees(&self) -> Result<Eip1559Fees, ExecutionError>;
}

trait DedupStore: Send + Sync {
    async fn mark_if_new(&self, key: DedupKey) -> Result<bool, StorageError>; // atomic check-and-set
}
```

### Decision stage (Phase 3) — `decision.rs`

```
TradeIntent
  -> RiskEngine::precheck  (no RPC: dedup mark, DEX whitelist, zero-address, blacklist, cooldown)
  -> ChainSnapshot         (1 concurrent RPC batch: gas, bot ETH, token is contract,
                            [SELL] bot token balance, [PROPORTIONAL] source ETH+WETH at block-1,
                            [FIXED_USD] Chainlink ETH/USD)
  -> Sizer (strategy)      (BUY: strategy amount capped at MAX_TRADE_ETH; SELL: SELL_RATIO x balance)
  -> RiskEngine::check     (invalid token, MAX_GAS_GWEI, MAX_TRADE_ETH, ETH for trade + gas reserve)
  -> CopyTrade
```

Rule là các method tường minh trong `RiskEngine`, không phải `Vec<Box<dyn RiskRule>>`:
số rule ít, thứ tự quan trọng (rẻ trước, cần RPC sau), và mỗi rule cần dữ liệu khác nhau.
Rule cần quote (`MAX_SLIPPAGE_BPS`/price impact, `MAX_TRADE_ETH` cho SELL) chạy ở checkpoint
sau quote của executor (Phase 4). Stage xử lý tuần tự có chủ đích để cooldown/balance không race.
`is_contract` coi EOA có delegation EIP-7702 (`0xef0100…`) là không phải contract.

## 5. ETH/WETH handling

- `TradeIntent.token_in`/`token_out` luôn là địa chỉ ERC20 thực tế được router dùng
  on-chain (router V2/V3 luôn dùng WETH nội bộ, kể cả khi user gửi native ETH qua
  `swapExactETHForTokens`).
- Thêm field ngữ cảnh `native_in: bool` / `native_out: bool` trên `TradeIntent` để
  phân biệt "target gửi native ETH" vs "target gửi WETH token" — quyết định bot nên
  gọi hàm `...ETH...` hay `...Tokens...` tương ứng khi build lại giao dịch.
- `WETH` và địa chỉ router theo `CHAIN_ID` nằm ở `utils::chain_addresses(chain_id)`
  (hiện chỉ mainnet; chain khác => fail startup).
- BUY = token_in thuộc {ETH, WETH} và token_out là token khác.
  SELL = token_in là token khác và token_out thuộc {ETH, WETH}.
  Swap token-token (không qua ETH/WETH) nằm ngoài scope V1 strategy sizing theo ETH —
  decoder trả `trade_ignored` với reason `token-to-token` (TradeIntent bắt buộc có side
  nên không tạo intent rồi để risk engine reject).

### Decode pipeline (Phase 2)

`DexDecoder::decode` trả `SwapPlan` cấp router (danh sách `SwapLeg`, ETH wrap, WETH
unwrap, sweep); `assemble_intent` dùng chung chuẩn hoá thành `TradeIntent`:
- Split route (nhiều leg cùng in/out): cộng amount. Chained route (leg trả output về
  router, leg sau tiêu thụ): token_in = leg đầu, token_out = leg cuối.
- Sentinel số lượng "router balance" (UR `1<<255`, SwapRouter02 `0`) được suy ra từ
  lượng ETH wrap; không suy ra được => ignore.
- Khi output nằm ở router, `UNWRAP_WETH`/`unwrapWETH9`/`SWEEP`/`sweepToken` của token
  output quyết định recipient và min-out thực.
- `amount_in` = lượng thực sự được swap (phí interface trích từ input không tính vào).
- Universal Router `V4_SWAP` (0x10, chỉ trên UR v2): `decoder/uniswap_v4.rs` đọc chuỗi action
  (`abi.encode(bytes actions, bytes[] params)`). Swap action → leg `PoolKind::UniswapV4` giữ
  `V4Path` (currency thật + `PoolKey` từng hop + hookData); `address(0)` (ETH native) được
  chuẩn hoá thành WETH trong `tokens`. `TAKE_ALL`/`TAKE` của currency *đầu ra* = payout
  (recipient + min thật); take của currency khác = hoàn tiền, bỏ qua. `OPEN_DELTA` (0) được
  suy ra từ `SETTLE` trước đó. Tắt bằng cách bỏ `UNISWAP_V4` khỏi `SUPPORTED_DEXES`.
- `WRAP_ETH` sau swap = hoàn ETH thừa; `UNWRAP_WETH` về chính router = đổi WETH đầu vào
  (vd trả pool V4 ETH native); unwrap khi đầu ra không phải WETH = hoàn tiền (exact-out).

## 6. Nonce & concurrency an toàn

`NonceManager` (`ethereum/nonce.rs`) cấp **lease**: một lease giữ lock của manager từ lúc
reserve `count` nonce liên tiếp tới khi các tx đã được gửi cho node (`finish`). Nhờ vậy:
- 2 trade không bao giờ dùng chung nonce, và **không có gap** (A giữ n, gửi lỗi, trong khi
  B đã gửi n+1 => B kẹt vĩnh viễn — không thể xảy ra vì gửi được tuần tự hoá).
- Chỉ phần ký (µs) + `eth_sendRawTransaction` bị tuần tự; quote/simulate vẫn song song.
- Lease drop/không gửi được => nonce không bị tiêu. Gửi lỗi giữa chừng (approve qua, swap
  bị từ chối) => giữ nonce đã gửi, đánh dấu resync (lần sau đọc `getTransactionCount(pending)`).
- Trạng thái: `pending` (đã gửi) → `confirmed(nonce)` khi có receipt; receipt timeout =>
  `resync`.

## 7. Dedup / idempotency

Key = `(chain_id, source_tx_hash)`. V1: `DashMap`-based in-memory store implement trait
`DedupStore`, TTL 24h, dọn lười tối đa 1 lần/phút. Trait tách riêng để sau này thay
bằng Redis (`SET NX`)/Postgres (unique insert) mà không đổi executor. Tx được đánh dấu
ngay ở `precheck` => mỗi source tx chỉ được *quyết định* một lần, kể cả khi bị reject
(lỗi RPC tạm thời làm rơi trade thay vì retry — trade retry sẽ đã cũ).

## 8. Simulation & DRY_RUN

Simulation **luôn chạy**, kể cả khi `DRY_RUN=true`. `DRY_RUN=true` chỉ tắt bước sign +
broadcast cuối cùng — toàn bộ pipeline phía trước chạy đầy đủ và log `trade_simulated`.
### Gas & broadcast (Phase 5)

- `GasStrategy` = `CappedEip1559`: base fee của block mới nhất + tip node đề xuất
  (`eth_maxPriorityFeePerGas`); không copy gas của source. `base + tip > MAX_GAS_GWEI` =>
  `GAS_CAP_MODE=skip` bỏ lệnh / `wait` kiểm tra lại mỗi 12s tới `GAS_WAIT_MAX_SECS`.
  `maxFee = min(2·base + tip, MAX_GAS_GWEI)`. Gas được kiểm tra *trước* khi quote.
- Ký bằng `EthereumWallet` lấy từ `BotSigner` (key không rời struct); EIP-1559, chain id
  từ config, gas limit = gas *đỉnh* khi simulate (`maxUsedGas`, trước refund) × 1.3 — gasUsed
  sau refund không đủ cho SELL đưa số dư token về 0 (đã gặp trên Anvil fork).
- Approve(s) + swap gửi theo thứ tự nonce; receipt của swap chờ tối đa 180s (`trade_confirmed`
  / `trade_failed`). Slot worker được trả ngay sau broadcast, không giữ trong lúc chờ receipt.

### Executor (Phase 4) — `execution/executor.rs`

Dùng `eth_simulateV1` (RPC bắt buộc hỗ trợ; kiểm tra lúc startup), 2 bundle / trade:

1. **Quote bundle**: `[source tx?] + với mỗi route ứng viên: quote(amount), quote(amount/1000)`.
   - Route ứng viên = các leg của source đi thẳng token_in → token_out (dedup). Adapter:
     V2 → Router02 `getAmountsOut`; V3 → QuoterV2 `quoteExactInput`. Chọn output lớn nhất.
   - Price impact = so tỉ giá của amount với tỉ giá của probe 1/1000 (fee pool triệt tiêu);
     > `MAX_SLIPPAGE_BPS` => reject. SELL có quote > `MAX_TRADE_ETH` => chỉ bán phần tương
     ứng `MAX_TRADE_ETH` (scale tuyến tính, bảo thủ).
2. **Measure bundle**: `[source tx?] + approvals + [balance trước, swap(min_out=0), balance sau]`.
   - Lượng thực nhận (sau thuế chuyển nhượng) = chênh lệch số dư (token `balanceOf` cho BUY,
     Multicall3 `getEthBalance` cho SELL). `min_out = received × (1 − MAX_SLIPPAGE_BPS)`.
   - Tx cuối giống hệt tx đã simulate, chỉ khác `amountOutMin` (< received) => cùng state
     thì chắc chắn qua.

**Source tx trong bundle**: khi source còn pending (`source_block = None` và nonce của source
chưa được mined), tx của source được đặt đầu bundle để quote/simulate trên state *sau* source
(tx của bot sẽ được mined sau source). Calldata gốc chỉ dùng trong simulation, không bao giờ
được ký/gửi. Source revert trong simulation => không copy.

**Execution**: bot luôn trả native ETH khi BUY và nhận native ETH khi SELL. V2: Router02
`swapExact{ETHForTokens,TokensForETH}SupportingFeeOnTransferTokens`; V3: SwapRouter02
`multicall(deadline, [exactInput, unwrapWETH9?])`. Deadline 120s.

**V4 execution** (`dex/uniswap_v4.rs`): Universal Router v2 `execute([WRAP_ETH?] V4_SWAP
[UNWRAP_WETH?])`, action `SWAP_EXACT_IN` (path) + `SETTLE_ALL`/`SETTLE` + `TAKE_ALL`/`TAKE`;
quote bằng V4Quoter `quoteExactInput`. ERC20 vào qua Permit2: executor thêm
`approve(token→Permit2)` và `permit2_approve(token, UR)` khi thiếu/hết hạn.

**Allowance**: SELL thiếu allowance => `approve(MAX)` cho router Uniswap (immutable, cố định);
allowance khác 0 nhưng thiếu => `approve(0)` trước (kiểu USDT).

**Concurrency**: `EXECUTOR_WORKERS` task song song (Semaphore + JoinSet); shutdown đợi task
đang chạy.

## 9. Metrics / latency

`TradeIntent.detected_at: Instant` được set ngay khi watcher nhận raw tx. Mỗi giai đoạn
tiếp theo (`decoded_at`, `decision_at`, `signed_at`, `broadcast_at`) được gắn vào struct
đi xuyên suốt pipeline (`CopyTrade` mang theo các mốc thời gian), không dùng global state.
Log 1 structured event `trade_*` ở mỗi giai đoạn với các field latency tính từ các mốc này.

## 10. Error handling

Mỗi module có error enum riêng (`thiserror`): `ProviderError`, `DecodeError`,
`StrategyError`, `RiskError`, `SimulationError`, `ExecutionError`. Executor bắt lỗi ở
boundary của từng `CopyTrade` — một trade fail chỉ log `trade_failed` + return, không
propagate panic lên watcher loop. Không `unwrap()`/`expect()` trên đường dữ liệu chạy
runtime (chỉ chấp nhận ở test hoặc static config đã validate tại startup).

## 11. Giới hạn đã biết của V1 (ghi nhận, không che giấu)

- Pending-mempool watch phụ thuộc RPC provider hỗ trợ `eth_subscribe("newPendingTransactions")`
  dạng full-tx hoặc hash-only (cần fetch thêm) — nhiều public RPC không hỗ trợ; README sẽ
  nêu rõ cần provider kiểu Alchemy/BlockPXY hỗ trợ pending full tx.
  — hướng nâng cấp: thêm flashbots/mempool-stream provider adapter sau.
  — tạm thời (chủ đích, ghi rõ): watcher/pending.rs sẽ coi thiếu hỗ trợ là lỗi cấu hình rõ
    ràng ở startup, không fallback âm thầm sang confirmed.
- Swap token-token không qua ETH/WETH: ngoài scope sizing V1 (xem mục 5).
- V4: pool có hook được copy như pool thường (simulation đo lượng thực nhận), nhưng hook có
  thể chặn/đánh thuế lúc *bán* — không phát hiện được lúc mua. Route ghép nhiều protocol
  (vd V3 exact-out → V4) không thực thi được.
- Min-out của source thường = 0 (342/388 trade mẫu, chủ yếu bot snipe) => bot phải tự
  tính slippage bound từ quote của chính mình, không copy min-out của source.
- Executor chỉ đi route mà một leg của source đi thẳng token_in → token_out (không tự tìm
  route, không ghép leg đa protocol). Thuế *bán* của token không đo được lúc mua.
- Startup: ví bot có code/delegation (EIP-7702 sweeper khi key lộ) => từ chối live mode.
- Tx bị kẹt (base fee vượt `maxFee`) không được tự thay thế/huỷ (replace-by-fee); chỉ resync
  nonce. Các trade sau sẽ xếp hàng sau nó cho tới khi nó được mined hoặc bị drop.
- Alloy WS tự reconnect + resubscribe bên trong; watcher chỉ restart session khi stream
  đóng/stall. Pending tx trong lúc mất kết nối bị mất (không khôi phục được).
- Dedup store in-memory: mất state khi restart — chấp nhận ở V1, có trait sẵn để thay
  persistent store.
