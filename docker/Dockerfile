# ── Stage 1: Builder (standalone — compile-only) ─────────────────────
FROM debian:bookworm-slim AS builder

# Build dependencies only
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates curl pkg-config libssl-dev build-essential \
    protobuf-compiler libprotobuf-dev \
    && rm -rf /var/lib/apt/lists/*

# Install Rust toolchain
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
ENV PATH="/root/.cargo/bin:${PATH}"

WORKDIR /build

# 1. Copy manifests only (for dependency caching)
COPY Cargo.toml Cargo.lock ./
COPY crates/jyc-types/Cargo.toml crates/jyc-types/Cargo.toml
COPY crates/jyc-utils/Cargo.toml crates/jyc-utils/Cargo.toml
COPY crates/jyc-core/Cargo.toml crates/jyc-core/Cargo.toml
COPY crates/jyc-mcp/Cargo.toml crates/jyc-mcp/Cargo.toml
COPY crates/jyc-agent/Cargo.toml crates/jyc-agent/Cargo.toml
COPY crates/jyc-services/Cargo.toml crates/jyc-services/Cargo.toml
COPY crates/jyc-channels/Cargo.toml crates/jyc-channels/Cargo.toml
COPY crates/jyc-inspect/Cargo.toml crates/jyc-inspect/Cargo.toml
COPY crates/jyc-cli/Cargo.toml crates/jyc-cli/Cargo.toml

# 2. Create empty stubs for all crates to compile dependencies
RUN for crate in jyc-types jyc-utils jyc-core jyc-mcp jyc-agent jyc-services jyc-channels jyc-inspect; do \
      mkdir -p "crates/$crate/src" && echo '' > "crates/$crate/src/lib.rs"; \
    done \
    && mkdir -p crates/jyc-cli/src && echo 'fn main() {}' > crates/jyc-cli/src/main.rs \
    && cargo build --release || true

# 3. Remove stubs and fingerprints, copy real source, build
RUN rm -rf crates/*/src target/release/deps/jyc* target/release/.fingerprint/jyc*

COPY crates/ crates/
COPY config.example.toml ./
RUN cargo build --release -p jyc-cli

# ── Stage 2: Slim runtime (minimal production image) ─────────────────
FROM debian:bookworm-slim AS slim

# Runtime dependencies only
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    curl \
    jq \
    ripgrep \
    python3 \
    python3-pip \
    libssl3 \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /opt/jyc

COPY --from=builder /build/target/release/jyc /usr/local/bin/jyc

ENTRYPOINT ["jyc"]
CMD ["monitor", "--workdir", "/opt/jyc", "--debug"]

# ── Stage 3: Full (slim + dev tools for AI agent development) ────────
FROM slim AS full

# Additional dev tools
RUN apt-get update && apt-get install -y --no-install-recommends \
    git \
    gh \
    build-essential \
    pkg-config \
    libssl-dev \
    protobuf-compiler \
    pandoc \
    inotify-tools \
    xz-utils \
    unzip \
    procps \
    && rm -rf /var/lib/apt/lists/*

# Install Rust toolchain (needed by AI agent for cargo build/test)
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
ENV PATH="/root/.cargo/bin:${PATH}"

# Install GitHub CLI
RUN curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg \
    | dd of=/usr/share/keyrings/githubcli-archive-keyring.gpg \
    && chmod go+r /usr/share/keyrings/githubcli-archive-keyring.gpg \
    && echo "deb [arch=$(dpkg --print-architecture) signed-by=/usr/share/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" \
    | tee /etc/apt/sources.list.d/github-cli.list > /dev/null \
    && apt-get update && apt-get install -y --no-install-recommends gh \
    && rm -rf /var/lib/apt/lists/*

RUN mkdir -p /root/.config/gh

# Install fnm (Fast Node Manager) and Node 22
RUN curl -fsSL https://fnm.vercel.app/install | bash -s -- --install-dir /root/.fnm && \
    export PATH="/root/.fnm:$PATH" && \
    export FNM_DIR="/root/.fnm" && \
    eval "$(fnm env --shell bash)" && \
    fnm install 22 && \
    fnm default 22 && \
    ln -sf "/root/.fnm/aliases/default" /root/.node-current
ENV FNM_DIR="/root/.fnm"
ENV PATH="/root/.node-current/bin:/root/.fnm:${PATH}"

# ── Stage 4: Production alias (backward compatible) ──────────────────
FROM full AS production
