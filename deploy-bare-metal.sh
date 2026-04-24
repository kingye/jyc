#!/bin/bash
set -e

# Guard: do NOT run this script with sudo.
# The script uses sudo internally only for commands that need it.
# Running the whole script as root poisons ~/.cargo with root-owned files.
if [[ $EUID -eq 0 ]]; then
    echo "ERROR: Do not run this script with sudo."
    echo "Run as your normal user: ./deploy-bare-metal.sh -d <dotfiles> -w <workdir>"
    echo "The script uses sudo internally where needed."
    exit 1
fi

DOTFILES=""
WORKDIR=""

while [[ $# -gt 0 ]]; do
    case $1 in
        -d|--dotfiles)
            DOTFILES="$2"
            shift 2
            ;;
        -w|--workdir)
            WORKDIR="$2"
            shift 2
            ;;
        *)
            echo "Unknown option: $1"
            exit 1
            ;;
    esac
done

if [[ -z "$DOTFILES" ]]; then
    echo "Usage: $0 -d <dotfiles_path> -w <jyc_workdir>"
    exit 1
fi

if [[ -z "$WORKDIR" ]]; then
    echo "Usage: $0 -d <dotfiles_path> -w <jyc_workdir>"
    exit 1
fi

echo "=== Installing system packages ==="
sudo apt-get update
sudo apt-get install -y git curl build-essential pkg-config libssl-dev \
    protobuf-compiler zsh ripgrep jq pandoc inotify-tools

echo "=== Installing oh-my-zsh ==="
if [ ! -d "$HOME/.oh-my-zsh" ]; then
    sh -c "$(curl -fsSL https://raw.githubusercontent.com/ohmyzsh/ohmyzsh/master/tools/install.sh)" "" --unattended
fi

echo "=== Installing Rust ==="
if ! command -v rustc &> /dev/null; then
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
    export PATH="$HOME/.cargo/bin:$PATH"
fi
export PATH="$HOME/.cargo/bin:$PATH"

echo "=== Installing Python 3 ==="
if ! command -v python3 &> /dev/null; then
    sudo apt-get install -y python3 python3-pip
fi

echo "=== Installing fnm and Node.js 22 ==="
if ! command -v fnm &> /dev/null; then
    curl -fsSL https://fnm.vercel.app/install | bash -s -- --install-dir "$HOME/.local/bin"
fi
export FNM_DIR="$HOME/.local/share/fnm"
eval "$(fnm env)"
if ! command -v node &> /dev/null; then
    fnm install 22
    fnm default 22
fi

echo "=== Installing GitHub CLI ==="
# Always ensure the official GitHub CLI APT repo is configured.
# Debian's own repos ship a very old version (e.g., 2.23.0) that has
# broken GraphQL queries (projectCards deprecation). We need 2.62.0+.
if [ ! -f /usr/share/keyrings/githubcli-archive-keyring.gpg ] || ! grep -q "cli.github.com" /etc/apt/sources.list.d/github-cli.list 2>/dev/null; then
    curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg \
        | sudo dd of=/usr/share/keyrings/githubcli-archive-keyring.gpg
    sudo chmod go+r /usr/share/keyrings/githubcli-archive-keyring.gpg
    echo "deb [arch=$(dpkg --print-architecture) signed-by=/usr/share/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" \
        | sudo tee /etc/apt/sources.list.d/github-cli.list > /dev/null
    sudo apt-get update
fi
sudo apt-get install -y gh

echo "=== Installing Starship ==="
if ! command -v starship &> /dev/null; then
    curl -sS https://starship.rs/install.sh | sh -s -- -y
fi

echo "=== Installing OpenCode ==="
if ! command -v opencode &> /dev/null; then
    curl -fsSL https://opencode.ai/install | bash || { echo "Failed to install OpenCode"; exit 1; }
fi

echo "=== Setting up dotfiles ==="
mkdir -p "$HOME/.config/opencode"

if [[ -f "$DOTFILES/zsh/.zshrc" ]]; then
    ln -sf "$DOTFILES/zsh/.zshrc" "$HOME/.zshrc"
fi

if [[ -f "$DOTFILES/opencode/opencode.jsonc" ]]; then
    mkdir -p "$HOME/.config/opencode"
    ln -sf "$DOTFILES/opencode/opencode.jsonc" "$HOME/.config/opencode/opencode.jsonc"
fi

if [[ -f "$DOTFILES/zsh/.zshrc.local.example" ]]; then
    cp "$DOTFILES/zsh/.zshrc.local.example" "$HOME/.zshrc.local"
fi

echo "=== Cloning and building jyc ==="
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
JYC_REPO_DIR="$SCRIPT_DIR"

if [[ -d "$JYC_REPO_DIR/.git" ]]; then
    echo "Building jyc from local repository..."
    cargo build --release
else
    echo "Cloning jyc repository..."
    git clone https://github.com/kingye/jyc.git /tmp/jyc
    cd /tmp/jyc
    cargo build --release
    JYC_REPO_DIR="/tmp/jyc"
fi

mkdir -p "$HOME/.local/bin"
ln -sf "$JYC_REPO_DIR/target/release/jyc" "$HOME/.local/bin/jyc"

echo "=== Preparing systemd user service ==="
mkdir -p "$HOME/.config/systemd/user"

WORKDIR="$(cd "$WORKDIR" 2>/dev/null && pwd || mkdir -p "$WORKDIR" && cd "$WORKDIR" && pwd)"

cat > "$HOME/.config/systemd/user/jyc.service" << EOF
[Unit]
Description=jyc - AI-powered developer assistant

[Service]
Type=simple
Environment="HOME=%h"
Environment="JYC_WORKDIR=$WORKDIR"
EnvironmentFile=%h/.zshrc.local
ExecStart=$JYC_REPO_DIR/run-jyc.sh
WorkingDirectory=$WORKDIR

[Install]
WantedBy=default.target
EOF

# Enable lingering so systemd --user runs without an active login session
sudo loginctl enable-linger "$(whoami)" 2>/dev/null || true

echo ""
echo "=== Provisioning complete ==="
echo "Dotfiles: $DOTFILES"
echo "Workdir: $WORKDIR"
echo ""
echo "Next steps:"
echo "  1. Edit ~/.zshrc.local and add:"
echo "     export ARK_API_KEY=your_api_key"
echo "     export JYC_BINARY=$HOME/.local/bin/jyc"
echo "     export JYC_WORKDIR=$WORKDIR"
echo ""
echo "  2. Start jyc:"
echo "     $JYC_REPO_DIR/deploy.sh"
echo ""
echo "  3. Manage the service:"
echo "     $JYC_REPO_DIR/jyc-ctl.sh {status|logs|restart|stop|start}"