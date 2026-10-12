# CLAUDE.md — dex-copy-trade

Copy-trading bot (Rust, Tokio). Một crate, **hai binary = hai process riêng**:
- `eth-copy-trade` (`src/eth/`, Alloy 2.5): theo dõi các ví trong `COPY_WALLETS`, decode swap
  Uniswap (V2 / V3 / Universal Router / V4) thành `TradeIntent`, áp strategy + risk, rồi
  **tự build giao dịch mới** cho ví bot. Chỉ mainnet (`CHAIN_ID=1`).
- `sol-copy-trade` (`src/sol/`, feature `sol`): pump.fun bonding curve + PumpSwap trên Solana
  mainnet. Chi tiết ở `SOLANA.md`.

Bot không bao giờ replay calldata/instruction gốc. `eth/` và `sol/` không import lẫn nhau,
chỉ cùng dùng `src/common/` (Telegram, `TtlSet` dedup, parse env, redact URL, tracing). Trạng
thái: Phase 1–8 xong (xem `PLAN.md`).

Tài liệu: `README.md` (vận hành ETH, env, troubleshooting), `SOLANA.md` (bot SOL),
`ARCHITECTURE.md` (thiết kế chi tiết + giới hạn đã biết), `DEPLOY.md`, `PLAN.md` (lịch sử
phase). Docs viết tiếng Việt, code/comment tiếng Anh.

## Lệnh

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test   # bắt buộc xanh trước khi xong việc
cargo clippy --no-default-features --all-targets -- -D warnings        # build chỉ ETH cũng phải sạch
ETH_HTTP_URL=https://ethereum-rpc.publicnode.com cargo test --test 'eth_*' -- --ignored   # live ETH + Anvil e2e
ETH_HTTP_URL=... cargo test --test eth_anvil_e2e -- --ignored --nocapture --test-threads 1
SOL_RPC_URL=https://solana-rpc.publicnode.com cargo test --test sol_live -- --ignored --nocapture
cargo run --release --bin eth-copy-trade   # đọc .env; DRY_RUN=true mặc định
cargo run --release --bin sol-copy-trade   # SOL_DRY_RUN=true mặc định (độc lập với DRY_RUN)
docker compose up -d --build [eth-bot|sol-bot]   # mỗi chain một container distroless nonroot
```

- Test `#[ignore]` cần mạng: chúng chỉ đọc state, simulate, hoặc gửi tx bằng key rỗng (bị
  node từ chối). Không test nào tiêu tiền thật.
- `anvil_e2e` cần Foundry (`~/.foundry/bin/anvil`, hoặc đặt `ANVIL_BIN`).
- Trên macOS không có lệnh `timeout`. Trong zsh, `for p in "a b"; set -- $p` không tách từ.

## Pipeline ETH (`src/bin/eth-copy-trade.rs` ghép các stage bằng bounded `mpsc`; module trong `src/eth/`)

```
watcher/        pending (full pending tx) | confirmed (newHeads + backfill ≤32 block) → WatchedTx
decoder/        classifier → DexDecoder → SwapPlan → assemble_intent() → TradeIntent
decision.rs     RiskEngine::precheck (không RPC) → ChainSnapshot (1 lượt RPC song song)
                → strategy::Sizer → RiskEngine::check → CopyTrade        [chạy tuần tự, có chủ đích]
execution/      gas policy → Executor::plan (2 bundle eth_simulateV1) → [live] Broadcaster
                (ký + gửi trong nonce lease) → track receipt               [EXECUTOR_WORKERS song song]
```

| Thư mục (`src/eth/`) | Nội dung chính |
|---|---|
| `models/` | `TradeIntent`, `RouteLeg` (+ `V4Path`/`V4Hop`), `CopyTrade`, `ChainSnapshot`, `Dex`, `PoolKind` |
| `decoder/` | `uniswap_v2.rs`, `uniswap_v3.rs` (SwapRouter + 02), `universal_router.rs`, `uniswap_v4.rs` (action V4); `assemble_intent` gộp split/chained route |
| `dex/` | `DexAdapter` (thuần, không RPC): build quote call + swap tx. V2 Router02, V3 SwapRouter02 + QuoterV2, V4 qua Universal Router v2 + V4Quoter. **Kiểu ABI V4 dùng chung nằm ở `dex/uniswap_v4.rs`** |
| `execution/` | `executor.rs` (route → quote → impact → approvals → đo lượng thực nhận → plan), `simulator.rs`, `allowance.rs` (ERC20 + Permit2), `gas.rs`, `broadcast.rs` |
| `ethereum/` | `provider.rs` (re-export redact từ `common`), `signer.rs` (`BotSigner`), `nonce.rs` (lease), `chain_state.rs` (`ChainReader`) |
| `risk/`, `strategy/`, `storage/` | Rule risk, 4 strategy, `DedupStore` (`InMemoryDedup` = `common::dedup::TtlSet<DedupKey>`, TTL 24h) |
| `notify/` | Enum `Notification` + render (etherscan). Giao hàng ở `common/telegram.rs`: `Notifier::notify` chỉ `try_send` (không await, đầy thì bỏ); task nền gửi tuần tự, retry 429. URL chứa token: lỗi reqwest phải `without_url()` |
| `utils/` | `chain_addresses(1)`: mọi địa chỉ contract (router, quoter, Permit2, WETH, Chainlink, Multicall3) |

