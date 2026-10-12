# dex-copy-trade — Ethereum Copy Trading Bot (Rust)

Bot theo dõi real-time một danh sách ví Ethereum. Khi một ví swap trên Uniswap, bot hiểu
*ý định* giao dịch (mua/bán token nào, bao nhiêu), áp dụng chiến lược copy + risk engine,
rồi **tự build giao dịch mới** cho ví của bot. Bot không bao giờ replay calldata gốc.

> ## ⚠️ CẢNH BÁO
> **`DRY_RUN=false` sẽ ký và gửi giao dịch bằng TÀI SẢN THẬT.** Mặc định `DRY_RUN=true`.
> Hãy chạy dry run, đọc log `trade_simulated`, rồi thử với một ví riêng chỉ chứa số tiền
> nhỏ trước khi chạy thật. Copy trading có rủi ro mất vốn (token lừa đảo, thuế bán cao,
> trượt giá, front-run…). Phần mềm không đảm bảo lợi nhuận.

---

## Mục lục
- [Architecture](#architecture)
- [Setup](#setup)
- [Environment variables](#environment-variables)
- [Running locally](#running-locally)
- [Dry run](#dry-run)
- [Production mode](#production-mode)
- [Supported DEX](#supported-dex)
- [Copy strategies](#copy-strategies)
- [Risk management](#risk-management)
- [Security considerations](#security-considerations)
- [Tests](#tests)
- [Troubleshooting](#troubleshooting)

---

## Architecture

```
Watcher (pending | confirmed, WebSocket)
   │ WatchedTx                       bounded mpsc (không block watcher)
Decoder  — classify → DexDecoder (V2 / V3 / Universal Router) → TradeIntent
   │                                 bounded mpsc
Decision — precheck (dedup, whitelist, blacklist, cooldown)
           → snapshot on-chain (1 lượt RPC song song) → strategy → risk check
   │ CopyTrade                       bounded mpsc
Executor (EXECUTOR_WORKERS song song)
           gas policy → quote → price impact → approve? → simulate (eth_simulateV1)
           → [DRY_RUN dừng ở đây] → ký → broadcast → theo dõi receipt
```

| Module | Vai trò |
|---|---|
| `config` | Đọc + validate toàn bộ env lúc startup |
| `watcher` | Subscribe pending tx / block mới, reconnect + backoff, backfill block bị lỡ |
| `decoder` | Phân loại tx, decode Uniswap, chuẩn hoá thành `TradeIntent` |
| `strategy` | Tính size lệnh (`FIXED_ETH`, `FIXED_USD`, `RATIO`, `PROPORTIONAL`) |
| `risk` | Risk engine trước khi thực thi |
| `storage` | Chống xử lý trùng (`DedupStore`, in-memory; sẵn trait cho Redis/Postgres) |
| `dex` | Adapter build quote/swap: V2 Router02, V3 SwapRouter02, V4 qua Universal Router |
| `execution` | Executor, simulator, allowance, gas, broadcast |
| `ethereum` | Provider, signer, nonce manager, đọc state on-chain |

Chi tiết thiết kế: [ARCHITECTURE.md](ARCHITECTURE.md). Tiến độ theo phase: [PLAN.md](PLAN.md).

### ETH / WETH
`token_in`/`token_out` luôn là địa chỉ ERC20, nên ETH được ghi là WETH. Bot **luôn trả
native ETH khi BUY và nhận native ETH khi SELL**. BUY = WETH → token, SELL = token → WETH.
Swap token↔token (không có phía ETH/WETH) bị bỏ qua.

---

## Setup

Yêu cầu:
- Rust stable ≥ 1.85 (edition 2024)
- RPC mainnet có:
  - **WebSocket** (`eth_subscribe`). Mode `pending` cần provider hỗ trợ
    `newPendingTransactions` dạng full tx body.
  - **HTTP** hỗ trợ **`eth_simulateV1`** (geth, reth, Nethermind, Alchemy… đều có). Bot
    kiểm tra lúc startup.
- Docker (tuỳ chọn) để chạy container.
- Foundry/Anvil (tuỳ chọn) để chạy integration test trên fork.

```bash
cp .env.example .env      # rồi sửa .env — KHÔNG commit file này
cargo build --release
```

---

## Environment variables

| Biến | Mặc định | Ý nghĩa |
|---|---|---|
| `CHAIN_ID` | `1` | Hiện chỉ hỗ trợ mainnet (bảng địa chỉ router/WETH) |
| `ETH_WS_URL` | *bắt buộc* | `ws://` / `wss://` cho subscription |
| `ETH_HTTP_URL` | *bắt buộc* | `http://` / `https://`, phải hỗ trợ `eth_simulateV1` |
| `PRIVATE_KEY` | *bắt buộc* | Key ví bot (32-byte hex). Không bao giờ được log |
| `COPY_WALLETS` | *bắt buộc* | Danh sách ví theo dõi, phân cách bằng dấu phẩy |
| `COPY_MODE` | `pending` | `pending` (mempool) / `confirmed` (block mới) |
| `COPY_STRATEGY` | `FIXED_ETH` | `FIXED_ETH` / `FIXED_USD` / `RATIO` / `PROPORTIONAL` |
| `FIXED_TRADE_ETH` | `0.05` | Size BUY cho `FIXED_ETH` |
| `FIXED_TRADE_USD` | `100` | Size BUY cho `FIXED_USD` (quy đổi bằng Chainlink ETH/USD) |
| `BUY_RATIO` | `0.10` | Tỉ lệ cho `RATIO` |
| `SELL_RATIO` | `1.0` | SELL bán bao nhiêu % số dư token của bot (≤ 1.0) |
| `MAX_TRADE_ETH` | `0.5` | Trần mỗi lệnh (BUY bị cắt; SELL chỉ bán phần ≤ trần) |
| `MAX_SLIPPAGE_BPS` | `300` | Giới hạn price impact và độ trượt cho `min_out` |
| `MAX_GAS_GWEI` | `100` | Trần base fee + tip; cũng là trần `maxFeePerGas` |
| `GAS_CAP_MODE` | `skip` | Khi gas vượt trần: `skip` bỏ lệnh / `wait` chờ |
| `GAS_WAIT_MAX_SECS` | `60` | Thời gian chờ tối đa ở mode `wait` |
| `DRY_RUN` | `true` | Chỉ nhận đúng `true`/`false`; gõ sai sẽ dừng startup |
| `SUPPORTED_DEXES` | cả 4 | `UNISWAP_V2,UNISWAP_V3,UNISWAP_UNIVERSAL_ROUTER,UNISWAP_V4` (V4 đi qua Universal Router nên cần cả hai) |
| `TOKEN_BLACKLIST` | rỗng | Token không bao giờ giao dịch |
| `TRADE_COOLDOWN_SECS` | `30` | Khoảng cách tối thiểu giữa 2 lệnh copy cùng token + chiều (0 = tắt) |
| `CHANNEL_CAPACITY` | `1024` | Kích thước các bounded channel |
| `EXECUTOR_WORKERS` | `4` | Số lệnh được quote/simulate song song |
| `TELEGRAM_BOT_TOKEN` | rỗng | Token bot từ @BotFather. Đặt cùng `TELEGRAM_CHAT_ID` để bật thông báo (tín hiệu copy, simulate/gửi/xác nhận/lỗi kèm link tx). Gửi qua task nền, hàng đợi đầy thì bỏ tin, không làm chậm trade |
| `TELEGRAM_CHAT_ID` | rỗng | Chat id user/group/channel (vd `-1001234567890` hoặc `@my_channel`) |
| `LOG_LEVEL` | `info` | Cú pháp `EnvFilter`, ví dụ `dex_copy_trade=debug,info` |

Mọi giá trị được validate lúc startup: địa chỉ phải hợp lệ (đúng EIP-55 nếu viết hoa/thường
lẫn lộn, khác zero), `COPY_WALLETS` không được chứa ví bot, các giới hạn phải hợp lý. Sai
là bot dừng ngay với thông báo rõ ràng.

---

## Running locally

```bash
cargo run --release
# hoặc
docker compose up --build
```

Dừng bằng `Ctrl+C` / `SIGTERM`. Bot ngừng nhận lệnh mới, đợi các lệnh đang chạy xong
(tối đa 15s), rồi thoát.

Log là structured (key=value), các event chính:

| Event | Khi nào |
|---|---|
| `wallet_tx_detected` | Ví theo dõi gửi tx |
| `trade_decoded` / `trade_ignored` / `tx_ignored` | Kết quả decode (kèm lý do) |
| `trade_approved` / `trade_rejected` | Strategy + risk (kèm lý do) |
| `trade_simulated` | Quote + simulate xong (price impact, thuế token, `min_out`, gas) |
| `trade_broadcasted` | Đã gửi (hash, nonce, latency) |
| `trade_confirmed` / `trade_failed` | Kết quả on-chain / lỗi ở stage nào |

Latency được log cho mỗi lệnh: `detection_to_decode_ms`, `decode_to_decision_ms`,
`decision_to_broadcast_ms`, `total_latency_ms` (ở dry run có `detection_to_simulated_ms`).

---

## Dry run

`DRY_RUN=true` (mặc định) chạy **toàn bộ** pipeline: decode, strategy, risk, quote, approve
và simulate swap trên state thật. Bước ký và gửi được bỏ qua. Log `trade_simulated` cho
biết lệnh copy *sẽ* trông thế nào. Ví bot vẫn phải có đủ ETH để qua risk check số dư.

---

## Production mode

1. Dùng **ví riêng cho bot**, chỉ nạp số tiền chấp nhận được mất.
2. Chạy dry run đủ lâu, đọc lý do `trade_rejected` / `trade_failed`.
3. Đặt `DRY_RUN=false`. Lúc startup bot cảnh báo `live_mode`, và **từ chối chạy** nếu ví
   bot có code/delegation (dấu hiệu key đã lộ, xem [Security](#security-considerations)).
4. Theo dõi `trade_broadcasted` → `trade_confirmed`.

Ở live mode:
- **Nonce:** reserve, ký và gửi trong cùng một khoá, nên không trùng nonce và không có nonce
  trống. Gửi lỗi hoặc chờ receipt quá hạn (180s) thì nonce được đọc lại từ chain.
- **Gas:** EIP-1559 với `maxFee = min(2·base + tip, MAX_GAS_GWEI)`, tip theo node đề xuất.
  Bot không copy gas của ví nguồn.
- **Gas limit** = gas đỉnh khi simulate (trước refund) × 1.3.

---

## Supported DEX

| Router | Hàm được decode |
|---|---|
| Uniswap V2 Router02 | 9 hàm `swap*` (kể cả `SupportingFeeOnTransferTokens`) |
| Uniswap V3 SwapRouter | `exactInput(Single)`, `exactOutput(Single)`, `multicall`, `unwrapWETH9`, `sweepToken`, `selfPermit*` |
| Uniswap V3 SwapRouter02 | Như trên (không deadline), `multicall(deadline/prevBlockhash)`, swap V2 nội bộ |
| Uniswap Universal Router (v1, v1.2, v2) | `V2/V3_SWAP_EXACT_IN/OUT`, `WRAP_ETH`, `UNWRAP_WETH`, `SWEEP`, phí (`PAY_PORTION`/`TRANSFER`), Permit2 |
| **Uniswap V4** (Universal Router v2, `V4_SWAP`) | Action `SWAP_EXACT_IN(_SINGLE)`, `SWAP_EXACT_OUT(_SINGLE)`, `SETTLE(_ALL)`, `TAKE(_ALL)`, `TAKE_PORTION`, … Single-hop và multi-hop, pool ETH native lẫn WETH/ERC20, pool có hook và dynamic fee |

V4: ETH native là `address(0)`; trong `TradeIntent` được chuẩn hoá thành WETH, kèm cờ
`native_in`/`native_out`. Leg V4 giữ nguyên `PoolKey` (currency thật, fee, tickSpacing, hooks,
hookData) để bot tái tạo đúng pool. Mẫu mainnet 10/2026: phần lớn lệnh qua giao diện Uniswap
là V4; 47/47 trade V4 thành công được decode khớp receipt.

Không hỗ trợ: swap token↔token, route ghép nhiều protocol (ví dụ V3 exact-out rồi V4), và các
DEX khác.

Thêm DEX mới: implement `DexDecoder` (decode) và `DexAdapter` (quote/swap), rồi đăng ký vào
`DecoderRegistry`. Không cần sửa strategy/risk/executor.

Bot thực thi theo **route mà ví nguồn đã dùng** (leg đi thẳng token_in → token_out), bot không
tự tìm route:
- V2 → Router02 (hàm fee-on-transfer); V3 → SwapRouter02; V4 → Universal Router v2, quote
  bằng V4Quoter.
- V4 BUY: pool ETH native dùng `SETTLE_ALL(ETH)` từ `msg.value`; pool WETH thì `WRAP_ETH`
  trước. V4 SELL: token kéo qua **Permit2**; đầu ra ETH native `TAKE_ALL`, đầu ra WETH thì
  `UNWRAP_WETH` ra ETH.

---

## Copy strategies

SELL luôn dùng chung một luật: bán `SELL_RATIO` × **số dư token của chính bot**. Không giả
định bot có cùng lượng token với ví nguồn. Số dư = 0 thì bỏ qua.

| Strategy | BUY size |
|---|---|
| `FIXED_ETH` | Luôn `FIXED_TRADE_ETH` (ví nguồn mua 10 ETH → bot mua 0.05 ETH) |
| `FIXED_USD` | `FIXED_TRADE_USD` quy đổi theo Chainlink ETH/USD (từ chối nếu giá cũ > 75 phút) |
| `RATIO` | `BUY_RATIO` × lượng ETH ví nguồn chi |
| `PROPORTIONAL` | Cùng tỉ lệ vốn: ETH ví nguồn chi / (ETH+WETH của ví nguồn trước tx) × ETH của bot |

Mọi BUY bị cắt về `MAX_TRADE_ETH`.

---

## Risk management

Trước khi thực thi (`risk/engine.rs`):
- Trùng lặp: key `chain_id + source_tx_hash`, mỗi tx nguồn chỉ được quyết định một lần.
- DEX whitelist (`SUPPORTED_DEXES`), token blacklist, zero-address.
- Cooldown theo (token, chiều): vừa mua vẫn bán được ngay.
- Token phải là contract (EOA có delegation EIP-7702 không tính).
- `MAX_GAS_GWEI` (ở mode `skip`), `MAX_TRADE_ETH` (BUY).
- Đủ ETH cho lệnh + dự trữ gas (SELL: đủ gas cho approve + swap).

Sau khi quote (`execution/executor.rs`):
- **Tx nguồn còn pending** thì được đặt đầu bundle mô phỏng, nên quote/simulate phản ánh
  state *sau* tx nguồn. Tx nguồn revert trong mô phỏng (ví dụ hết gas) thì không copy.
- **Price impact** của lệnh bot (so với quote 1/1000) > `MAX_SLIPPAGE_BPS` thì từ chối.
- SELL có giá trị > `MAX_TRADE_ETH` thì chỉ bán phần tương ứng `MAX_TRADE_ETH`.
- **Lượng thực nhận** được đo bằng mô phỏng (số dư trước/sau), nên đúng cả với token có
  thuế chuyển nhượng. `min_out = thực nhận × (1 − MAX_SLIPPAGE_BPS)`. Thực nhận = 0 thì từ chối.
- Approve + swap được simulate cùng nhau. Bất kỳ bước nào revert thì không gửi.

---

## Security considerations

- `PRIVATE_KEY` chỉ được đọc lúc startup và nằm trong `BotSigner`. `Debug` của nó chỉ in
  địa chỉ. Lỗi parse key không chứa key. Key không bị serialize, log, đưa vào metrics.
- URL RPC (thường chứa API key) bị che (`https://host/***`) trong log và error. Span
  debug/trace của Alloy transport bị ghim ở mức `info` kể cả khi `LOG_LEVEL=trace`.
- `.env` nằm trong `.gitignore` và `.dockerignore`. `.env.example` không chứa key thật.
- **Key công khai / key đã lộ:** các key dev phổ biến (ví dụ key Anvil) trên mainnet đều bị
  gắn delegation EIP-7702 "sweeper", tự chuyển đi mọi ETH nhận được. Bot kiểm tra code của
  ví bot lúc startup và **từ chối live mode** nếu có.
- Approve: bot approve `MAX` cho router Uniswap / Permit2 (contract cố định, không nâng cấp
  được), và với V4 cấp Permit2 allowance `MAX` (hết hạn tối đa) cho Universal Router v2, để
  mỗi lệnh SELL không tốn thêm một tx approve. Nếu muốn approve đúng số lượng, sửa
  `execution/allowance.rs`.
- Docker: image distroless, user `nonroot`, filesystem read-only, `cap_drop: ALL`. Biến môi
  trường (gồm `PRIVATE_KEY`) đọc được qua `docker inspect` bởi người có quyền Docker; dùng
  secret manager / orchestrator secrets nếu môi trường dùng chung.
- Không dùng `unsafe`.

---

## Tests

```bash
cargo test                                  # unit + fixture (offline)
ETH_HTTP_URL=https://... cargo test -- --ignored   # live mainnet (chỉ đọc + simulate)
```

| Bộ test | Nội dung |
|---|---|
| unit (`src/**`) | Config, decoder, strategy, risk, dedup, nonce (64 task song song), gas, executor, broadcast (ký thật bằng key test) |
| `uniswap_fixtures` | 16 tx Uniswap mainnet thật (V2/V3/UR/V4, đã đối chiếu receipt) |
| `rpc_chain_reader`, `decision_live`, `executor_live` | Đọc state, quyết định, quote/simulate trên mainnet thật |
| `broadcast_live` | Ký + gửi thật bằng key ngẫu nhiên số dư 0: node giải mã tx và từ chối vì thiếu tiền |
| `anvil_e2e` | **Vòng đời đầy đủ trên Anvil fork**, cho V2 và V4: ví nguồn swap (pending) → bot copy BUY → mined → ví nguồn bán → bot copy SELL (approve [+ Permit2] + swap, nonce liên tiếp) → mined |

`anvil_e2e` cần Foundry (`anvil` trong PATH, `~/.foundry/bin`, hoặc `ANVIL_BIN`).

---

## Troubleshooting

| Triệu chứng | Nguyên nhân / cách xử lý |
|---|---|
| `RPC must support eth_simulateV1` | Đổi `ETH_HTTP_URL` sang provider hỗ trợ `eth_simulateV1` |
| Watcher lỗi ngay lúc startup ở mode `pending` | Provider không hỗ trợ `newPendingTransactions` full body. Đổi provider hoặc dùng `COPY_MODE=confirmed` |
| `chain id mismatch` | `CHAIN_ID` không khớp RPC |
| `has no router/WETH address table` | Chỉ hỗ trợ `CHAIN_ID=1` |
| `bot wallet ... has code/delegation` | Ví bot có code/delegation. Ngừng dùng key này, chuyển tiền sang ví mới |
| `trade_ignored ... V4_SWAP` | `UNISWAP_V4` chưa có trong `SUPPORTED_DEXES` (hoặc router Universal Router cũ) |
| `trade_ignored ... mixed exact-in and exact-out` | Route ghép nhiều protocol, không thực thi được |
| `trade_ignored ... token-to-token` | Swap không có phía ETH/WETH |
| `trade_rejected ... cooldown` / `duplicate` | Hành vi mong muốn, xem `TRADE_COOLDOWN_SECS` |
| `trade_rejected ... insufficient ETH` | Nạp thêm ETH cho ví bot (cần cả dự trữ gas) |
| `trade_failed stage=gas` | Gas vượt `MAX_GAS_GWEI`. Tăng trần hoặc dùng `GAS_CAP_MODE=wait` |
| `trade_failed ... price impact` | Pool mỏng so với size lệnh. Giảm size hoặc tăng `MAX_SLIPPAGE_BPS` |
| `trade_failed ... source tx reverts` | Tx nguồn sẽ thất bại (hết gas, trượt giá…), bot không copy |
| `trade_failed ... min output ... is zero` | Bot không nhận được gì trong mô phỏng (token honeypot, ví có sweeper…) |
| `trade_failed stage=receipt` | Chưa có receipt sau 180s (base fee vượt `maxFee`?). Bot resync nonce; tx kẹt không được tự thay thế |
| `blocks_skipped` / `watcher_disconnected` | Mất kết nối WS. Bot tự reconnect, backfill tối đa 32 block |
| RPC public trả lỗi lúc đọc block mới nhất | Load balancer có backend chậm block. Bot tự retry; nên dùng RPC riêng cho production |
