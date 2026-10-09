# syntax=docker/dockerfile:1

# ---- build ----
FROM rust:1-slim-bookworm AS builder
WORKDIR /app

# Build dependencies first so source edits don't re-download/re-compile them.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src \
    && echo 'fn main() {}' > src/main.rs \
    && touch src/lib.rs \
    && cargo build --release --locked \
    && rm -rf src

COPY src ./src
# COPY keeps host mtimes; touch so cargo rebuilds the real crate over the dummy one.
RUN touch src/main.rs src/lib.rs && cargo build --release --locked

# ---- runtime ----
# glibc + CA certificates (TLS uses the system trust store), no shell, non-root user.
FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=builder /app/target/release/dex-copy-trade /usr/local/bin/dex-copy-trade
USER nonroot:nonroot
ENTRYPOINT ["/usr/local/bin/dex-copy-trade"]
