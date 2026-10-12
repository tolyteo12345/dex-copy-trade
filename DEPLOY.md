# Hướng dẫn setup & deploy

Tài liệu này đi từng bước: từ máy dev đến chạy production trên server. Ý nghĩa từng biến
môi trường, strategy và risk xem [README.md](README.md). Thiết kế chi tiết xem
[ARCHITECTURE.md](ARCHITECTURE.md).

Repo có hai bot, **mỗi chain một process / một container**: `eth-copy-trade` (service
`eth-bot`) và `sol-copy-trade` (service `sol-bot`, pump.fun). Có thể chạy một hoặc cả hai.
Các mục dưới mô tả bot ETH; phần riêng của bot SOL có đánh dấu **[SOL]**, chi tiết ở
[SOLANA.md](SOLANA.md).

> **`DRY_RUN=false` ký và gửi giao dịch bằng tiền thật.** Luôn đi đủ các bước dry run ở mục 4
> trước khi bật live.

---

## 1. Chuẩn bị

| Thứ cần có | Ghi chú |
|---|---|
| Ví bot **riêng** | Tạo key mới, không dùng lại ví cá nhân hay key dev (key Anvil/Hardhat trên mainnet đều bị gắn delegation "sweeper"). Chỉ nạp số ETH chấp nhận mất |
| RPC mainnet **WebSocket** | Cho `eth_subscribe`. Mode `pending` cần `newPendingTransactions` trả **full tx body** (không phải mọi provider có) |
| RPC mainnet **HTTP** | Bắt buộc hỗ trợ `eth_simulateV1` (geth, reth, Nethermind, Alchemy…). Bot kiểm tra lúc startup |
| Danh sách ví cần copy | `COPY_WALLETS`, không được chứa ví bot |
| Telegram bot (tuỳ chọn) | Token từ @BotFather + chat id |

