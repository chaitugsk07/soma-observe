# soma-observe — multi-stage Dockerfile
#
# Build context: the soma-platform PARENT directory (one level above soma-observe/).
# Reason: soma-observe has path dependencies on ../soma-infra, ../soma-schema,
# and the dashboard depends on ../soma-ui — all siblings under soma-platform/.
#
# Build command (run from soma-platform/):
#   docker build -f soma-observe/Dockerfile -t soma-observe .
#
# Or with a tag for release:
#   docker build -f soma-observe/Dockerfile -t ghcr.io/chaitugsk07/soma-observe:latest .

# ---------------------------------------------------------------------------
# Stage 1 — Rust + Trunk builder
# ---------------------------------------------------------------------------
FROM rust:1.82-slim AS builder

# Install system dependencies for musl-libc linking and Trunk tooling.
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config \
    libssl-dev \
    musl-tools \
    curl \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Add build targets.
RUN rustup target add wasm32-unknown-unknown x86_64-unknown-linux-musl

# Install trunk (Leptos/WASM build tool).
RUN curl -fsSL https://github.com/thedodd/trunk/releases/download/v0.21.1/trunk-x86_64-unknown-linux-gnu.tar.gz \
    | tar -xzC /usr/local/bin

WORKDIR /build

# Copy sibling repos that soma-observe path-depends on.
# These are all siblings under the soma-platform/ parent context.
COPY soma-infra/   soma-infra/
COPY soma-schema/  soma-schema/
COPY soma-ui/      soma-ui/
COPY soma-observe/ soma-observe/

# ---------------------------------------------------------------------------
# Build step 1: Leptos dashboard → dashboard/dist/
# rust-embed bakes dashboard/dist/ into the binary at compile time, so trunk
# MUST run before `cargo build`. Without this, cargo build on portal.rs fails.
# ---------------------------------------------------------------------------
WORKDIR /build/soma-observe/dashboard
RUN trunk build --release

# ---------------------------------------------------------------------------
# Build step 2: Rust binary (static, musl).
# dashboard/dist/ is now populated; rust-embed will find it.
# ---------------------------------------------------------------------------
WORKDIR /build/soma-observe
ENV CARGO_NET_GIT_FETCH_WITH_CLI=true
RUN cargo build --release --target x86_64-unknown-linux-musl

# ---------------------------------------------------------------------------
# Stage 2 — Minimal runtime image
# ---------------------------------------------------------------------------
FROM gcr.io/distroless/static-debian12:nonroot AS runtime

COPY --from=builder /build/soma-observe/target/x86_64-unknown-linux-musl/release/soma-observe /soma-observe

# OTLP/HTTP ingest + OTel-faithful JSON query API
EXPOSE 4318

ENTRYPOINT ["/soma-observe"]
