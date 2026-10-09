# Implementation Plan

Theo đúng thứ tự PHASE trong spec. Mỗi phase kết thúc bằng `cargo fmt && cargo clippy
-- -D warnings && cargo test`, phải xanh trước khi sang phase kế.

## PHASE 1 — Bootstrap ✅ DONE
- [x] cargo init, workspace layout
- [x] Cargo.toml: chỉ dependency Phase 1 dùng (alloy 2.5, tokio, futures, dotenvy, tracing,
      tracing-subscriber, anyhow, thiserror). serde/dashmap/parking_lot/uuid thêm ở phase dùng tới.
- [x] config/mod.rs: load + validate toàn bộ env, COPY_WALLETS (trim/dedup/EIP-55/zero),
      chặn bot wallet nằm trong COPY_WALLETS, DRY_RUN chỉ nhận true/false
- [x] ethereum/provider.rs: HTTP + WS provider, check CHAIN_ID, redact URL/API key khỏi error
- [x] ethereum/signer.rs: BotSigner, Debug chỉ in address, error không chứa key
- [x] watcher: pending (full pending tx sub) + confirmed (newHeads + fetch block), reconnect
      exponential backoff, stall timeout 60s, backfill tối đa 32 block, retry block chưa sẵn
      sàng trên RPC load-balanced, graceful shutdown
- [x] tracing init theo LOG_LEVEL (alloy transport bị ghim ở info để không lộ RPC URL)
- [x] main.rs: wiring, bounded channel, SIGINT/SIGTERM, shutdown timeout 15s
- [x] 27 unit test; fmt/clippy -D warnings/test xanh; chạy live mainnet cả 2 mode

## PHASE 2 — Classification + TradeIntent + Uniswap decoding ✅ DONE
- [x] classifier.rs (SWAP/TRANSFER/APPROVE/CONTRACT_CALL/CONTRACT_DEPLOY/UNKNOWN)
- [x] models/trade_intent.rs (TradeSide, SwapKind, RouteLeg, ETH/WETH theo §5)
- [x] decoder: V2 Router02 (9 hàm swap), V3 SwapRouter + SwapRouter02 (multicall,
      unwrap/sweep/selfPermit), Universal Router v1/v1.2/v2 (V2/V3 commands); registry
      chỉ đăng ký DEX trong SUPPORTED_DEXES
- [x] lib + bin; watcher -> decoder -> (sink tạm, Phase 3 thay) qua mpsc bounded
- [x] 50 unit test + 4 integration test trên 12 fixture tx mainnet thật
- [x] Kiểm chứng: decode 632 tx router thật, 385/385 trade thành công khớp receipt log

## PHASE 3 — Strategy + Risk ✅ DONE
- [x] strategy: FIXED_ETH, FIXED_USD (Chainlink ETH/USD, chặn giá stale/<=0), RATIO,
      PROPORTIONAL (số dư ETH+WETH của source trước tx); BUY cap MAX_TRADE_ETH; SELL =
      SELL_RATIO x số dư token của bot, số dư 0 => bỏ qua
- [x] risk/engine.rs: precheck (dedup, DEX whitelist, zero-address, blacklist, cooldown
      theo token+side) + check (invalid token, MAX_GAS_GWEI, MAX_TRADE_ETH, đủ ETH cho
      trade + gas reserve)
- [x] storage: DedupStore trait + InMemoryDedup (DashMap, TTL 24h)
- [x] ethereum/chain_state.rs: ChainReader + RpcChainReader (EIP-7702 aware)
- [x] decision.rs: stage tuần tự, 1 batch RPC song song / quyết định
- [x] config: FIXED_TRADE_USD, TOKEN_BLACKLIST, TRADE_COOLDOWN_SECS
- [x] 81 unit + 4 fixture test; 4 test live mainnet (`--ignored`, cần ETH_HTTP_URL)
- [x] MAX_SLIPPAGE_BPS/price impact + MAX_TRADE_ETH cho SELL: làm ở executor (Phase 4)

## PHASE 4 — DEX Executor + allowance + simulation ✅ DONE
- [x] dex/: DexAdapter (V2 Router02 FoT-safe, V3 QuoterV2 + SwapRouter02), thuần, không RPC
- [x] execution/simulator.rs: eth_simulateV1, decode revert (Error/Panic/custom selector)
- [x] execution/allowance.rs: approve(MAX), reset về 0 khi cần (USDT)
- [x] execution/executor.rs: route → quote (sau source nếu pending) → price impact ≤
      MAX_SLIPPAGE_BPS → cap SELL theo MAX_TRADE_ETH → đo lượng thực nhận (FoT) → min_out
      → approvals + swap đã simulate; N worker
