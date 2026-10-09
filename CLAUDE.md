# CLAUDE.md — dex-copy-trade

Ethereum copy-trading bot (Rust, Tokio, Alloy 2.5). Theo dõi các ví trong `COPY_WALLETS`,
decode swap Uniswap (V2 / V3 / Universal Router / V4) thành `TradeIntent`, áp strategy +
risk, rồi **tự build giao dịch mới** cho ví bot. Bot không bao giờ replay calldata gốc.
Chỉ hỗ trợ mainnet (`CHAIN_ID=1`). Trạng thái: Phase 1–7 xong (xem `PLAN.md`).

Tài liệu: `README.md` (vận hành, env, troubleshooting), `ARCHITECTURE.md` (thiết kế chi
tiết + giới hạn đã biết), `PLAN.md` (lịch sử phase). Docs viết tiếng Việt, code/comment
tiếng Anh.

## Lệnh

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test   # bắt buộc xanh trước khi xong việc
ETH_HTTP_URL=https://ethereum-rpc.publicnode.com cargo test -- --ignored   # live mainnet + Anvil e2e
ETH_HTTP_URL=... cargo test --test anvil_e2e -- --ignored --nocapture --test-threads 1
cargo run --release            # đọc .env; DRY_RUN=true mặc định
docker compose up --build      # distroless nonroot, ~46MB
```

- Test `#[ignore]` cần mạng: chúng chỉ đọc state, simulate, hoặc gửi tx bằng key rỗng (bị
  node từ chối). Không test nào tiêu tiền thật.
- `anvil_e2e` cần Foundry (`~/.foundry/bin/anvil`, hoặc đặt `ANVIL_BIN`).
- Trên macOS không có lệnh `timeout`. Trong zsh, `for p in "a b"; set -- $p` không tách từ.

## Pipeline (`src/main.rs` ghép các stage bằng bounded `mpsc`)

```
watcher/        pending (full pending tx) | confirmed (newHeads + backfill ≤32 block) → WatchedTx
decoder/        classifier → DexDecoder → SwapPlan → assemble_intent() → TradeIntent
decision.rs     RiskEngine::precheck (không RPC) → ChainSnapshot (1 lượt RPC song song)
                → strategy::Sizer → RiskEngine::check → CopyTrade        [chạy tuần tự, có chủ đích]
execution/      gas policy → Executor::plan (2 bundle eth_simulateV1) → [live] Broadcaster
                (ký + gửi trong nonce lease) → track receipt               [EXECUTOR_WORKERS song song]
```

| Thư mục | Nội dung chính |
|---|---|
| `models/` | `TradeIntent`, `RouteLeg` (+ `V4Path`/`V4Hop`), `CopyTrade`, `ChainSnapshot`, `Dex`, `PoolKind` |
| `decoder/` | `uniswap_v2.rs`, `uniswap_v3.rs` (SwapRouter + 02), `universal_router.rs`, `uniswap_v4.rs` (action V4); `assemble_intent` gộp split/chained route |
| `dex/` | `DexAdapter` (thuần, không RPC): build quote call + swap tx. V2 Router02, V3 SwapRouter02 + QuoterV2, V4 qua Universal Router v2 + V4Quoter. **Kiểu ABI V4 dùng chung nằm ở `dex/uniswap_v4.rs`** |
| `execution/` | `executor.rs` (route → quote → impact → approvals → đo lượng thực nhận → plan), `simulator.rs`, `allowance.rs` (ERC20 + Permit2), `gas.rs`, `broadcast.rs` |
| `ethereum/` | `provider.rs` (redact URL), `signer.rs` (`BotSigner`), `nonce.rs` (lease), `chain_state.rs` (`ChainReader`) |
| `risk/`, `strategy/`, `storage/` | Rule risk, 4 strategy, `DedupStore` (in-memory, TTL 24h) |
| `utils/` | `chain_addresses(1)`: mọi địa chỉ contract (router, quoter, Permit2, WETH, Chainlink, Multicall3), `Backoff` |

Thêm DEX: implement `DexDecoder` (đăng ký trong `DecoderRegistry::new`) và `DexAdapter`
(thêm vào `Executor::adapter`), và một biến thể `PoolKind`/`Dex` nếu cần.

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

## Quy ước khi sửa code

- Comment chỉ giải thích *tại sao* (ràng buộc, workaround); mỗi lần đơn giản hoá có chủ đích
  thì ghi rõ trần/giới hạn.
- Test: unit test cạnh code; mock qua trait (`ChainReader`, `Simulator`, `TxSender`,
  `GasStrategy`, `FeeSource`, `DedupStore`). Decoder kiểm bằng fixture tx mainnet thật trong
  `tests/fixtures/`, giá trị kỳ vọng đã đối chiếu với receipt. Thêm decoder mới thì nên quét
  tx thật và đối chiếu receipt trước khi tin.
- Thay đổi `RouteLeg` / `ChainReader` / `CallOutcome` / `PlannedTx` kéo theo việc sửa nhiều
  mock và struct literal trong test (`grep -rn "RouteLeg {"`).
- `rust-version = "1.85"`: không dùng let-chains (`if … && let …`).

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

## Giới hạn hiện tại

Swap token↔token và route ghép nhiều protocol bị bỏ qua. Bot không tự tìm route (đi theo
leg của ví nguồn). Tx bị kẹt không được tự thay thế (replace-by-fee). Dedup chỉ lưu
in-memory. Thuế *bán* / hook chặn bán không phát hiện được lúc mua.