`common/`: `telegram.rs` (`Notifier<N: Render>`; mỗi chain có enum message riêng), `dedup.rs`
(`TtlSet<K>`; ETH bọc thành `InMemoryDedup` + trait `DedupStore`), `env.rs`, `redact.rs`,
`runtime.rs`, `backoff.rs`.

Thêm DEX (ETH): implement `DexDecoder` (đăng ký trong `DecoderRegistry::new`) và `DexAdapter`
(thêm vào `Executor::adapter`), và một biến thể `PoolKind`/`Dex` nếu cần.

## Pipeline SOL (`src/bin/sol-copy-trade.rs`, module trong `src/sol/`)

```
watcher.rs   Yellowstone gRPC (processed, failed=false, account_include=SOL_COPY_WALLETS) → SourceTx
decoder.rs   event pump (TradeEvent / BuyEvent / SellEvent) → SolTradeIntent   [không đọc instruction]
decision.rs  precheck (dedup signature, venue, blacklist, cooldown) → 1× getMultipleAccounts
             → resolve market (curve/pool) → sizing → số dư                       [tuần tự]
executor.rs  build (CU limit/price + pump::swap_instructions) → simulateTransaction
             → min_out → [live] sendTransaction → poll getSignatureStatuses      [SOL_EXECUTOR_WORKERS]
```

`pump.rs` chứa mọi hằng số/PDA/layout/discriminator của pump (từ IDL chính thức
github.com/pump-fun/pump-public-docs). `rpc.rs`: trait `SolRpc` (mock trong test) + client
JSON-RPC tự viết trên reqwest (không dùng `solana-rpc-client`/`solana-sdk`: quá nặng).

## Bất biến (không được phá)

- **Secret:** `PRIVATE_KEY` chỉ nằm trong `BotSigner` (`Debug` chỉ in địa chỉ; ký qua
  `wallet()`). Không log, serialize hay đưa nó vào error. URL RPC chứa API key: mọi
  `TransportError` đi qua `ProviderError::from` (đã redact); log Alloy transport bị ghim ở
  `info` trong `init_tracing`.
- **An toàn tiền:** `DRY_RUN` mặc định `true` và chỉ nhận đúng `true`/`false`. Live mode từ
  chối chạy nếu ví bot có code/delegation EIP-7702.
- **Simulate trước khi gửi:** tx cuối giống hệt tx đã simulate, chỉ khác `amountOutMin`
  (< lượng thực nhận). Không broadcast nếu bất kỳ bước nào revert.
- **`min_out`** = lượng *thực nhận* trong simulation (số dư trước/sau) × (1 − `MAX_SLIPPAGE_BPS`).
  Không lấy từ quote (quote bỏ qua thuế token), không copy min-out của ví nguồn (thường = 0).
- **Gas limit** = `peak_gas` (`maxUsedGas`, trước refund) × 1.3. Dùng `gasUsed` sẽ làm SELL hết gas.
- **Nonce:** chỉ cấp qua `NonceManager::lease`, giữ lock từ lúc reserve tới lúc gửi xong.
  Không reserve nonce ở chỗ khác.
- **Source tx pending** được đặt đầu bundle simulate (chỉ để mô phỏng state sau source,
  không bao giờ ký/gửi). Source revert trong mô phỏng thì không copy.
- **ETH/WETH:** trong `TradeIntent`, ETH luôn là WETH (kể cả `address(0)` của V4), kèm cờ
  `native_in`/`native_out`. Bot trả và nhận native ETH.
- Production path không dùng `unwrap()` hay `expect()`. Mỗi module có error riêng (`thiserror`).
  Một trade fail chỉ log, không làm crash bot. Dự án đặt `#![forbid(unsafe_code)]`.
- Mỗi source tx được đánh dấu "đã xử lý" ngay ở `precheck` (quyết định đúng một lần, kể cả
  khi bị reject).
- **Tách process:** code ETH không được phụ thuộc `sol/` hay dependency Solana; mọi thứ
  Solana nằm sau feature `sol`. Biến SOL luôn có tiền tố `SOL_` (`SOL_DRY_RUN` riêng).
- **SOL:** tx gửi = tx đã simulate, chỉ khác `min_out` và CU limit (cùng blockhash).
  `min_out` = lượng thực nhận trong simulation × (1 − `SOL_MAX_SLIPPAGE_BPS`): BUY = Δ ATA
  token, SELL = Δ (lamports ví + lamports ATA WSOL). CU limit = unitsConsumed × 1.3.
  `SOL_PRIVATE_KEY` chỉ nằm trong `SolSigner`; `SOL_GRPC_X_TOKEN` bọc trong `Secret`.

