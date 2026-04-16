# Bare Metal Deployment Guide

This guide covers deploying jyc on a fresh Ubuntu/Debian server without Docker.

## Prerequisites

- **OS**: Ubuntu 20.04+ or Debian 11+
- **Access**: Root sudo access for package installation
- **API Keys**:
  - `ARK_API_KEY` - Anthropic API key for OpenCode

## Quick Start

### 1. Clone Repositories

```bash
git clone https://github.com/kingye/jyc.git
git clone https://github.com/kingye/dotfiles.git /path/to/dotfiles
```

### 2. Run Deployment Script

```bash
cd jyc
sudo ./deploy-bare-metal.sh -d /path/to/dotfiles -w /path/to/jyc-data
```

The script will:
- Install system packages (git, curl, build-essential, pkg-config, libssl-dev, protobuf-compiler, zsh, starship)
- Install runtimes (Rust, Python 3, Node.js LTS)
- Install OpenCode AI backend
- Symlink dotfiles
- Clone and build jyc
- Install binary to `~/.local/bin/jyc`
- Create and enable systemd user service

### 3. Configure Environment

Edit `~/.zshrc.local` and add your API key:

```bash
vi ~/.zshrc.local
```

Ensure these variables are set:

```bash
export ARK_API_KEY="your-anthropic-api-key"
export JYC_BINARY="$HOME/.local/bin/jyc"
export JYC_WORKDIR="/path/to/jyc-data"
```

### 4. Start the Service

```bash
systemctl --user restart jyc
```

### 5. Verify

```bash
systemctl --user status jyc
journalctl --user -u jyc -f
```

## Configuration

### Environment Variables

| Variable | Required | Description |
|----------|----------|-------------|
| `ARK_API_KEY` | Yes | Anthropic API key for OpenCode |
| `JYC_BINARY` | Yes | Path to jyc binary (auto-set by script) |
| `JYC_WORKDIR` | Yes | Path to jyc data directory |
| `RUST_BACKTRACE` | No | Set to "1" for debug logging |
| `LOG_LEVEL` | No | Logging level (debug, info, warn, error) |

### Systemd Service

The deployment script creates a user systemd service at `~/.config/systemd/user/jyc.service`.

Commands:
```bash
systemctl --user start jyc     # Start
systemctl --user stop jyc      # Stop
systemctl --user restart jyc   # Restart
systemctl --user status jyc    # Check status
journalctl --user -u jyc -f    # View logs
```

## Troubleshooting

### Service Won't Start

Check logs:
```bash
journalctl --user -u jyc -e
```

Common issues:
- Missing `ARK_API_KEY` in `~/.zshrc.local`
- Invalid API key
- Port 8080 already in use

### OpenCode Connection Failed

Verify OpenCode is installed:
```bash
opencode --version
```

Check OpenCode config:
```bash
cat ~/.config/opencode/config.jsonc
```

### Rebuild jyc

```bash
cd /path/to/jyc
cargo build --release
ln -sf "$PWD/target/release/jyc" ~/.local/bin/jyc
systemctl --user restart jyc
```

## Directory Structure

After deployment:

```
~/.local/bin/jyc              # jyc binary
~/.config/systemd/user/jyc.service  # systemd service
~/.zshrc                      # zsh config (symlinked)
~/.zshrc.local                # environment variables
~/.config/opencode/opencode.jsonc  # OpenCode config (symlinked)
/path/to/jyc-data/            # jyc data directory
```