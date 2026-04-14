"""Lightweight HTML parser for extracting PDF download URLs from invoice pages.

This is the primary parser. It uses only Python stdlib (no external dependencies).
When this fails, the system automatically falls back to playwright_extractor.py.

Usage:
    python3 html_parser.py <html_file> [<base_url>]

Output (JSON to stdout):
    {"success": true, "pdf_url": "https://..."}
    {"success": false}
"""

import json
import re
import sys
import urllib.parse
from pathlib import Path


def extract_pdf_url(html_content: str, base_url: str = "") -> str | None:
    """Try to extract a PDF/image download URL from HTML content.

    Strategies (in order):
    1. Direct href/src links to PDF/image files
    2. JavaScript variable patterns (downloadUrl, fileUrl, etc.)
    3. API endpoint patterns (/api/download/, /invoice/pdf/, etc.)
    4. data-* attribute patterns
    """
    # Strategy 1: Direct links
    url = _find_direct_links(html_content, base_url)
    if url:
        return url

    # Strategy 2: JavaScript variables
    url = _find_js_variables(html_content, base_url)
    if url:
        return url

    # Strategy 3: API endpoints
    url = _find_api_endpoints(html_content, base_url)
    if url:
        return url

    # Strategy 4: data-* attributes
    url = _find_data_attributes(html_content, base_url)
    if url:
        return url

    return None


def _resolve_url(url: str, base_url: str) -> str:
    """Resolve a relative URL against a base URL."""
    if not url:
        return ""
    if url.startswith("http://") or url.startswith("https://"):
        return url
    if base_url:
        return urllib.parse.urljoin(base_url, url)
    return url


def _find_direct_links(html: str, base_url: str) -> str | None:
    """Find direct href/src links to PDF or image files."""
    patterns = [
        r'href=["\']([^"\']*\.(?:pdf|PDF)(?:\?[^"\']*)?)["\']',
        r'src=["\']([^"\']*\.(?:pdf|PDF)(?:\?[^"\']*)?)["\']',
        r'href=["\']([^"\']*\.(?:jpg|JPG|jpeg|JPEG|png|PNG)(?:\?[^"\']*)?)["\']',
    ]
    for pattern in patterns:
        matches = re.findall(pattern, html)
        for match in matches:
            # Skip tiny images (likely icons/QR codes)
            if any(skip in match.lower() for skip in ["favicon", "icon", "logo", "qr"]):
                continue
            return _resolve_url(match, base_url)
    return None


def _find_js_variables(html: str, base_url: str) -> str | None:
    """Find PDF URLs in JavaScript variable assignments."""
    patterns = [
        r'"download(?:Url|URL|Link)"\s*:\s*"([^"]+\.(?:pdf|jpg|png)(?:\?[^"]*)?)"',
        r'"file(?:Url|URL|Link)"\s*:\s*"([^"]+\.(?:pdf|jpg|png)(?:\?[^"]*)?)"',
        r'"pdf(?:Url|URL|Link)"\s*:\s*"([^"]+)"',
        r'"invoice(?:Url|URL|Link)"\s*:\s*"([^"]+\.(?:pdf|jpg|png)(?:\?[^"]*)?)"',
        r"var\s+\w*(?:pdf|download|file|invoice)\w*\s*=\s*[\"']([^\"']+\.(?:pdf|jpg|png))[\"']",
        r"window\.location\.(?:href|assign)\s*=\s*[\"']([^\"']+\.(?:pdf|jpg|png))[\"']",
    ]
    for pattern in patterns:
        matches = re.findall(pattern, html, re.IGNORECASE)
        for match in matches:
            return _resolve_url(match, base_url)
    return None


def _find_api_endpoints(html: str, base_url: str) -> str | None:
    """Find API endpoint URLs that look like invoice download endpoints."""
    patterns = [
        r'"(https?://[^"]*(?:download|invoice|fapiao|pdf)[^"]*\.(?:pdf|jpg|png)[^"]*)"',
        r"'(https?://[^']*(?:download|invoice|fapiao|pdf)[^']*\.(?:pdf|jpg|png)[^']*)'",
        r'"(/api/[^"]*(?:download|invoice|pdf)[^"]*)"',
    ]
    for pattern in patterns:
        matches = re.findall(pattern, html, re.IGNORECASE)
        for match in matches:
            return _resolve_url(match, base_url)
    return None


def _find_data_attributes(html: str, base_url: str) -> str | None:
    """Find PDF URLs in data-* attributes."""
    patterns = [
        r'data-(?:pdf|file|download|invoice)-url=["\']([^"\']+)["\']',
        r'data-(?:pdf|file|download|invoice)-src=["\']([^"\']+)["\']',
        r'data-url=["\']([^"\']*\.(?:pdf|jpg|png)(?:\?[^"\']*)?)["\']',
    ]
    for pattern in patterns:
        matches = re.findall(pattern, html, re.IGNORECASE)
        for match in matches:
            return _resolve_url(match, base_url)
    return None


def main():
    if len(sys.argv) < 2:
        print(json.dumps({"success": False, "error": "Usage: html_parser.py <html_file> [base_url]"}))
        sys.exit(1)

    html_file = sys.argv[1]
    base_url = sys.argv[2] if len(sys.argv) > 2 else ""

    try:
        content = Path(html_file).read_text(encoding="utf-8", errors="ignore")
    except Exception as e:
        print(json.dumps({"success": False, "error": str(e)}))
        sys.exit(1)

    pdf_url = extract_pdf_url(content, base_url)
    if pdf_url:
        print(json.dumps({"success": True, "pdf_url": pdf_url}))
    else:
        print(json.dumps({"success": False}))


if __name__ == "__main__":
    main()
