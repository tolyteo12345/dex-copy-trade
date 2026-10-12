# Solana / pump.fun — `sol-copy-trade`

Bot copy trade trên Solana cho **pump.fun**: bonding curve (trước khi migrate) và **PumpSwap**
(AMM sau khi migrate). Chạy thành **process riêng** (`sol-copy-trade`), tách hẳn khỏi bot ETH
(`eth-copy-trade`): runtime, kết nối RPC, cấu hình (`SOL_*`) và container đều riêng. Hai bot
chỉ dùng chung code trong `src/common/` (Telegram, dedup, parse env, tracing). Bot SOL bận
hoặc lỗi không làm chậm bot ETH.

> **`SOL_DRY_RUN=false` ký và gửi giao dịch bằng SOL thật.** Mặc định `true`, và độc lập với
> `DRY_RUN` của ETH: bật live bên ETH không làm SOL live.

## Yêu cầu

| Thứ cần có | Ghi chú |
|---|---|
| Yellowstone (Geyser) gRPC | `SOL_GRPC_URL` (+ `SOL_GRPC_X_TOKEN`). Helius, Triton, QuickNode hoặc node riêng. Solana không có mempool public: gRPC ở commitment `processed` là cách thấy tx sớm nhất |
| RPC HTTP mainnet-beta | `SOL_RPC_URL`: `getMultipleAccounts`, `simulateTransaction`, `sendTransaction`. Bot kiểm tra genesis hash lúc startup |
| Ví bot riêng | `SOL_PRIVATE_KEY`: base58 64 byte (export từ Phantom) hoặc mảng JSON của `solana-keygen`. Chỉ nạp số SOL chấp nhận mất |

## Pipeline

```
watcher   gRPC: tx thành công, không phải vote, có nhắc tới ví trong SOL_COPY_WALLETS → SourceTx
decoder   event của pump (TradeEvent / BuyEvent / SellEvent) → SolTradeIntent
decision  precheck (không RPC) → 1× getMultipleAccounts → sizing + risk → SolCopyTrade
executor  build tx → simulateTransaction (đo lượng thực nhận) → [live] ký + gửi → theo dõi
```

Decision chạy tuần tự (giống ETH) để cooldown và kiểm số dư không bị race. Executor chạy song
song `SOL_EXECUTOR_WORKERS`. Solana không có nonce nên không cần lease như ETH.

## Nhận diện lệnh của ví nguồn

Decoder đọc **event** do chính chương trình pump phát ra (self-CPI của Anchor), không đọc
instruction. Mọi phiên bản instruction (legacy, `_v2`, `_v3`, exact-in/out) và mọi router
(Jupiter, bot trading) đều phát ra cùng các event này, nên pump ra instruction mới thì decoder
không phải sửa.

Lệnh được copy khi:
- Ví theo dõi là **người ký** tx (ví chỉ nhận transfer thì bỏ qua).
- Event có `user` là ví đó. Nếu router trade qua PDA của nó thì `user` là PDA; khi đó vẫn
  tính nếu số dư token của ví (mint đó) thay đổi đúng chiều mua/bán.
- Cả tx chỉ có **một** (venue, chiều, mint). Nhiều lần khớp của cùng một lệnh được cộng dồn.

Bị bỏ qua (log `tx_skipped` kèm lý do): mua và bán trong cùng tx, multi-hop hoặc token↔token,
pool PumpSwap đảo chiều (base là WSOL), coin không ghép với SOL.

## Bot build lệnh thế nào

