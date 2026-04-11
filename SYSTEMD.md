# systemd Service Management for jyc

This allows jyc to run with systemd process supervision, enabling self-bootstrapping with automatic restarts.

## Setup

### 1. Create systemd user service (one-time setup)

Replace `<JYC_BINARY>` with the path to your jyc binary and `<JYC_WORKDIR>` with your jyc data directory.

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
ExecStart=<JYC_BINARY> monitor --workdir <JYC_WORKDIR> --debug
WorkingDirectory=<JYC_WORKDIR>
Restart=always
RestartSec=5
StandardOutput=journal
StandardError=journal

[Install]
WantedBy=default.target
EOF

systemctl --user daemon-reload
systemctl --user enable jyc
```

**Example** (adjust paths for your setup):
```
ExecStart=/home/user/bin/jyc monitor --workdir /home/user/jyc-data --debug
WorkingDirectory=/home/user/jyc-data
```

**Environment Variables:**

The service uses `EnvironmentFile` to source `~/.zshrc.local`.
Any environment variables defined there (API keys, etc.) will be available to jyc.

Alternatively, use the `run-jyc.sh` wrapper script as `ExecStart`:
```bash
ExecStart=/path/to/run-jyc.sh
```

The wrapper script auto-detects paths from `JYC_BINARY` and `JYC_WORKDIR` environment variables,
or falls back to `which jyc` and the current directory.

### 2. Build jyc binary

```bash
cd jyc  # your cloned jyc repository
cargo build --release
```

### 3. Install the binary

Copy the built binary to the location referenced in the service file:
```bash
cp target/release/jyc <JYC_BINARY>
```

### 4. Start jyc with systemd

```bash
systemctl --user start jyc
```

## Usage

### Control Scripts

- `jyc-ctl.sh` - Control the jyc service

### jyc-ctl Commands

```bash
# Check service status
./jyc-ctl.sh status

# Follow service logs
./jyc-ctl.sh logs

# Restart jyc
./jyc-ctl.sh restart

# Stop jyc
# WARN: AI should never call this command, it will break the whole process
./jyc-ctl.sh stop

# Start jyc
./jyc-ctl.sh start
```

## Self-Bootstrapping

The AI can rebuild and deploy jyc using `deploy.sh`:

1. Build: `cd jyc && cargo test && cargo build --release`
2. Deploy: `systemd-run --user --unit=jyc-deploy --working-directory=$(pwd)/jyc bash ./deploy.sh`

`deploy.sh` auto-detects:
- Source binary from its own directory (`target/release/jyc`)
- Install path from systemd service (`ExecStart`)

See the `jyc-deploy-bare` skill for detailed instructions.

## Architecture

- **systemd user service**: Process supervisor (built into Linux)
- **Service file**: `~/.config/systemd/user/jyc.service`
- **Binary location**: Configured in service file `ExecStart`
- **Data directory**: Configured in service file `WorkingDirectory`
- **Logs**: Managed by systemd journal (`journalctl --user -u jyc`)
- **Restart policy**: `Restart=always` with 5-second delay

## Directory Structure

```
~/.config/systemd/user/
└── jyc.service              # systemd user service file

<JYC_WORKDIR>/               # jyc data directory
├── config.toml              # jyc configuration
├── <channel>/workspace/     # per-channel thread workspaces
│   └── <thread>/
│       └── jyc/             # cloned jyc repo (per-thread)

<JYC_BINARY>                 # installed jyc binary
```

## Service Features

- **Automatic restarts**: If jyc crashes, systemd restarts it automatically
- **Journal integration**: Logs go to systemd journal for easy viewing
- **Dependency management**: Service starts after network is ready
- **User-scoped**: Runs as user without requiring sudo
- **Boot persistence**: Can be configured to start at login

## Troubleshooting

### Service won't start

Check service status and logs:
```bash
./jyc-ctl.sh status
./jyc-ctl.sh logs
```

### View detailed logs

```bash
# Last 100 lines
journalctl --user -u jyc -n 100

# Since last boot
journalctl --user -u jyc -b

# Follow logs live
journalctl --user -u jyc -f
```

### Missing OpenSSL dev packages

If build fails with OpenSSL errors:
```bash
sudo apt-get install pkg-config libssl-dev
```

## Comparison with Docker

| Feature | Docker | systemd |
|---------|--------|-----------|
| Process supervision | s6-overlay | systemd |
| Self-bootstrapping | Yes | Yes |
| Automatic restarts | Yes | Yes |
| Runtime environment | Isolated | Native host |
| Build isolation | Containerized | Direct access |
| Setup complexity | Docker required | One-time service file |
| Resource overhead | Container overhead | Minimal |
| Log management | s6 logs | systemd journal |
