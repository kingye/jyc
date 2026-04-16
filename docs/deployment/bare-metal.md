# Bare Metal Deployment Guide

This guide covers deploying JYC on bare metal (Ubuntu 22.04+ / Debian 12+) using the automated deployment script or manual steps.

## System Requirements

- **OS**: Ubuntu 22.04+ or Debian 12+
- **Shell**: zsh
- **Tools**: git, curl, pandoc, jq, ripgrep
- **Languages**: Python 3.10+, Node.js 18+
- **Runtime**: Rust toolchain (via rustup)

## Quick Start

Run the automated deployment script:

```bash
git clone https://github.com/kingye/jyc.git ~/jyc
cd ~/jyc
./scripts/deploy-bare-metal.sh
```

Follow the interactive prompts. The script will:
1. Detect your system
2. Install missing dependencies
3. Set up dotfiles
4. Clone or update JYC repository
5. Build the JYC binary
6. Create systemd service configuration

## Automated Script Usage

### Basic Usage

```bash
./scripts/deploy-bare-metal.sh
```

### Options

| Option | Description | Default |
|--------|-------------|---------|
| `--repo-path PATH` | JYC repository location | `~/jyc` |
| `--binary-path PATH` | Binary installation path | `~/.local/bin/jyc` |
| `--workdir PATH` | JYC data directory | `~/.jyc-data` |
| `--skip-deps` | Skip dependency installation | No |
| `--skip-build` | Skip JYC build | No |
| `--skip-systemd` | Skip systemd setup | No |
| `-h, --help` | Show help | - |

### Examples

```bash
# Custom paths
./scripts/deploy-bare-metal.sh \
    --repo-path ~/projects/jyc \
    --binary-path /usr/local/bin/jyc \
    --workdir /var/jyc-data

# Skip time-consuming steps for re-runs
./scripts/deploy-bare-metal.sh --skip-deps --skip-build
```

## Manual Deployment

If you prefer to deploy manually, follow these steps.

### 1. Install System Dependencies

```bash
sudo apt-get update
sudo apt-get install -y \
    git \
    curl \
    pandoc \
    jq \
    ripgrep \
    pkg-config \
    libssl-dev \
    build-essential \
    zsh
```

### 2. Install Rust

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source ~/.cargo/env
```

### 3. Install OpenCode

From dotfiles:
```bash
mkdir -p ~/.local/bin
ln -s ~/projects/dotfiles/opencode/bin/opencode ~/.local/bin/opencode
```

Or from official source (see https://opencode.ai).

### 4. Clone JYC Repository

```bash
git clone https://github.com/kingye/jyc.git ~/jyc
cd ~/jyc
```

### 5. Build JYC

```bash
cargo build --release
```

### 6. Install Binary

```bash
mkdir -p ~/.local/bin
cp target/release/jyc ~/.local/bin/jyc
chmod +x ~/.local/bin/jyc
```

Verify:
```bash
~/.local/bin/jyc --version
```

### 7. Configure Systemd Service

Create the service file:

```bash
mkdir -p ~/.config/systemd/user
cat > ~/.config/systemd/user/jyc.service << 'EOF'
[Unit]
Description=JYC - Channel-agnostic AI agent
After=network.target

[Service]
Type=simple
EnvironmentFile=%h/.zshrc.local
Environment=PATH=%h/.opencode/bin:%h/.local/bin:%h/.cargo/bin:/usr/local/bin:/usr/bin:/bin
ExecStart=%h/.local/bin/jyc monitor --workdir %h/.jyc-data --debug
WorkingDirectory=%h/.jyc-data
Restart=always
RestartSec=5
StandardOutput=journal
StandardError=journal

[Install]
WantedBy=default.target
EOF
```

Enable the service:

```bash
systemctl --user daemon-reload
systemctl --user enable jyc
```

## First-Time Configuration

### 1. Create Workdir

```bash
mkdir -p ~/.jyc-data
```

### 2. Copy Config Template

```bash
cp ~/jyc/config.example.toml ~/.jyc-data/config.toml
```

### 3. Edit Configuration

Edit `~/.jyc-data/config.toml` with your credentials:

```toml
[general]
max_concurrent_threads = 3

[channels.your_channel]
type = "email"

[channels.your_channel.inbound]
host = "imap.example.com"
port = 993
tls = true
username = "your@email.com"
password = "your_password"
```

### 4. Add Environment Variables

Add to `~/.zshrc.local`:

```bash
export JYC_BINARY=~/.local/bin/jyc
export JYC_WORKDIR=~/.jyc-data
# Add your API keys here
export OPENCODE_API_KEY=your_api_key
```

### 5. Start JYC

```bash
systemctl --user start jyc
```

## Service Management

```bash
# Check status
systemctl --user status jyc

# View logs
journalctl --user -u jyc -f

# Stop (use with caution - AI should never call this)
systemctl --user stop jyc

# Restart
systemctl --user restart jyc
```

Or use the control script:

```bash
./jyc-ctl.sh status
./jyc-ctl.sh logs
./jyc-ctl.sh restart
```

## Directory Structure

After deployment:

```
~/.local/bin/
└── jyc                    # JYC binary

~/.config/systemd/user/
└── jyc.service            # Systemd service file

~/.jyc-data/
├── config.toml            # JYC configuration
└── <channel>/             # Channel workspaces
    └── <thread>/          # Thread directories
        └── jyc/           # Per-thread JYC clone
```

## Troubleshooting

### Build Fails - Missing OpenSSL

```bash
sudo apt-get install pkg-config libssl-dev
```

### Build Fails - Missing protobuf

```bash
sudo apt-get install protobuf-compiler
```

### Service Won't Start

Check status and logs:

```bash
systemctl --user status jyc
journalctl --user -u jyc -n 50
```

### Config Not Found

Ensure `config.toml` exists in your workdir:

```bash
ls -la ~/.jyc-data/config.toml
```

### Binary Not Found

Verify the binary path matches what's in the service file:

```bash
cat ~/.config/systemd/user/jyc.service | grep ExecStart
ls -la ~/.local/bin/jyc
```

## Re-running the Deployment

The script is idempotent - you can run it multiple times safely.

To skip time-consuming steps:

```bash
./scripts/deploy-bare-metal.sh --skip-deps --skip-build
```

This is useful when:
- Updating the systemd service file
- Re-cloning the repository
- Re-configuring paths

## Updating JYC

After making changes to JYC source:

```bash
cd ~/jyc
cargo build --release
cp target/release/jyc ~/.local/bin/jyc
systemctl --user restart jyc
```

Or use the automated deploy script (see jyc-deploy-bare skill).