## Quy ước khi sửa code

- Comment chỉ giải thích *tại sao* (ràng buộc, workaround); mỗi lần đơn giản hoá có chủ đích
  thì ghi rõ trần/giới hạn.
- Test: unit test cạnh code; mock qua trait (`ChainReader`, `Simulator`, `TxSender`,
  `GasStrategy`, `FeeSource`, `DedupStore`). Decoder kiểm bằng fixture tx mainnet thật trong
  `tests/fixtures/`, giá trị kỳ vọng đã đối chiếu với receipt. Thêm decoder mới thì nên quét
  tx thật và đối chiếu receipt trước khi tin.
- Thay đổi `RouteLeg` / `ChainReader` / `CallOutcome` / `PlannedTx` kéo theo việc sửa nhiều
  mock và struct literal trong test (`grep -rn "RouteLeg {"`).
- `rust-version = "1.85"`: không dùng let-chains (`if … && let …`). (Thực tế alloy 2.5 đã
  cần rustc ≥ 1.94, crate Solana cần ≥ 1.89.)
- Test file trong `tests/` có tiền tố chain (`eth_*`, `sol_*`); test SOL mở đầu bằng
  `#![cfg(feature = "sol")]`. Fixture: `tests/fixtures/{eth,sol}/`.

## Bẫy đã gặp (đừng "sửa" ngược lại)

- **RPC public (publicnode) là load balancer:** backend có thể chậm block. Vì vậy watcher
  retry `get_block` (5 × 250ms), và Anvil test fork ở `head − 5`.
- **Key dev Anvil trên mainnet đều có delegation EIP-7702 "sweeper"** (tự chuyển ETH đi).
  Trên fork phải `anvil_setCode(addr, "0x")`. `is_contract()` không coi delegation là
  contract.
- **`eth_estimateGas` của Anvil đôi khi thiếu** cho tx nguồn. Test đặt gas limit 300k cho tx
  nguồn (bot đúng khi từ chối copy tx nguồn hết gas).
- **Universal Router:** `WRAP_ETH` sau swap là hoàn tiền; `UNWRAP_WETH` về router là đổi
  đầu vào; command `0x10` chỉ là `V4_SWAP` trên UR v2 (`universal_router_v4`).
- **V4:** chỉ take của currency *đầu ra* mới là payout. `OPEN_DELTA` (0) được lấy từ `SETTLE`
  trước đó. Fee `0x800000` = dynamic fee (hợp lệ).
- **Sell qua V4** cần 2 tầng approve: `token.approve(Permit2)` rồi `Permit2.approve(token, UR, max, max)`.
- **`eth_simulateV1` là bắt buộc** (kiểm tra lúc startup). Nó chạy với `validation=false`,
  base fee 0, nên số dư đo được không bị trừ phí gas.

- **Solana tx v1 (SIMD-0385) đã chạy trên mainnet:** `getTransaction` cần
  `maxSupportedTransactionVersion: 1`. Decoder chỉ dựa vào meta + event nên không phụ thuộc
  version message.
- **pump đổi instruction liên tục** (buy/buy_v2/buy_v3, exact_quote_in…). Đừng decode lệnh
  nguồn theo instruction: dùng event. Bot chỉ build v3 (curve) / v2 (PumpSwap), loại
  17 account cố định.
- **Event PumpSwap không có mint:** lấy `base_mint` từ instruction PumpSwap trade trên pool đó
  (`accounts[3]`, như nhau ở v1/v2). Token balance của `user_base_token_account` có thể không
  có (ATA WSOL tạo và đóng trong cùng tx).
- **`sol_amount` trong TradeEvent là số gộp trước phí** và các trường phí không cộng lại khớp
  được. Vì vậy `min_out` đo theo số dư trong simulation, không theo event.
- **Pool PumpSwap đảo chiều (base = WSOL) khá phổ biến:** bị bỏ qua (`Skip::InvertedPool`).
- **Coin cashback:** v3/v2 bị pump từ chối (6094/6079), nên decision loại trước.
- **Router trade qua PDA:** `user` trong event là PDA. Chỉ tính khi số dư token của người ký
  đổi đúng chiều.

## Giới hạn hiện tại

ETH: swap token↔token và route ghép nhiều protocol bị bỏ qua. Bot không tự tìm route (đi theo
leg của ví nguồn). Tx bị kẹt không được tự thay thế (replace-by-fee). Dedup chỉ lưu
in-memory. Thuế *bán* / hook chặn bán không phát hiện được lúc mua.
SOL: không backfill khi mất kết nối gRPC, không Jito / không tự gửi lại, chỉ coin ghép SOL,
không multi-hop. Xem `SOLANA.md → Giới hạn đã biết`.
