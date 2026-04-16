#!/usr/bin/env zsh

set -e

DOTFILES_SOURCE="/home/jiny/projects/dotfiles"
DOTFILES_DEST="$HOME/.config"

JYC_REPO_URL="https://github.com/kingye/jyc.git"
JYC_DEFAULT_DIR="$HOME/jyc"

show_usage() {
    cat << EOF
Usage: $0 [OPTIONS]

Deploy JYC on bare metal (Ubuntu/Debian).

OPTIONS:
    --repo-path PATH    JYC repository path (default: $JYC_DEFAULT_DIR)
    --binary-path PATH  JYC binary installation path (default: ~/.local/bin/jyc)
    --workdir PATH      JYC workdir for data (default: ~/.jyc-data)
    --skip-deps        Skip dependency installation
    --skip-build       Skip JYC build
    --skip-systemd     Skip systemd service creation
    -h, --help         Show this help message

EXAMPLES:
    $0                              # Interactive deployment with defaults
    $0 --repo-path ~/my-jyc        # Custom repo path
    $0 --skip-deps                 # Skip installing dependencies
EOF
}

REPO_PATH="$JYC_DEFAULT_DIR"
BINARY_PATH="$HOME/.local/bin/jyc"
WORKDIR="$HOME/.jyc-data"
SKIP_DEPS=false
SKIP_BUILD=false
SKIP_SYSTEMD=false

while [[ $# -gt 0 ]]; do
    case $1 in
        --repo-path)
            REPO_PATH="$2"
            shift 2
            ;;
        --binary-path)
            BINARY_PATH="$2"
            shift 2
            ;;
        --workdir)
            WORKDIR="$2"
            shift 2
            ;;
        --skip-deps)
            SKIP_DEPS=true
            shift
            ;;
        --skip-build)
            SKIP_BUILD=true
            shift
            ;;
        --skip-systemd)
            SKIP_SYSTEMD=true
            shift
            ;;
        -h|--help)
            show_usage
            exit 0
            ;;
        *)
            echo "Unknown option: $1"
            show_usage
            exit 1
            ;;
    esac
done

print_step() {
    echo ""
    echo "========================================"
    echo "STEP: $1"
    echo "========================================"
}

print_info() {
    echo "[INFO] $1"
}

print_warn() {
    echo "[WARN] $1"
}

print_error() {
    echo "[ERROR] $1"
}

confirm() {
    local prompt="$1"
    local response
    echo ""
    echo -n "$prompt [y/N]: "
    read response
    case "$response" in
        [yY][eE][sS]|[yY]) return 0 ;;
        *) return 1 ;;
    esac
}