- [x] main: kiểm tra eth_simulateV1 lúc startup; DRY_RUN=false bị từ chối tới Phase 5
- [x] 103 unit + 4 fixture test; live: 3 test executor (BUY V2/V3, SELL có approve) +
      6/6 BUY fixture thật simulate OK (kể cả token thuế ~5%)

## PHASE 5 — Signing + Nonce + Broadcast + Receipt ✅ DONE
- [x] ethereum/nonce.rs: lease (reserve + gửi trong một lock: không trùng, không gap),
      pending/confirmed, partial send, resync; test 64 task song song
- [x] execution/gas.rs: GasStrategy CappedEip1559, GAS_CAP_MODE=skip|wait
- [x] execution/broadcast.rs: ký EIP-1559, gửi approve+swap theo nonce, receipt 180s
- [x] executor stage: gas → plan → [live] broadcast → receipt; log latency đủ §18
- [x] main: DRY_RUN=false được bật; risk engine bỏ check gas khi wait mode
- [x] 120 unit + 4 fixture; 8 test live (`--ignored`), gồm ký + gửi thật lên mainnet
      bằng key rỗng (node giải mã OK, từ chối vì insufficient funds)
- [ ] Chưa test được vòng đời tx thành công thật (mined + receipt) — cần Anvil fork (Phase 6)

## PHASE 6 — Metrics, Docker, Tests, Docs ✅ DONE
- [x] Latency đủ §18 trong `trade_broadcasted`; dry run có `detection_to_simulated_ms`
- [x] Dockerfile multi-stage (distroless nonroot, 45.8MB), docker-compose (read-only,
      cap_drop ALL, grace 30s), .dockerignore; đã build + chạy container thật
- [x] `tests/anvil_e2e.rs`: vòng đời đầy đủ trên Anvil fork (pending source → copy BUY →
      mined → copy SELL approve+swap → mined), pass ổn định 3/3
- [x] README đủ §26; `#![forbid(unsafe_code)]`
- [x] Bug tìm ra nhờ e2e: gas limit tính từ gasUsed sau refund => SELL hết gas on-chain.
      Sửa: dùng `maxUsedGas` (peak trước refund) × 1.3
- [x] An toàn: live mode từ chối ví bot có code/delegation (key lộ, sweeper EIP-7702)

## Giả định đã chốt (ghi rõ để không ngầm quyết định)
1. "Validate tất cả Ethereum address khi start" áp dụng cho: tất cả phần tử COPY_WALLETS,
   địa chỉ derive từ PRIVATE_KEY (qua chữ ký, không áp dụng validate riêng), và các router
   address nếu được đưa vào config/hardcode theo CHAIN_ID.
2. Router address của Uniswap V2/V3/Universal Router: hardcode theo CHAIN_ID (mainnet=1)
   trong code (không phải env) vì đây là hằng số công khai, không phải secret/config user.
   Nếu cần chain khác sẽ mở rộng bảng hằng số này.
3. Phase 1 chưa có decoder: watcher đã forward thật qua bounded channel, đầu nhận tạm thời là
   `drain_watched` trong main.rs (log + queue latency). Phase 2 thay nó bằng decoder stage.
4. Pending mode dùng `newPendingTransactions` dạng full body (không dùng hash-only + fetch
   từng tx vì tốn 1 RPC/tx mempool). RPC không hỗ trợ => bot fail ngay lúc startup.

## PHASE 7 — Uniswap V4 ✅ DONE
- [x] Decode UR v2 `V4_SWAP`: exact in/out, single/multi-hop, SETTLE/TAKE variants, native ETH,
      hooks, dynamic fee; `UNISWAP_V4` trong SUPPORTED_DEXES
- [x] Sửa decoder UR: WRAP_ETH hoàn tiền sau swap, UNWRAP_WETH về router, unwrap hoàn tiền
- [x] Executor V4 qua Universal Router + V4Quoter; Permit2 approvals (token→Permit2→UR)
- [x] Kiểm chứng: 116 tx V4 mainnet → 53 trade, 47/47 thành công khớp receipt, 0 hồi quy
      V2/V3; 3 route V4 thật (hook, dynamic fee, 2-hop) simulate OK; e2e Anvil V4 BUY+SELL
- [x] 136 unit/fixture + 12 test live