Bot luôn tự build bằng instruction mới nhất, số account cố định (17 account, không có account
tuỳ chọn), theo [pump-public-docs](https://github.com/pump-fun/pump-public-docs):

| Venue | BUY | SELL |
|---|---|---|
| Bonding curve | `buy_exact_quote_in_v3` (tiêu đúng N lamports) | `sell_v3` |
| PumpSwap | `buy_exact_quote_in_v2` | `sell_v2` |

- BUY tạo ATA của token trước (idempotent), vì v3/v2 yêu cầu ATA đã tồn tại.
- PumpSwap: SOL được wrap vào ATA WSOL của bot rồi đóng ATA ở cuối tx. Bot luôn trả và nhận
  **SOL native**.
- Coin "cashback" không trade được bằng v3/v2 (pump từ chối), nên bị bỏ qua ở decision.
- Account được kiểm offline bằng tx thật (`tests/sol_pump_fixtures.rs`) và bằng simulate trên
  mainnet (`tests/sol_live.rs`).

## Quy tắc an toàn (giống bot ETH)

- **Simulate trước khi gửi.** Tx gửi đi giống hệt tx đã simulate, chỉ khác `min_out` và
  compute-unit limit. Simulate lỗi thì không gửi.
- **`min_out`** = lượng *thực nhận* trong simulation × (1 − `SOL_MAX_SLIPPAGE_BPS`). Không lấy
  từ quote, không copy min-out của ví nguồn.
  - BUY: chênh lệch số dư ATA token của bot (trước/sau).
  - SELL: chênh lệch **tổng** lamports của ví bot và ATA WSOL. Wrap/unwrap chỉ chuyển SOL giữa
    hai account này, nên tổng thay đổi đúng bằng tiền nhận − phí − rent mới. Con số này hơi
    thấp hơn thực tế, tức là theo hướng an toàn.
- **Compute-unit limit** = unit tiêu thụ khi simulate × 1.3. Simulate với limit 400k CU (lệnh
  pump dùng khoảng 50–80k CU).
- **Ví bot**: phải thuộc System Program. Nếu ví bị một chương trình sở hữu thì live mode từ
  chối chạy.
- **Secret**: `SOL_PRIVATE_KEY` chỉ nằm trong `SolSigner` (Debug chỉ in pubkey).
  `SOL_GRPC_X_TOKEN` và URL được che trong log và lỗi.

## Biến môi trường

| Biến | Mặc định | Ý nghĩa |
|---|---|---|
| `SOL_RPC_URL` | bắt buộc | JSON-RPC HTTP(S) mainnet-beta |
| `SOL_GRPC_URL` | bắt buộc | Yellowstone gRPC, `http(s)://` |
| `SOL_GRPC_X_TOKEN` | trống | Token xác thực gRPC (header `x-token`) |
| `SOL_PRIVATE_KEY` | bắt buộc | Keypair ví bot |
| `SOL_COPY_WALLETS` | bắt buộc | Danh sách ví cần copy (base58, cách nhau dấu phẩy), không được chứa ví bot |
| `SOL_COPY_STRATEGY` | `FIXED_SOL` | `FIXED_SOL` hoặc `RATIO` |
| `SOL_FIXED_TRADE_SOL` | `0.1` | SOL mỗi lệnh BUY (FIXED_SOL), phải ≤ `SOL_MAX_TRADE_SOL` |
| `SOL_BUY_RATIO` | `0.10` | RATIO: tỉ lệ so với số SOL ví nguồn đã chi |
| `SOL_SELL_RATIO` | `1.0` | SELL: tỉ lệ số dư token *của bot* được bán |
| `SOL_MAX_TRADE_SOL` | `1` | Trần mỗi lệnh BUY |
| `SOL_MAX_SLIPPAGE_BPS` | `500` | Slippage cho `min_out` |
| `SOL_PRIORITY_FEE_MICROLAMPORTS` | `200000` | Giá compute unit (priority fee) |
| `SOL_FEE_RESERVE_SOL` | `0.01` | SOL luôn giữ lại cho phí và rent |
| `SOL_DRY_RUN` | `true` | Chỉ nhận `true`/`false` |
| `SOL_VENUES` | `PUMP_FUN,PUMP_SWAP` | Venue được phép copy |
| `SOL_TOKEN_BLACKLIST` | trống | Mint không bao giờ trade |
| `SOL_TRADE_COOLDOWN_SECS` | `30` | Khoảng cách tối thiểu giữa hai lệnh copy cùng mint + chiều |
| `SOL_CHANNEL_CAPACITY` / `SOL_EXECUTOR_WORKERS` | `1024` / `4` | Kích thước pipeline |

`TELEGRAM_BOT_TOKEN`, `TELEGRAM_CHAT_ID`, `LOG_LEVEL` dùng chung với bot ETH.

## Chạy

```bash
cargo run --release --bin sol-copy-trade
# hoặc
docker compose up -d --build sol-bot && docker compose logs -f sol-bot
```

Log chính: `startup` → `sol_watcher_subscribed` → `trade_decoded` / `tx_skipped` →
`trade_approved` / `trade_rejected` → `trade_planned` (có `received`, `min_out`, `cu_limit`)
→ [live] `trade_sent` → `trade_confirmed` / `trade_failed`.

## Test

```bash
cargo test                                                       # unit + fixture (offline)
SOL_RPC_URL=https://solana-rpc.publicnode.com \
  cargo test --test sol_live -- --ignored --nocapture            # mainnet: đọc + simulate
```

| Bộ test | Nội dung |
|---|---|
| unit (`src/sol/**`) | Config, signer, layout account, event, build instruction, decision (mock RPC), executor (min_out, CU, dry run, không gửi khi simulate lỗi), chuyển đổi gRPC → SourceTx |
| `sol_pump_fixtures` | 13 tx mainnet thật (curve trực tiếp và qua router, PumpSwap, các trường hợp bị bỏ qua). Account bot build được so với instruction `buy_v3` / `sell_v3` / `sell_v2` thật |
| `sol_live` | Lấy một ví vừa mua trên pump, chạy decoder + DecisionStage thật, rồi simulate BUY và SELL do bot build trên mainnet (curve và PumpSwap). Không gửi gì |

## Giới hạn đã biết

- Không backfill: tx chạy trong lúc gRPC mất kết nối hoặc bot tắt thì không được copy.
- Bot copy *sau* khi tx nguồn đã chạy (`processed`), nên luôn mua sau ví nguồn.
- Không dùng Jito. Tx gửi qua `sendTransaction` thường, RPC tự gửi lại cho tới khi blockhash
  hết hạn (khoảng 60–90s). Không tự tăng phí hay gửi lại. Phù hợp với volume nhỏ.
- `PreBalances` đọc ở decision, simulate chạy sau đó vài chục ms. Nếu một lệnh khác của bot
  land giữa hai thời điểm này, phép đo có thể lệch. Lệch xuống thì `min_out` thấp hơn (an
  toàn). Lệch lên thì tx revert (mất phí, không mất vốn).
- Dedup lưu in-memory (mất khi restart), giống ETH.
- Không hỗ trợ: coin cashback, coin ghép USDC hoặc ghép pump coin khác, pool PumpSwap đảo
  chiều, multi-hop. Không phát hiện được hook chặn bán.

## Troubleshooting

| Triệu chứng | Nguyên nhân / cách xử lý |
|---|---|
| `SOL_RPC_URL is not mainnet-beta` | RPC trỏ devnet hoặc testnet |
| `sol_watcher_disconnected` lặp lại | Sai `SOL_GRPC_URL` / `SOL_GRPC_X_TOKEN`, hoặc provider không bật Yellowstone |
| `tx_skipped reason=no watched wallet signed` | Ví chỉ nhận token hoặc SOL, không tự trade (bình thường) |
| `trade_rejected ... bonding curve complete` | Coin đang migrate sang PumpSwap |
| `trade_failed stage=plan ... simulation failed` | Xem log chương trình trong lỗi (slippage, hết token, coin bị hạn chế…) |
| `trade_failed stage=confirm ... blockhash expired` | Tx không land kịp: tăng `SOL_PRIORITY_FEE_MICROLAMPORTS` |
| `insufficient SOL` | Nạp thêm SOL (lệnh + `SOL_FEE_RESERVE_SOL`) |
