#!/usr/bin/env python3
"""Lightweight download proxy for geo-blocked invoice platforms.

Runs on a mainland China server to proxy download requests from overseas
servers that are blocked by CDN WAF (e.g., Alibaba Cloud blocks HK IPs).

Usage:
    python3 download_proxy.py [--port PORT] [--bind BIND]

API:
    GET /fetch?url=<encoded_url>
        Downloads the URL and returns the content with original content-type.
        Only whitelisted domains are allowed.

    GET /health
        Returns {"status": "ok"}

Security:
    - Only whitelisted domains are proxied (no open proxy)
    - Rate limited to prevent abuse
    - No authentication (runs on internal network)
"""

from __future__ import annotations

import argparse
import json
import time
import urllib.parse
import urllib.request
from http.server import HTTPServer, BaseHTTPRequestHandler

# Only proxy requests to these domains
ALLOWED_DOMAINS = [
    "dlj.51fapiao.cn",
    "ei.51fapiao.cn",
    "pms.maycur.com",
]

# Rate limiting: max requests per minute
MAX_REQUESTS_PER_MINUTE = 30
_request_times: list[float] = []

USER_AGENT = (
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 "
    "(KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36"
)


def is_allowed_domain(url: str) -> bool:
    """Check if the URL's domain is in the whitelist."""
    try:
        parsed = urllib.parse.urlparse(url)
        return any(parsed.hostname == d or (parsed.hostname and parsed.hostname.endswith("." + d))
                    for d in ALLOWED_DOMAINS)
    except Exception:
        return False


def is_rate_limited() -> bool:
    """Simple rate limiter."""
    now = time.time()
    _request_times[:] = [t for t in _request_times if now - t < 60]
    if len(_request_times) >= MAX_REQUESTS_PER_MINUTE:
        return True
    _request_times.append(now)
    return False


def fetch_url(url: str) -> tuple[bytes, str, int]:
    """Fetch a URL and return (content, content_type, status_code)."""
    req = urllib.request.Request(url, headers={
        "User-Agent": USER_AGENT,
        "Accept": "text/html,application/xhtml+xml,application/xml;q=0.9,application/pdf,*/*;q=0.8",
    })
    try:
        resp = urllib.request.urlopen(req, timeout=30)
        content = resp.read()
        content_type = resp.headers.get("Content-Type", "application/octet-stream")
        return content, content_type, resp.status
    except urllib.error.HTTPError as e:
        return e.read(), "text/html", e.code
    except Exception as e:
        return str(e).encode(), "text/plain", 502


class ProxyHandler(BaseHTTPRequestHandler):
    def do_GET(self):
        parsed = urllib.parse.urlparse(self.path)
        params = urllib.parse.parse_qs(parsed.query)

        if parsed.path == "/health":
            self._respond(200, "application/json", json.dumps({"status": "ok"}).encode())
            return

        if parsed.path == "/fetch":
            url = params.get("url", [None])[0]
            if not url:
                self._respond(400, "application/json",
                              json.dumps({"error": "Missing 'url' parameter"}).encode())
                return

            if not is_allowed_domain(url):
                self._respond(403, "application/json",
                              json.dumps({"error": f"Domain not allowed. Allowed: {ALLOWED_DOMAINS}"}).encode())
                return

            if is_rate_limited():
                self._respond(429, "application/json",
                              json.dumps({"error": "Rate limit exceeded"}).encode())
                return

            content, content_type, status = fetch_url(url)
            self._respond(status, content_type, content)
            return

        self._respond(404, "application/json",
                      json.dumps({"error": "Not found. Use /fetch?url=... or /health"}).encode())

    def _respond(self, status: int, content_type: str, body: bytes):
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, format, *args):
        """Override to add timestamp."""
        print(f"[{time.strftime('%Y-%m-%d %H:%M:%S')}] {args[0]}")


def main():
    parser = argparse.ArgumentParser(description="Download proxy for geo-blocked invoice platforms")
    parser.add_argument("--port", type=int, default=8765, help="Port to listen on (default: 8765)")
    parser.add_argument("--bind", default="0.0.0.0", help="Address to bind to (default: 0.0.0.0)")
    args = parser.parse_args()

    server = HTTPServer((args.bind, args.port), ProxyHandler)
    print(f"Download proxy started on {args.bind}:{args.port}")
    print(f"Allowed domains: {ALLOWED_DOMAINS}")
    print(f"Health check: http://{args.bind}:{args.port}/health")
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("\nShutting down...")
        server.server_close()


if __name__ == "__main__":
    main()
