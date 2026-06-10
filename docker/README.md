# JYC Docker Deployment

Run JYC as a containerized service.

## Architecture

- **Multi-stage build**: `builder` (compile) → `slim` (minimal runtime) → `full` (dev tools) → `production`
- **Single binary**: `jyc` (the MCP reply tool is a hidden subcommand `jyc mcp-reply-tool`).
- **Host networking**: Container shares host network (`network_mode: host`), so services on `localhost` are accessible from inside the container.

## Image Variants

| Target | Size | Use Case |
|--------|------|----------|
| `slim` | ~150-250 MB | Production deployments, external compose files |
| `full` / `production` | ~2 GB+ | Local development with AI agent tooling |

### Slim Image

Minimal runtime with only essential tools:

| Tool | Purpose |
|------|---------|
| OpenCode | AI agent runtime (bind-mounted) |
| ripgrep, jq | Code search, JSON processing |
| curl | HTTP requests |
| python3 | Python scripting |

### Full / Production Image

Extends `slim` with development tools for the AI agent to rebuild and work with code:

| Tool | Purpose |
|------|---------|
| git, gh CLI | Version control, GitHub PRs |
| build-essential | C compiler for native deps |
| Rust toolchain | cargo build/test for AI agent |
| pandoc | Document conversion |
| protobuf-compiler | Protocol buffer compilation |

## Build

**Slim image (for production / external use):**

```bash
docker build --target slim -t jyc:slim -f docker/Dockerfile .
```

**Full image (for local development):**

```bash
docker build --target production -t jyc:latest -f docker/Dockerfile .
# or
cd docker && docker compose up --build -d
```

## Prerequisites

- Docker or Podman
- JYC data directory with `config.toml` configured
- OpenCode config file (`opencode.jsonc`) with API keys

## Directory Structure

```
/opt/jyc/                    ← JYC data (bind-mounted from host)
├── config.toml              ← JYC configuration
├── .env                     ← Environment variables for secrets
├── <channel>/
│   ├── .imap/               ← IMAP state
│   └── workspace/           ← Thread workspaces
├── .netrc                   ← Git credentials
└── gh_hosts.yml             ← GitHub CLI auth

/root/.config/opencode/
└── opencode.jsonc           ← OpenCode global config (bind-mounted)

/root/.claude/skills/        ← Skills (bind-mounted, read-only)
/root/.agents/skills/        ← Agent skills (bind-mounted, read-only)
```

## Quick Start

### 1. Create `.env` file

```bash
cp docker/.env.example docker/.env
# Edit docker/.env with your paths
```

### 2. Build and start

**With Docker/Podman Compose (uses `production` target):**

```bash
cd docker
docker compose up --build -d
docker compose logs -f
```

**With Podman (without Compose):**

```bash
# Build slim image
podman build --target slim -t jyc:slim -f docker/Dockerfile ..

# Run slim image
podman run -d --name jyc \
  --network=host \
  -v /path/to/jyc-data:/opt/jyc \
  -v /path/to/opencode.jsonc:/root/.config/opencode/opencode.jsonc:ro \
  -v /path/to/.claude/skills:/root/.claude/skills:ro \
  -v /path/to/.agents/skills:/root/.agents/skills:ro \
  --restart unless-stopped \
  jyc:slim
```

### 3. Check logs

```bash
docker compose logs -f jyc
# or
podman logs -f jyc
```

### 4. Restart the service

```bash
docker compose restart jyc
```

## Volume Mounts

| Mount | Container Path | Purpose |
|-------|---------------|---------|
| JYC data dir | `/opt/jyc` | Config, channels, workspace |
| OpenCode config | `/root/.config/opencode/opencode.jsonc` | API keys, providers |
| OpenCode data | `/root/.local/share/opencode` | Sessions DB, logs, snapshots |
| Claude skills | `/root/.claude/skills` | Skills (read-only) |
| Agent skills | `/root/.agents/skills` | Agent skills (read-only) |
| .netrc | `/root/.netrc` | Git credentials (full image only) |
| gh_hosts.yml | `/root/.config/gh/hosts.yml` | GitHub CLI auth (full image only) |

### Project Source Mounts

To give the AI agent access to project source code, mount directories into
the thread workspace. Customize per your channel/thread layout:

```yaml
volumes:
  - /path/to/project:/opt/jyc/<channel>/workspace/<thread>/project
```

## Troubleshooting

### Container starts but JYC doesn't run
Check container logs:
```bash
docker compose logs -f jyc
```

### OpenCode not found
The install script may fail behind a proxy. Build with proxy args:
```bash
docker build --build-arg HTTP_PROXY=http://proxy:8080 --build-arg HTTPS_PROXY=http://proxy:8080 -t jyc:latest -f docker/Dockerfile .
```

### Skills not available
Ensure the skills directories are mounted and readable:
```bash
docker exec -it jyc ls /root/.claude/skills/
docker exec -it jyc ls /root/.agents/skills/
```

## OAuth2 Authentication

Some MCP servers (e.g., Jira, Figma) require interactive browser-based OAuth2 login flows. The `slim` image does not include `oauth2-forwarder`. If you need OAuth2 support inside containers, use the `full` image or install `oauth2-forwarder` on the host.

### Host-Side OAuth2 Forwarder

**1. Install oauth2-forwarder on your host:**

```bash
npm install -g oauth2-forwarder
```

**2. Start the host server:**

```bash
cd docker
./o2f-server.sh
```

By default, the server listens on port 9191. To use a different port:

```bash
./o2f-server.sh 8080
# or
OAUTH2_FORWARDER_PORT=8080 ./o2f-server.sh
```

**3. Verify the connection:**

The container shares host networking (`network_mode: host`), so it can reach `o2f-server` at `localhost:9191` automatically.

### Disabling OAuth2 Forwarder

To disable it and use the system default browser:

```bash
OAUTH2_FORWARDER_BROWSER=
```

in your `docker/.env` file. This tells tools inside the container to use their default browser behavior instead of routing through `o2f-browser`.

### Passthrough Mode

Passthrough mode (`OAUTH2_FORWARDER_PASSTHROUGH=true`) is enabled by default. This allows MCP tools that use device code flows (where the user authenticates on a different device or via a code) to work correctly. If you encounter issues with redirect-based OAuth2 flows, you may need to adjust this setting.

### Troubleshooting

**Connection refused when opening browser:**
- Verify `o2f-server` is running on the host: `curl http://localhost:9191`
- Check the port matches `OAUTH2_FORWARDER_PORT` in your `.env`

**Port conflicts:**
- If port 9191 is in use, specify a different port: `./o2f-server.sh 9192`
- Update `OAUTH2_FORWARDER_PORT` in your `.env` to match
