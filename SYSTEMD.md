# systemd Service Management for jyc

This allows jyc to run with systemd process supervision, enabling self-bootstrapping with automatic restarts.

## Setup

### 1. Create systemd user service (one-time setup)

The service file is created at `~/.config/systemd/user/jyc.service`:

```bash
mkdir -p ~/.config/systemd/user
cat > ~/.config/systemd/user/jyc.service << 'EOF'
[Unit]
Description=JYC - Channel-agnostic AI agent
After=network.target

[Service]
Type=simple
EnvironmentFile=/home/jiny/.zshrc.local
Environment=PATH=/home/jiny/.opencode/bin:/home/jiny/.local/bin:/home/jiny/.cargo/bin:/usr/local/bin:/usr/bin:/bin
ExecStart=/home/jiny/projects/jyc/jyc monitor --workdir /home/jiny/projects/jyc-data --debug
WorkingDirectory=/home/jiny/projects/jyc-data
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

**Environment Variables:**

The service uses `run-jyc.sh` wrapper script that sources `~/.zshrc.local`.
This means any environment variables defined in `~/.zshrc.local` will be available to jyc when running under systemd.

The wrapper script (`run-jyc.sh`):
```bash
#!/usr/bin/bash
cd /home/jiny/projects/jyc-data

# Source environment variables from .zshrc.local
if [ -f ~/.zshrc.local ]; then
  set -a
  source ~/.zshrc.local
  set +a
fi

exec /home/jiny/projects/jyc/jyc monitor --workdir /home/jiny/projects/jyc-data --debug
```

Update `~/.zshrc.local` and restart the service to apply changes:
```bash
systemctl --user restart jyc
```

### 2. Build jyc binary

```bash
cargo build --release
cp target/release/jyc jyc
```

### 3. Start jyc with systemd

```bash
systemctl --user start jyc
```

## Usage

### Control Scripts

- `./jyc-ctl.sh` - Control the jyc service

### jyc-ctl Commands

```bash
# Check service status
./jyc-ctl.sh status

# Follow service logs
./jyc-ctl.sh logs

# Restart jyc (e.g., after self-bootstrapping)
./jyc-ctl.sh restart

# Stop jyc
# WARN:-As AI never call this command, it will break the whole process
./jyc-ctl.sh stop

# Start jyc
./jyc-ctl.sh start
```

### Direct systemctl Commands

```bash
# Check service status
systemctl --user status jyc

# View logs
journalctl --user -u jyc -f

# Restart service
systemctl --user restart jyc

# Stop service
# WARN:-As AI never call this command, it will break the whole process
systemctl --user stop jyc

# Start service
systemctl --user start jyc
```

## Self-Bootstrapping

The AI can rebuild and deploy jyc from inside the running process:

1. Build: `cargo build --release`
2. Deploy:
   ```bash
   cp target/release/jyc jyc
   systemctl --user restart jyc
   ```
3. systemd automatically restarts jyc with the new binary

See `system.md.example` for detailed bootstrap instructions.

## Architecture

- **systemd user service**: Process supervisor (built into Linux)
- **Service file**: `~/.config/systemd/user/jyc.service`
- **Binary location**: `/home/jiny/projects/jyc/jyc` (gitignored)
- **Logs**: Managed by systemd journal (`journalctl --user -u jyc`)
- **Restart policy**: `Restart=always` with 5-second delay

## Directory Structure

```
~/.config/systemd/user/
└── jyc.service             # systemd user service file

/home/jiny/projects/jyc/
├── jyc                      # binary (gitignored)
├── jyc-ctl.sh              # control script
└── system.md.example         # bootstrap instructions
```

## Service Features

- **Automatic restarts**: If jyc crashes, systemd restarts it automatically
- **Journal integration**: Logs go to systemd journal for easy viewing
- **Dependency management**: Service starts after network is ready
- **User-scoped**: Runs as user without requiring sudo
- **Boot persistence**: Can be configured to start at login

## Troubleshooting

### Binary not found

If the service fails to start due to missing binary:

```bash
cargo build --release
cp target/release/jyc jyc
systemctl --user restart jyc
```

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
