# ── Build stage ─────────────────────────────────────────────────────────────
FROM rust:1.89-slim AS builder
WORKDIR /app

# Cache the dependency layer: build with stub sources first so `cargo build`
# only re-runs for dependency changes, not every source edit.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src \
    && echo 'fn main() {}' > src/main.rs \
    && touch src/lib.rs \
    && cargo build --release \
    && rm -rf src

COPY src ./src
RUN touch src/main.rs src/lib.rs && cargo build --release

# ── Runtime stage ───────────────────────────────────────────────────────────
# distroless/cc: glibc + CA certs, no shell, ~20 MB
FROM gcr.io/distroless/cc-debian12
COPY --from=builder /app/target/release/gateway /usr/local/bin/gateway

# Containers must bind beyond loopback; auth tokens are still required for
# non-localhost binds (enforced at startup).
ENV GATEWAY_BIND=0.0.0.0:4000
EXPOSE 4000

# Mount your config at /etc/gateway/gateway.yaml
ENTRYPOINT ["/usr/local/bin/gateway", "--config", "/etc/gateway/gateway.yaml"]
