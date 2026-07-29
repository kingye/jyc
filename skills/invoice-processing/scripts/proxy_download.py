#!/usr/bin/env python3
"""Download files from geo-blocked invoice platforms.

Tries direct download first. If blocked (WAF 405/403), falls back to the
Shanghai proxy. This means:
- On a mainland China server: direct download works, no proxy needed
- On an overseas server (HK, etc.): auto-falls back to Shanghai proxy

Usage:
    python3 proxy_download.py <url> <output_file>
    python3 proxy_download.py <url> <output_file> --proxy <proxy_url>

Output (JSON to stdout):
    {"success": true, "file": "invoice.pdf", "size": 50972, "content_type": "application/pdf", "method": "direct"}
    {"success": true, "file": "invoice.pdf", "size": 50972, "content_type": "application/pdf", "method": "proxy"}
    {"success": false, "error": "Both direct and proxy download failed"}
"""

from __future__ import annotations

import json
import sys
import urllib.parse
import urllib.request

# Proxy URL: set via environment variable INVOICE_DOWNLOAD_PROXY
# If not set, proxy fallback is disabled (direct download only).
# Example: export INVOICE_DOWNLOAD_PROXY=http://150.158.50.252:8765
import os
DEFAULT_PROXY = os.environ.get("INVOICE_DOWNLOAD_PROXY", "")

USER_AGENT = (
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 "
    "(KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36"
)


def _download_direct(url: str, output_file: str) -> dict:
    """Try direct download with browser headers."""
    req = urllib.request.Request(url, headers={
        "User-Agent": USER_AGENT,
        "Accept": "text/html,application/xhtml+xml,application/xml;q=0.9,application/pdf,*/*;q=0.8",
    })
    try:
        resp = urllib.request.urlopen(req, timeout=15)
        content = resp.read()
        content_type = resp.headers.get("Content-Type", "unknown")

        # Check if response is a WAF block page (small HTML with error codes)
        if len(content) < 5000 and b"405" in content and b"blocked" in content.lower():
            return {"success": False, "error": "WAF blocked (405)"}

        with open(output_file, "wb") as f:
            f.write(content)

        return {
            "success": True,
            "file": output_file,
            "size": len(content),
            "content_type": content_type,
            "method": "direct",
        }
    except urllib.error.HTTPError as e:
        return {"success": False, "error": f"HTTP {e.code}"}
    except Exception as e:
        return {"success": False, "error": str(e)}


def _download_via_proxy(url: str, output_file: str, proxy_url: str) -> dict:
    """Download via the Shanghai proxy."""
    encoded_url = urllib.parse.quote(url, safe="")
    fetch_url = f"{proxy_url}/fetch?url={encoded_url}"

    try:
        req = urllib.request.Request(fetch_url)
        resp = urllib.request.urlopen(req, timeout=30)
        content = resp.read()
        content_type = resp.headers.get("Content-Type", "unknown")

        with open(output_file, "wb") as f:
            f.write(content)

        return {
            "success": True,
            "file": output_file,
            "size": len(content),
            "content_type": content_type,
            "method": "proxy",
        }
    except urllib.error.HTTPError as e:
        error_body = e.read().decode("utf-8", errors="ignore")[:200]
        return {"success": False, "error": f"Proxy HTTP {e.code}: {error_body}"}
    except Exception as e:
        return {"success": False, "error": f"Proxy error: {e}"}


def download(url: str, output_file: str, proxy_url: str = DEFAULT_PROXY) -> dict:
    """Download a URL. Try direct first, fall back to proxy if blocked."""
    # Try direct download first
    result = _download_direct(url, output_file)
    if result["success"]:
        return result

    direct_error = result["error"]

    # Direct failed — try via proxy (if configured)
    if not proxy_url:
        return {
            "success": False,
            "error": f"Direct download failed: {direct_error}. No proxy configured (set INVOICE_DOWNLOAD_PROXY env var).",
        }

    result = _download_via_proxy(url, output_file, proxy_url)
    if result["success"]:
        return result

    # Both failed
    return {
        "success": False,
        "error": f"Both methods failed. Direct: {direct_error}. Proxy: {result['error']}",
    }


def main():
    if len(sys.argv) < 3:
        print(json.dumps({
            "success": False,
            "error": "Usage: proxy_download.py <url> <output_file> [--proxy <proxy_url>]"
        }))
        sys.exit(1)

    url = sys.argv[1]
    output_file = sys.argv[2]
    proxy_url = DEFAULT_PROXY

    if "--proxy" in sys.argv:
        idx = sys.argv.index("--proxy")
        if idx + 1 < len(sys.argv):
            proxy_url = sys.argv[idx + 1]

    result = download(url, output_file, proxy_url)
    print(json.dumps(result, ensure_ascii=False))

    if not result["success"]:
        sys.exit(1)


if __name__ == "__main__":
    main()
