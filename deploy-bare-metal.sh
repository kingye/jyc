#!/bin/bash
set -e

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
sudo apt-get install -y git curl build-essential pkg-config libssl-dev protobuf-compiler zsh

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

echo "=== Installing Node.js LTS ==="
if ! command -v node &> /dev/null; then
    curl -fsSL https://deb.nodesource.com/setup_lts.x | sudo -E bash -
    sudo apt-get install -y nodejs
fi

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

echo "=== Creating systemd user service ==="
mkdir -p "$HOME/.config/systemd/user"

mkdir -p "$WORKDIR"

cat > "$HOME/.config/systemd/user/jyc.service" << EOF
[Unit]
Description=jyc - AI-powered developer assistant

[Service]
Type=simple
Environment="HOME=%h"
Environment="JYC_WORKDIR=$WORKDIR"
EnvironmentFile=%h/.zshrc.local
ExecStart=%h/.local/bin/jyc
WorkingDirectory=$WORKDIR

[Install]
WantedBy=default.target
EOF

systemctl --user daemon-reload
systemctl --user enable jyc.service
systemctl --user start jyc.service

echo "=== Deployment complete ==="
echo "Dotfiles: $DOTFILES"
echo "Workdir: $WORKDIR"
echo ""
echo "Please configure your environment:"
echo "  - Edit ~/.zshrc.local and add:"
echo "    export ARK_API_KEY=your_api_key"
echo "    export JYC_BINARY=$HOME/.local/bin/jyc"
echo "    export JYC_WORKDIR=$WORKDIR"
echo ""
echo "Restart the service:"
echo "  systemctl --user restart jyc"