detect_system() {
    print_step "System Detection"

    if [[ ! -f /etc/os-release ]]; then
        print_error "Cannot detect OS. This script requires /etc/os-release."
        exit 1
    fi

    source /etc/os-release
    print_info "Detected OS: $NAME $VERSION"

    case "$ID" in
        ubuntu)
            if [[ "${VERSION_ID%.*}" -lt 22 ]]; then
                print_error "Ubuntu 22.04+ required. Detected: $VERSION_ID"
                exit 1
            fi
            ;;
        debian)
            if [[ "$VERSION_ID" -lt 12 ]]; then
                print_error "Debian 12+ required. Detected: $VERSION_ID"
                exit 1
            fi
            ;;
        *)
            print_error "Unsupported OS: $ID. Only Ubuntu 22.04+ and Debian 12+ are supported."
            exit 1
            ;;
    esac

    print_info "Shell: $SHELL"
    if [[ "$SHELL" != */zsh ]]; then
        print_error "zsh is required. Detected: $SHELL"
        exit 1
    fi

    local required_cmds=("git" "curl" "pandoc" "jq" "rg")
    local missing_cmds=()
    for cmd in $required_cmds; do
        if ! command -v "$cmd" &>/dev/null; then
            missing_cmds+=("$cmd")
        fi
    done

    if [[ ${#missing_cmds[@]} -gt 0 ]]; then
        print_warn "Missing commands: ${missing_cmds[*]}"
        if ! confirm "Install missing commands?"; then
            print_error "Aborted."
            exit 1
        fi
    else
        print_info "All required commands present"
    fi
}

install_deps() {
    if [[ "$SKIP_DEPS" == true ]]; then
        print_info "Skipping dependency installation (--skip-deps)"
        return
    fi

    print_step "Installing Dependencies"

    local update_needed=false
    if ! command -v git &>/dev/null || ! command -v curl &>/dev/null || ! command -v pandoc &>/dev/null || ! command -v jq &>/dev/null || ! command -v rg &>/dev/null; then
        update_needed=true
    fi

    if [[ "$update_needed" == true ]]; then
        if ! confirm "Update package lists (apt update)?"; then
            print_error "Aborted."
            exit 1
        fi

        print_info "Updating package lists..."
        sudo apt-get update -qq

        print_info "Installing system packages..."
        sudo apt-get install -y -qq \
            git \
            curl \
            pandoc \
            jq \
            ripgrep \
            pkg-config \
            libssl-dev \
            build-essential \
            zsh
    else
        print_info "System packages already installed"
    fi

    if ! command -v python3 &>/dev/null; then
        print_info "Python3 not found. Please ensure Python 3.10+ is installed."
    else
        PYTHON_VERSION=$(python3 --version | cut -d' ' -f2 | cut -d'.' -f1,2)
        print_info "Python: $PYTHON_VERSION"
    fi

    if ! command -v node &>/dev/null; then
        print_info "Node.js not found. Please ensure Node.js 18+ is installed."
    else
        NODE_VERSION=$(node --version)
        print_info "Node.js: $NODE_VERSION"
    fi

    if ! command -v rustc &>/dev/null; then
        if confirm "Install Rust toolchain (rustup)?"; then
            print_info "Installing Rust via rustup..."
            curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
            if [[ -f "$HOME/.cargo/env" ]]; then
                source "$HOME/.cargo/env"
            fi
        fi
    else
        RUST_VERSION=$(rustc --version)
        print_info "Rust: $RUST_VERSION"
    fi

    if ! command -v protoc &>/dev/null; then
        if confirm "Install protobuf-compiler?"; then
            print_info "Installing protobuf-compiler..."
            sudo apt-get install -y -qq protobuf-compiler
        fi
    else
        PROTOC_VERSION=$(protoc --version)
        print_info "Protobuf: $PROTOC_VERSION"
    fi

    if ! command -v opencode &>/dev/null; then
        print_info "OpenCode not found."
        if [[ -d "$DOTFILES_SOURCE/opencode" ]]; then
            if confirm "Link OpenCode from dotfiles?"; then
                mkdir -p "$HOME/.local/bin"
                if [[ ! -L "$HOME/.local/bin/opencode" ]]; then
                    ln -s "$DOTFILES_SOURCE/opencode/bin/opencode" "$HOME/.local/bin/opencode"
                    print_info "Linked OpenCode to ~/.local/bin/opencode"
                else
                    print_info "OpenCode already linked"
                fi
            fi
        else
            print_warn "OpenCode not found in dotfiles. Please install manually."
        fi
    else
        OPENCODE_VERSION=$(opencode --version 2>/dev/null || echo "unknown")
        print_info "OpenCode: $OPENCODE_VERSION"
    fi
}

setup_dotfiles() {
    print_step "Setting up Dotfiles"

    if [[ -d "$DOTFILES_SOURCE" ]]; then
        print_info "Found dotfiles at $DOTFILES_SOURCE"
        
        local configs=("zsh" "vim" "git" "atuin" "ghostty" "alacritty")
        for config in $configs; do
            if [[ -d "$DOTFILES_SOURCE/$config" ]]; then
                mkdir -p "$DOTFILES_DEST"
                if [[ ! -L "$DOTFILES_DEST/$config" ]]; then
                    ln -sf "$DOTFILES_SOURCE/$config" "$DOTFILES_DEST/$config" 2>/dev/null || true
                    print_info "Linked $config"
                fi
            fi
        done
    else
        print_warn "Dotfiles not found at $DOTFILES_SOURCE"
    fi

    if [[ -d "$HOME/.opencode" ]]; then
        print_info "OpenCode config found at $HOME/.opencode"
    else
        print_warn "OpenCode config not found at $HOME/.opencode"
    fi
}

clone_or_update_jyc() {
    print_step "JYC Repository"

    if [[ -d "$REPO_PATH/.git" ]]; then
        print_info "JYC repository already exists at $REPO_PATH"
        if confirm "Pull latest changes?"; then
            cd "$REPO_PATH"
            git pull
        fi
    else
        print_info "Will clone JYC to $REPO_PATH"
        if confirm "Proceed with clone?"; then
            git clone "$JYC_REPO_URL" "$REPO_PATH"
        else
            print_error "Aborted."
            exit 1
        fi
    fi
}

build_jyc() {
    if [[ "$SKIP_BUILD" == true ]]; then
        print_info "Skipping build (--skip-build)"
        return
    fi

    print_step "Building JYC"

    if [[ ! -d "$REPO_PATH" ]]; then
        print_error "JYC repository not found at $REPO_PATH"
        exit 1
    fi

    cd "$REPO_PATH"

    if confirm "Build JYC (cargo build --release)?"; then
        print_info "Building JYC (this may take a few minutes)..."
        
        if command -v protoc &>/dev/null; then
            print_info "protoc found"
        else
            print_warn "protoc not found - build may fail"
        fi
        
        cargo build --release
        
        if [[ -f "target/release/jyc" ]]; then
            local binary_size=$(du -h "target/release/jyc" | cut -f1)
            print_info "Build successful! Binary size: $binary_size"
            
            mkdir -p "$(dirname "$BINARY_PATH")"
            cp "target/release/jyc" "$BINARY_PATH"
            chmod +x "$BINARY_PATH"
            print_info "Installed to $BINARY_PATH"
            
            "$BINARY_PATH" --version
        else
            print_error "Build failed - binary not found"
            exit 1
        fi
    else
        print_error "Aborted."
        exit 1
    fi
}

setup_systemd() {
    if [[ "$SKIP_SYSTEMD" == true ]]; then
        print_info "Skipping systemd setup (--skip-systemd)"
        return
    fi

    print_step "Systemd Service Configuration"

    mkdir -p "$HOME/.config/systemd/user"

    local service_file="$HOME/.config/systemd/user/jyc.service"
    
    if [[ -f "$service_file" ]]; then
        print_info "Service file already exists at $service_file"
        if ! confirm "Overwrite existing service file?"; then
            print_info "Keeping existing service file"
            return
        fi
    fi

    if confirm "Create systemd service at $service_file?"; then
        cat > "$service_file" << EOF
[Unit]
Description=JYC - Channel-agnostic AI agent
After=network.target

[Service]
Type=simple
EnvironmentFile=%h/.zshrc.local
Environment=PATH=%h/.opencode/bin:%h/.local/bin:%h/.cargo/bin:/usr/local/bin:/usr/bin:/bin
ExecStart=$BINARY_PATH monitor --workdir $WORKDIR --debug
WorkingDirectory=$WORKDIR
Restart=always
RestartSec=5
StandardOutput=journal
StandardError=journal

[Install]
WantedBy=default.target
EOF

        print_info "Created $service_file"
        
        if confirm "Enable service to start on boot?"; then
            systemctl --user daemon-reload
            systemctl --user enable jyc
            print_info "Service enabled"
        fi
        
        print_info "NOT starting service - user must configure and start manually"
    fi
}

user_config_guide() {
    print_step "User Configuration"

    echo ""
    echo "IMPORTANT: You need to configure JYC before starting it!"
    echo ""
    echo "1. Create workdir:"
    echo "   mkdir -p $WORKDIR"
    echo ""
    echo "2. Copy config template:"
    echo "   cp $REPO_PATH/config.example.toml $WORKDIR/config.toml"
    echo ""
    echo "3. Edit config.toml with your credentials:"
    echo "   vim $WORKDIR/config.toml"
    echo ""
    echo "4. Add to ~/.zshrc.local:"
    echo "   export JYC_BINARY=$BINARY_PATH"
    echo "   export JYC_WORKDIR=$WORKDIR"
    echo ""
    echo "5. To start manually:"
    echo "   systemctl --user start jyc"
    echo ""
    echo "6. To check status:"
    echo "   systemctl --user status jyc"
    echo ""
    echo "7. To view logs:"
    echo "   journalctl --user -u jyc -f"
    echo ""
}

main() {
    echo "========================================"
    echo "JYC Bare Metal Deployment"
    echo "========================================"
    echo ""
    echo "Configuration:"
    echo "  Repository: $REPO_PATH"
    echo "  Binary:     $BINARY_PATH"
    echo "  Workdir:    $WORKDIR"
    echo ""

    if ! confirm "Proceed with deployment?"; then
        print_error "Aborted."
        exit 1
    fi

    detect_system
    install_deps
    setup_dotfiles
    clone_or_update_jyc
    build_jyc
    setup_systemd
    user_config_guide

    print_step "Deployment Complete"
    print_info "Please review the configuration steps above and start the service when ready."
}

main "$@"