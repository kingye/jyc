# ── Stage 1: Base tools (cached — rarely changes) ─────────────────────
FROM debian:bookworm-slim AS base

# Runtime tools for jyc and AI agent
RUN apt-get update && apt-get install -y --no-install-recommends \
    git curl ripgrep jq procps xz-utils unzip inotify-tools \
    ca-certificates pkg-config libssl-dev build-essential \
    protobuf-compiler libprotobuf-dev \
    pandoc python3 \
    && rm -rf /var/lib/apt/lists/*

# Install Rust toolchain (needed by OpenCode agent for cargo build/test)
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
ENV PATH="/root/.cargo/bin:${PATH}"

# Install OpenCode
RUN curl -fsSL https://opencode.ai/install | bash \
    && ln -sf /root/.opencode/bin/opencode /usr/local/bin/opencode

# Install GitHub CLI
RUN curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg \
    | dd of=/usr/share/keyrings/githubcli-archive-keyring.gpg \
    && chmod go+r /usr/share/keyrings/githubcli-archive-keyring.gpg \
    && echo "deb [arch=$(dpkg --print-architecture) signed-by=/usr/share/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" \
    | tee /etc/apt/sources.list.d/github-cli.list > /dev/null \
    && apt-get update && apt-get install -y --no-install-recommends gh && rm -rf /var/lib/apt/lists/*

# Install fnm (Fast Node Manager) and pre-install Node.js 22
RUN curl -fsSL https://fnm.vercel.app/install | bash -s -- --install-dir /usr/local/bin \
    && eval "$(fnm env)" \
    && fnm install 22 \
    && fnm default 22
ENV FNM_DIR="/root/.local/share/fnm"
ENV PATH="/root/.local/share/fnm/aliases/default/bin:${PATH}"

# Install oauth2-forwarder for browser-based OAuth2 in containers
RUN npm install -g oauth2-forwarder

# Directories and workdir
RUN mkdir -p /root/.claude/skills /root/.agents/skills \
    /root/.config/opencode /root/.config/gh \
    /root/.local/share/opencode \
    /opt/jyc

WORKDIR /opt/jyc

VOLUME ["/opt/jyc", "/root/.config/opencode", "/root/.claude/skills", "/root/.agents/skills"]

# ── Stage 2: Build jyc (changes when source changes) ─────────────────
FROM base AS builder

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
    && cargo build --release 2>/dev/null || true

# 3. Remove stubs and fingerprints, copy real source, build
RUN rm -rf crates/*/src target/release/deps/jyc* target/release/.fingerprint/jyc*

COPY crates/ crates/
COPY config.example.toml ./
RUN cargo build --release -p jyc-cli

# ── Stage 3: Production image ────────────────────────────────────────
# Build: podman build -t jyc:latest -f docker/Dockerfile .
FROM base AS production

COPY --from=builder /build/target/release/jyc /usr/local/bin/jyc

ENTRYPOINT ["jyc"]
CMD ["monitor", "--workdir", "/opt/jyc", "--debug"]
