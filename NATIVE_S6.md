# Native s6-overlay Support for jyc

This allows jyc to run with process supervision outside of Docker, enabling self-bootstrapping with automatic restarts.

## Setup

### 1. Install s6-overlay (one-time setup)

s6-overlay is installed to `~/.local/share/jyc-s6/`:

```bash
mkdir -p ~/.local/share/jyc-s6
cd ~/.local/share/jyc-s6
curl -fsSL "https://github.com/just-containers/s6-overlay/releases/download/v3.1.6.2/s6-overlay-noarch.tar.xz" -o /tmp/s6-overlay-noarch.tar.xz
tar -C . -Jxpf /tmp/s6-overlay-noarch.tar.xz && rm /tmp/s6-overlay-noarch.tar.xz
curl -fsSL "https://github.com/just-containers/s6-overlay/releases/download/v3.1.6.2/s6-overlay-x86_64.tar.xz" -o /tmp/s6-overlay-x86_64.tar.xz
tar -C . -Jxpf /tmp/s6-overlay-x86_64.tar.xz && rm /tmp/s6-overlay-x86_64.tar.xz
```

### 2. Build jyc binary

```bash
cargo build --release
cp target/release/jyc jyc
```

### 3. Start jyc with s6 supervision

```bash
./start-jyc.sh
```

## Usage

### Control Scripts

- `./start-jyc.sh` - Start jyc with s6 supervision
- `./jyc-ctl.sh` - Control the jyc service

### jyc-ctl Commands

```bash
# Check service status
./jyc-ctl.sh status

# Restart jyc (e.g., after self-bootstrapping)
./jyc-ctl.sh restart

# Stop jyc
./jyc-ctl.sh stop

# Start jyc (if stopped)
./jyc-ctl.sh start
```

## Self-Bootstrapping

The AI can rebuild and deploy jyc from inside the running process:

1. Build: `cargo build --release`
2. Deploy:
   ```bash
   cp. target/release/jyc jyc.bak
   cp target/release/jyc jyc
   $HOME/.local/share/jyc-s6/command/s6-svc -r $HOME/.local/share/jyc-s6/service/jyc
   ```
3. s6 automatically restarts jyc with the new binary

See `system.md.example` for detailed bootstrap instructions.

## Architecture

- **s6-overlay v3.1.6.2**: Process supervisor installed to `~/.local/share/jyc-s6/`
- **Service configs**: Versioned in `s6-rc.d/` directory
- **Runtime state**: Managed by s6 in `~/.local/share/jyc-s6/service/`
- **Binary location**: `/home/jiny/projects/jyc/jyc` (gitignored)
- **Backup**: `/home/jiny/projects/jyc/jyc.bak` (gitignored)

## Directory Structure

```
~/.local/share/jyc-s6/
├── command/              # s6 binaries (s6-rc, s6-rc-init, s6-svc, etc.)
├── etc/
├── init
├── package/
└── service/              # runtime state directory (created by start-jyc.sh)

/home/jiny/projects/jyc/
├── s6-rc.d/             # versioned service configs
│   ├── jyc/
│   │   ├── type         # "longrun"
│   │   └── run          # bash script to start jyc
│   └── user/contents.d/jyc
├── jyc                  # binary (gitignored)
├── jyc.bak              # backup (gitignored)
├── start-jyc.sh         # startup script
├── jyc-ctl.sh           # control script
└── system.md.example    # bootstrap instructions
```

## Troubleshooting

### Binary not found

If `start-jyc.sh` reports binary not found:

```bash
cargo build --release
cp target/release/jyc jyc
```

### Service won't start

Check s6 logs and status:

```bash
./jyc-ctl.sh status
ls -la ~/.local/share/jyc-s6/service/jyc/
```

### Missing OpenSSL dev packages

If build fails with OpenSSL errors:

```bash
sudo apt-get install pkg-config libssl-dev
```

## Comparison with Docker

| Feature | Docker | Native s6 |
|---------|--------|-----------|
| Process supervision | s6-overlay | s6-overlay |
| Self-bootstrapping | Yes | Yes |
| Automatic restarts | Yes | Yes |
| Runtime environment | Isolated | Native host |
| Build isolation | Containerized | Direct access |
| Setup complexity | Docker required | One-time s6 install |
| Resource overhead | Container overhead | Minimal |