**[SOL]** Cần thêm: endpoint **Yellowstone gRPC** (`SOL_GRPC_URL` + `SOL_GRPC_X_TOKEN`), RPC
HTTP mainnet-beta (`SOL_RPC_URL`), và ví Solana riêng (`SOL_PRIVATE_KEY`). Xem
[SOLANA.md → Yêu cầu](SOLANA.md#yêu-cầu).

Nên dùng RPC riêng (trả phí hoặc node tự chạy) cho production. RPC public như publicnode là
load balancer, backend có thể chậm block và giới hạn rate.

Kiểm tra nhanh RPC HTTP có `eth_simulateV1`:

```bash
curl -s -X POST "$ETH_HTTP_URL" -H 'content-type: application/json' --data \
  '{"jsonrpc":"2.0","id":1,"method":"eth_simulateV1","params":[{"blockStateCalls":[{"calls":[]}]},"latest"]}'
# Có "result" là được; "method not found" là RPC không dùng được
```

---

## 2. Setup máy dev

Yêu cầu: Rust stable ≥ 1.85, Docker (tuỳ chọn), Foundry (tuỳ chọn, cho test Anvil).

```bash
git clone <repo> && cd dex-copy-trade
cp .env.example .env            # sửa .env, KHÔNG commit (đã có trong .gitignore)
cargo build --release
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test
```

Test cần mạng (chỉ đọc state / simulate, không tiêu tiền):

```bash
ETH_HTTP_URL=https://ethereum-rpc.publicnode.com cargo test -- --ignored
```

---

## 3. Cấu hình `.env`

Tối thiểu cần sửa so với `.env.example`:

```dotenv
ETH_WS_URL=wss://...
ETH_HTTP_URL=https://...
PRIVATE_KEY=0x...                # key ví bot
COPY_WALLETS=0xabc...,0xdef...
DRY_RUN=true                     # giữ true cho tới bước 5
```

Gợi ý cho lần chạy đầu: size nhỏ (`FIXED_TRADE_ETH=0.01`, `MAX_TRADE_ETH=0.02`), giữ
`MAX_SLIPPAGE_BPS=300`, `GAS_CAP_MODE=skip`. Mọi giá trị được validate lúc startup, sai là bot
dừng ngay với thông báo rõ ràng.

Ví bot vẫn phải có ETH ngay cả khi dry run, vì risk check kiểm số dư (lệnh + dự trữ gas).

**[SOL]** Điền phần `SOL_*` trong cùng file `.env` (`SOL_RPC_URL`, `SOL_GRPC_URL`,
`SOL_PRIVATE_KEY`, `SOL_COPY_WALLETS`). `SOL_DRY_RUN` độc lập với `DRY_RUN`. Ví bot SOL cũng
phải có SOL khi dry run (simulate cần số dư thật).

---

## 4. Dry run

```bash
cargo run --release --bin eth-copy-trade      # [SOL] --bin sol-copy-trade
# hoặc
docker compose up --build eth-bot             # [SOL] sol-bot
```

Checklist khi khởi động, theo thứ tự log:

1. `event=startup`: kiểm tra `bot_wallet`, `copy_wallets`, `strategy`, `dry_run=true`. URL
   RPC hiện dạng `https://host/***` (đã che API key).
2. Không có lỗi `startup RPC check failed` (sai chain id / RPC không kết nối được) hay
   `RPC must support eth_simulateV1`.
3. Không có `bot_wallet_has_code`. Nếu có: key đã lộ, **bỏ ví này**.

Để chạy đủ lâu (vài giờ đến vài ngày) và đọc:

- `trade_decoded` → `trade_approved` → `trade_simulated`: lệnh copy *sẽ* như thế nào
  (`min_out`, price impact, thuế token, gas).
- `trade_rejected` / `trade_failed` / `trade_ignored`: lý do bị bỏ. Bảng giải thích ở
  [README → Troubleshooting](README.md#troubleshooting).

Chỉ chuyển sang live khi tỉ lệ `trade_simulated` hợp lý và các lý do reject đã hiểu rõ.

**[SOL]** Checklist tương tự: `event=startup chain=solana dry_run=true` →
`sol_watcher_subscribed` (gRPC kết nối được) → `trade_decoded` / `tx_skipped` →
`trade_approved` → `trade_planned` (`received`, `min_out`, `cu_limit`). Lỗi startup
`SOL_RPC_URL is not mainnet-beta` nghĩa là RPC sai cluster.

---

## 5. Deploy production

Chọn **một** trong hai cách. Cả hai đều chạy đúng **một instance** cho mỗi ví bot (xem mục 6).

### 5a. Docker Compose (khuyến nghị)

Image distroless, user `nonroot`, filesystem read-only, `cap_drop: ALL`, khoảng 46MB. Compose
đã cấu hình `restart: unless-stopped`, `stop_grace_period: 30s` và xoay vòng log (5 × 50MB).

```bash
# Trên server (Linux, có Docker + plugin compose)
git clone <repo> && cd dex-copy-trade
cp .env.example .env && chmod 600 .env     # điền giá trị thật
docker compose up -d --build eth-bot         # [SOL] sol-bot; bỏ tên service = chạy cả hai
docker compose logs -f eth-bot
```

Bật live: sửa `DRY_RUN=false` (**[SOL]** `SOL_DRY_RUN=false`) trong `.env` rồi
`docker compose up -d <service>` (tạo lại container để đọc env mới). Log phải có
`event=live_mode`.

Nếu muốn build image ở máy khác rồi đẩy lên server:

```bash
docker build --build-arg BIN=eth-copy-trade --build-arg FEATURES=--no-default-features \
  -t <registry>/dex-copy-trade-eth:<tag> .
docker build --build-arg BIN=sol-copy-trade --build-arg FEATURES= \
  -t <registry>/dex-copy-trade-sol:<tag> .
docker push <registry>/dex-copy-trade-eth:<tag>   # và -sol
# trên server: đổi `image:` của từng service trong docker-compose.yml thành tag trên, bỏ `build:`
```

### 5b. Binary + systemd

```bash
cargo build --release --locked --bins          # chỉ ETH: --no-default-features --bin eth-copy-trade
sudo useradd --system --no-create-home copybot
sudo install -m 755 target/release/eth-copy-trade target/release/sol-copy-trade /usr/local/bin/
sudo install -d -m 700 -o copybot /etc/dex-copy-trade
sudo install -m 600 -o copybot .env /etc/dex-copy-trade/env
```

Mỗi chain một unit. `/etc/systemd/system/eth-copy-trade.service` (**[SOL]** copy thành
`sol-copy-trade.service`, đổi `Description` và `ExecStart=/usr/local/bin/sol-copy-trade`):

```ini
[Unit]
Description=eth-copy-trade bot
After=network-online.target
Wants=network-online.target

[Service]
User=copybot
EnvironmentFile=/etc/dex-copy-trade/env
ExecStart=/usr/local/bin/eth-copy-trade
Restart=on-failure
RestartSec=10
KillSignal=SIGTERM
TimeoutStopSec=30
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now eth-copy-trade      # [SOL] sol-copy-trade
journalctl -u eth-copy-trade -f
```

`EnvironmentFile` không hiểu dấu nháy hay comment cuối dòng như dotenv: giữ mỗi dòng dạng
`KEY=value` đơn giản. Bot không cần file `.env` trong thư mục làm việc khi env đã có sẵn.

---

## 6. Vận hành

**Chỉ một instance cho mỗi `PRIVATE_KEY`.** Nonce manager và dedup nằm trong bộ nhớ của
process. Hai instance cùng key sẽ tranh nonce và copy trùng lệnh. **[SOL]** Tương tự với
`SOL_PRIVATE_KEY`: hai instance sẽ copy trùng.

**Restart / update.** Bot nhận `SIGTERM`, ngừng nhận lệnh mới, đợi lệnh đang chạy tối đa 15s
rồi thoát (compose/systemd ở trên cho 30s). Sau khi restart:
- Dedup bị reset (in-memory). Không vấn đề với mode `pending` vì bot chỉ thấy tx mới.
- Backfill (≤ 32 block) chỉ có khi WS reconnect trong cùng process. Tx của ví nguồn trong
  lúc bot tắt **không** được copy.
- Nonce được đọc lại từ chain.

Update phiên bản:

```bash
git pull
docker compose up -d --build                 # Docker
# hoặc: cargo build --release --locked --bins && sudo install ... && sudo systemctl restart eth-copy-trade sol-copy-trade
```

Rollback: checkout commit/tag cũ rồi build lại, hoặc trỏ `image:` về tag cũ.

**Theo dõi.** Bot không có endpoint health/metrics; theo dõi qua log và Telegram:

| Cần để ý | Ý nghĩa / hành động |
|---|---|
| `trade_broadcasted` không theo sau bởi `trade_confirmed` | Xem `trade_failed stage=receipt`: tx có thể kẹt do base fee vượt `maxFee`. Bot không tự replace-by-fee, phải xử lý tay |
| `watcher_disconnected`, `blocks_skipped` lặp lại | WS không ổn định, cân nhắc đổi RPC |
| `trade_rejected ... insufficient ETH` | Nạp thêm ETH cho ví bot |
| Container/service restart liên tục | Lỗi startup (config, RPC, ví có code). Xem dòng log cuối |

**Dừng khẩn cấp.** `docker compose stop` hoặc `systemctl stop eth-copy-trade sol-copy-trade`. Để chắc chắn
bot không gửi gì khi bật lại, đặt `DRY_RUN=true` trước khi start. Allowance `MAX` đã cấp cho
router Uniswap / Permit2 vẫn còn; muốn thu hồi thì revoke thủ công từ ví bot.

---

## 7. Bảo mật khi deploy

- `.env` / file env: quyền `600`, chủ sở hữu là user chạy bot, không commit, không copy vào
  image (`.dockerignore` đã loại).
- Biến môi trường (gồm `PRIVATE_KEY`) đọc được qua `docker inspect` hoặc `/proc` bởi người có
  quyền root/Docker trên server. Server dùng chung thì dùng secret manager của orchestrator.
- Không bật `LOG_LEVEL=trace` toàn cục trên production nếu log được gửi ra ngoài. API key
  trong URL RPC đã được che, nhưng log trace rất lớn.
- Định kỳ rút lợi nhuận ra ví khác, chỉ để lại vốn vận hành trong ví bot.
