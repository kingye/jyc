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

### GitHub CLI (`gh`) Version Too Old

The `deploy-bare-metal.sh` script installs `gh` from the **official GitHub CLI APT
repository** (`cli.github.com/packages`). However, if `gh` was already installed
from Debian's own repos before running the script, the idempotent guard
(`if ! command -v gh`) will skip the official repo setup — leaving you with
Debian's outdated version (e.g., 2.23.0 from 2023).

**Symptoms:**
- `gh pr edit --add-label` fails with:
  `GraphQL: Projects (classic) is being deprecated...`
- Any `gh` command involving issues/PRs with labels fails

**Fix:** Re-add the official repo and upgrade:

```bash
curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg \
    | sudo dd of=/usr/share/keyrings/githubcli-archive-keyring.gpg
sudo chmod go+r /usr/share/keyrings/githubcli-archive-keyring.gpg
echo "deb [arch=$(dpkg --print-architecture) signed-by=/usr/share/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" \
    | sudo tee /etc/apt/sources.list.d/github-cli.list > /dev/null
sudo apt-get update && sudo apt-get install -y gh
gh --version  # Should be 2.62.0+ to fix the projectCards deprecation
```

**Required version:** 2.62.0+ (fixes the `projectCards` GraphQL deprecation error).

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