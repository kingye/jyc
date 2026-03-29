# s6-rc.d Directory

This directory contains s6-overlay service configuration files for native jyc process supervision.

## Purpose

These configuration files define how jyc runs under s6 supervision outside of Docker, enabling:
- Automatic restarts after crashes
- Self-bootstrapping with controlled restarts
- Process lifecycle management

## Directory Structure

```
s6-rc.d/
├── jyc/
│   ├── type              # Service type: "longrun"
│   └── run               # Executable startup script
└── user/
    └── contents.d/
        └── jyc           # Service registration in user bundle
```

## Files

### jyc/type
- **Content**: `longrun`
- **Purpose**: Defines jyc as a long-running service (runs automatically, restarts if it exits)

### jyc/run
- **Type**: Executable bash script
- **Purpose**: Starts jyc monitor with proper environment configuration
- **Actions**:
  1. Changes to jyc project directory
  2. Loads .env file for config.toml variable substitution
  3. Configures git credential helper
  4. Executes jyc monitor in --debug mode

### user/contents.d/jyc
- **Type**: Empty marker file
- **Purpose**: Registers jyc service in the "user" s6 bundle
- **Effect**: Service is started when user bundle is brought up

## Integration with External s6 Installation

These config files are versioned in the project and copied to the external s6 installation at runtime:

**External location**: `~/.local/share/jyc-s6/`

**Startup flow** (`start-jyc.sh`):
1. Copies `s6-rc.d/jyc/` → `~/.local/share/jyc-s6/s6-rc.d/jyc/`
2. Creates `~/.local/share/jyc-s6/s6-rc.d/user/contents.d/jyc`
3. Initializes s6 supervision: `s6-rc-init ~/.local/share/jyc-s6/service`
4. Brings up services: `s6-rc -u ~/.local/share/jyc-s6/service`

## Service Control

Once running, control jyc via `jyc-ctl.sh`:

```bash
./jyc-ctl.sh status    # Check service status
./jyc-ctl.sh restart   # Restart service (after bootstrap)
./jyc-ctl.sh stop      # Stop service
./jyc-ctl.sh start     # Start service
```

## Self-Bootstrapping Restart

When jyc rebuilds itself via AI bootstrap, it triggers a restart:

```bash
# After building new binary:
$HOME/.local/share/jyc-s6/command/s6-svc -r $HOME/.local/share/jyc-s6/service/jyc
```

s6 receives the restart signal, terminates the running jyc process, and automatically restarts it using the new binary.

## Comparison with Docker

| Aspect | Docker | Native s6 |
|--------|--------|-----------|
| Config location | `/etc/s6-overlay/s6-rc.d/` | `s6-rc.d/` (in project) |
| Config management | Installed at image build time | Copied from project at runtime |
| Runtime state | `/run/service/jyc/` | `~/.local/share/jyc-s6/service/jyc/` |
| Binary location | `/usr/local/bin/jyc` | `/home/jiny/projects/jyc/jyc` |

## See Also

- [NATIVE_S6.md](../NATIVE_S6.md) - Complete native s6 setup guide
- [system.md.example](../system.md.example) - Bootstrap instructions
- [start-jyc.sh](../start-jyc.sh) - Startup script
- [jyc-ctl.sh](../jyc-ctl.sh) - Service control script