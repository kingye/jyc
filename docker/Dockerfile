# ── Stage 1: Base tools (cached — rarely changes) ─────────────────────
FROM debian:bookworm-slim AS base

# Runtime tools for jyc and AI agent
RUN apt-get update && apt-get install -y --no-install-recommends \
    git curl ripgrep jq procps xz-utils unzip inotify-tools \
    ca-certificates pkg-config libssl-dev build-essential \
    protobuf-compiler \
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
FROM rust:bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    protobuf-compiler libprotobuf-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY Cargo.toml Cargo.lock ./

RUN mkdir src && echo 'fn main() {}' > src/main.rs \
    && cargo build --release

RUN rm -rf src target/release/deps/jyc* target/release/jyc* target/release/.fingerprint/jyc*

COPY src/ src/
COPY config.example.toml ./
RUN cargo build --release

# ── Stage 3: Production image ────────────────────────────────────────
# Build: podman build -t jyc:latest -f docker/Dockerfile ..
FROM base AS production

COPY --from=builder /build/target/release/jyc /usr/local/bin/jyc

ENTRYPOINT ["jyc"]
CMD ["monitor", "--workdir", "/opt/jyc", "--debug"]
