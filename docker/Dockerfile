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

# Copy full source
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
COPY config.example.toml ./

# Build with cached cargo registry and target directory
RUN --mount=type=cache,target=/root/.cargo/registry \
    --mount=type=cache,target=/root/.cargo/git \
    --mount=type=cache,target=/build/target \
    cargo build --release -p jyc-cli \
    && cp target/release/jyc /usr/local/bin/jyc

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

COPY --from=builder /usr/local/bin/jyc /usr/local/bin/jyc

ENTRYPOINT ["jyc"]
CMD ["monitor", "--workdir", "/opt/jyc", "--debug"]

# ── Stage 3: Full (slim + dev tools for AI agent development) ────────
FROM --platform=linux/amd64 ccr.ccs.tencentyun.com/jiny/jyc:slim AS full

# Install all dev tools in a single layer to minimize export time
RUN apt-get update && apt-get install -y --no-install-recommends \
        git build-essential pkg-config libssl-dev \
        protobuf-compiler pandoc inotify-tools xz-utils unzip procps \
    && rm -rf /var/lib/apt/lists/* \
    && curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y \
    && curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg \
       | dd of=/usr/share/keyrings/githubcli-archive-keyring.gpg \
    && chmod go+r /usr/share/keyrings/githubcli-archive-keyring.gpg \
    && echo "deb [arch=$(dpkg --print-architecture) signed-by=/usr/share/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" \
       | tee /etc/apt/sources.list.d/github-cli.list > /dev/null \
    && apt-get update && apt-get install -y --no-install-recommends gh \
    && rm -rf /var/lib/apt/lists/* \
    && mkdir -p /root/.config/gh \
    && curl -fsSL https://fnm.vercel.app/install | bash -s -- --install-dir /root/.fnm \
    && export PATH="/root/.fnm:$PATH" \
    && export FNM_DIR="/root/.fnm" \
    && eval "$(fnm env --shell bash)" \
    && fnm install 22 && fnm default 22 \
    && ln -sf "/root/.fnm/aliases/default" /root/.node-current

ENV PATH="/root/.node-current/bin:/root/.fnm:/root/.cargo/bin:${PATH}"
ENV FNM_DIR="/root/.fnm"
