# Web UI Dashboard

JYC includes an adaptive browser-based dashboard integrated into the inspect
server. It is served on the same port as the inspect API (default `9876`)
and requires no additional configuration, ports, or processes.

## Accessing the Dashboard

1. Start `jyc serve`
2. Open `http://127.0.0.1:9876/` in your browser

> **No token file configured?** The dashboard is fully accessible without
> authentication. See "Authentication" below for token-based setups.

## Features

### Channel & Thread Listing

The left sidebar lists all configured channels (grouped) and their active
threads. Each thread shows its status indicator and last activity time.

### Chat

Click any thread to open the chat pane on the right. Messages are displayed
in a conversation view with user messages (blue, right-aligned) and AI
replies (dark, left-aligned).

**Channel types:**

| Channel type | Chat mechanism |
|---|---|
| WebSocket (local_dev) | Real-time WebSocket with history loading |
| email, feishu, github, etc. | `POST /inject_message` + 5s polling via `GET /state` |

### Status Indicators

- **Pulsing blue dot** — thread is processing
- **Red dot** — thread has an error
- **Yellow dot** — thread is queued or waiting for answer
- **Grey dot** — idle

### Responsive Layout

| Viewport | Layout |
|---|---|
| ≥768px (desktop) | Two-column: sidebar + chat pane side by side |
| <768px (mobile) | Single column: sidebar or chat pane at full width, toggled via menu button |

## Authentication

### No Token (Default)

If no `inspect-token` file exists at the data directory, the dashboard is
fully accessible without authentication. The login button is hidden.

### With Token

When authentication is enabled (via `jyc token generate`):

1. Visit `http://127.0.0.1:9876/` — the page loads without auth
2. Click the **Login** button in the top-right corner
3. Paste your token (get it with `jyc token show`) into the dialog
4. The token is stored in `localStorage` and attached to all subsequent
   API and WebSocket requests as `Authorization: Bearer <token>`
5. If the token expires or is invalid, the login dialog reappears automatically

> **Note:** The HTML/CSS/JS pages themselves are served without auth
> (so the login dialog can be displayed). Only API endpoints and
> WebSocket upgrades are behind the auth middleware.

### WebSocket Auth Limitation

The browser `WebSocket` API does **not** support custom HTTP headers
(`Authorization: Bearer ...`). As a result, when a token file is configured:

- **`fetch()` calls** send the token via the `Authorization` header — works fine
- **`/ws/{channel}`** upgrade is rejected with 401 — chat **silently falls back
  to polling** via `POST /inject_message` + `GET /state` every 5 seconds

Messaging still works in both modes; only the real-time push channel is
degraded. WebSocket chat without auth works normally. The WebSocket connection
auto-reconnects on disconnect with exponential backoff (capped at 30s).

## Architecture

The web UI is implemented as a pure Rust static content provider in
[`crates/jyc-web/`](../crates/jyc-web/). It uses:

- **`include_str!`** — embeds HTML, CSS, and JS directly into the binary
  at compile time (no Node/npm, no build step)
- **vanilla JavaScript** — ~500 lines, no frameworks, no dependencies
- **CSS Grid** — responsive layout via media queries, no JavaScript for layout
- **Askama** — not used; all pages are fully static (rendered client-side)

The crate has zero Rust dependencies and only exposes static string constants
(`INDEX_HTML`, `THREAD_HTML`, `STYLE_CSS`, `APP_JS`, `NOT_FOUND_HTML`).

HTTP handlers live in [`crates/jyc-inspect/src/server.rs`](../crates/jyc-inspect/src/server.rs)
with the web UI routes placed in a separate router group **before** the auth
middleware layer, so the login page is accessible without credentials.

## Extending

To add a new page or asset:

1. Add the file under `crates/jyc-web/assets/`
2. Export it as a `pub static` in `crates/jyc-web/src/lib.rs`
3. Add a route + handler in `crates/jyc-inspect/src/server.rs`

## Troubleshooting

| Symptom | Likely cause |
|---|---|
| Blank page / no content | Token required — click Login in top-right corner |
| "Failed to connect" | `jyc serve` not running, or wrong port |
| Chat doesn't send | Ensure you've selected a thread first |
| WS channel shows "Connecting..." then falls back | WebSocket handshake failed; falls back to polling automatically |
| Chat is delayed ~5s | If auth is enabled, polling is used instead of WebSocket (browser API limitation) |
