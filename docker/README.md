# JYC Docker Deployment

Run JYC as a containerized service.

## Architecture

- **Multi-stage build**: `base` (shared tools) → `builder` (Rust compile) → `production`
- **Single binary**: `jyc` (the MCP reply tool is a hidden subcommand `jyc mcp-reply-tool`).
- **Host networking**: Container shares host network (`network_mode: host`), so services on `localhost` are accessible from inside the container.

## Included Tools

| Tool | Purpose |
|------|---------|
| OpenCode | AI agent runtime |
| git, gh CLI | Version control, GitHub PRs |
| ripgrep, jq | Code search, JSON processing |
| curl | HTTP requests |
| build-essential | C compiler for native deps |
| python3 | Python scripting |
| nodejs (LTS) | JavaScript/TypeScript runtime |
| pandoc | Document conversion |

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

**With Docker/Podman Compose:**
```bash
cd docker
docker compose up --build -d
docker compose logs -f
```

**With Podman (without Compose):**
```bash
# Build
podman build -t jyc:latest -f docker/Dockerfile ..

# Run
podman run -d --name jyc \
  --network=host \
  -v /path/to/jyc-data:/opt/jyc \
  -v /path/to/opencode.jsonc:/root/.config/opencode/opencode.jsonc:ro \
  -v /path/to/.claude/skills:/root/.claude/skills:ro \
  -v /path/to/.agents/skills:/root/.agents/skills:ro \
  -v /path/to/jyc-data/.netrc:/root/.netrc \
  -v /path/to/jyc-data/gh_hosts.yml:/root/.config/gh/hosts.yml \
  --restart unless-stopped \
  jyc:latest
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
| .netrc | `/root/.netrc` | Git credentials |
| gh_hosts.yml | `/root/.config/gh/hosts.yml` | GitHub CLI auth |

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

Some MCP servers (e.g., Jira, Figma) require interactive browser-based OAuth2 login flows that don't work well inside containers by default. JYC includes [oauth2-forwarder](https://github.com/sam-mfb/oauth2-forwarder) to solve this.

### How It Works

- `o2f-browser` (in container): Intercepts browser-open requests and proxies them to the host
- `o2f-client` (in container): Runs inside the container, proxies browser requests
- `o2f-server` (on host): Receives browser requests from the container and opens them in the host's browser

### Setup

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

The forwarder is enabled by default. To disable it and use the system default browser:

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
