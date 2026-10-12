# syntax=docker/dockerfile:1
#
# One image per chain from the same Dockerfile (see docker-compose.yml):
#   ETH: --build-arg BIN=eth-copy-trade --build-arg FEATURES=--no-default-features
#        (ETH-only build: the Solana/gRPC crates are not even compiled)
#   SOL: --build-arg BIN=sol-copy-trade

# ---- build ----
FROM rust:1-slim-bookworm AS builder
ARG BIN=eth-copy-trade
ARG FEATURES=--no-default-features
WORKDIR /app

# Build dependencies first so source edits don't re-download/re-compile them.
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src/bin \
    && echo 'fn main() {}' > src/bin/eth-copy-trade.rs \
    && echo 'fn main() {}' > src/bin/sol-copy-trade.rs \
    && touch src/lib.rs \
    && cargo build --release --locked --bin "$BIN" $FEATURES \
    && rm -rf src

COPY src ./src
# COPY keeps host mtimes; touch so cargo rebuilds the real crate over the dummy one.
RUN touch src/lib.rs src/bin/*.rs \
    && cargo build --release --locked --bin "$BIN" $FEATURES \
    && cp "target/release/$BIN" /copy-bot

# ---- runtime ----
# glibc + CA certificates (TLS uses the system trust store), no shell, non-root user.
FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=builder /copy-bot /usr/local/bin/copy-bot
USER nonroot:nonroot
ENTRYPOINT ["/usr/local/bin/copy-bot"]
