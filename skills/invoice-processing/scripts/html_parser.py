"""Lightweight HTML parser for extracting PDF download URLs from invoice pages.

This is the primary parser. It uses only Python stdlib (no external dependencies).
When this fails, the system automatically falls back to playwright_extractor.py.

Usage:
    python3 html_parser.py <html_file> [<base_url>]

Output (JSON to stdout):
    {"success": true, "pdf_url": "https://..."}
    {"success": false}

Known invoice platforms and their extraction strategies:

  Platform          | Domain              | Strategy | Notes
  ------------------|---------------------|----------|---------------------------
  51发票            | dlj.51fapiao.cn     | 5 + 6    | Hidden inputs + PDF.js viewer
  每刻云票 (Maycur) | pms.maycur.com      | SKIP     | React SPA → playwright_extractor.py

If the URL matches a known Playwright-only platform (e.g., Maycur), this parser
returns failure immediately so the system falls through to playwright_extractor.py
without wasting time on regex strategies.
"""

from __future__ import annotations

import json
import re
import sys
import urllib.parse
from pathlib import Path

# ---------------------------------------------------------------------------
# Platforms that require Playwright (React SPAs, JS-heavy pages).
# html_parser.py cannot extract URLs from these — skip immediately.
# ---------------------------------------------------------------------------
_PLAYWRIGHT_ONLY_DOMAINS = [
    "pms.maycur.com",  # 每刻云票 — React SPA, button text "PDF下载"
]


def _is_playwright_only_platform(base_url: str) -> bool:
    """Check if URL belongs to a platform that requires Playwright."""
    for domain in _PLAYWRIGHT_ONLY_DOMAINS:
        if domain in base_url:
            return True
    return False


def extract_pdf_url(html_content: str, base_url: str = "") -> str | None:
    """Try to extract a PDF/image download URL from HTML content.

    Strategies (in order):
    1. Direct href/src links to PDF/image files
    2. JavaScript variable patterns (downloadUrl, fileUrl, etc.)
    3. API endpoint patterns (/api/download/, /invoice/pdf/, etc.)
    4. data-* attribute patterns
    5. Hidden input fields + JS download URL construction
    6. PDF.js viewer iframe file parameter
    """
    # Fast-path: skip entirely for known Playwright-only platforms
    if _is_playwright_only_platform(base_url):
        return None

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

    # Strategy 5: Hidden input fields + JS download URL construction
    url = _find_hidden_input_download(html_content, base_url)
    if url:
        return url

    # Strategy 6: PDF.js viewer iframe file parameter
    url = _find_pdfjs_viewer_url(html_content, base_url)
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
            if any(skip in match.lower() for skip in ["favicon", "icon", "logo", "qr", "ewm", "qrcode"]):
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
    skip_words = ["favicon", "icon", "logo", "qr", "ewm", "qrcode", "images/code"]
    for pattern in patterns:
        matches = re.findall(pattern, html, re.IGNORECASE)
        for match in matches:
            if any(skip in match.lower() for skip in skip_words):
                continue
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


def _find_hidden_input_download(html: str, base_url: str) -> str | None:
    """Extract download URL from hidden input fields + JS URL construction.

    Generic pattern: pages that store download parameters in <input type="hidden">
    elements and construct download URLs in JavaScript.

    Known platforms using this pattern:
    - 51fapiao.cn: inputs dlj + signatureString
      → /dlj/v7/downloadFile/{dlj}?signatureString={signatureString}

    Approach:
    1. Extract all hidden input id:value pairs
    2. Find JS code that constructs download URLs referencing those IDs
    3. Substitute variable references with the extracted values
    4. Resolve against base_url
    """
    # Step 1: Extract hidden input id:value pairs
    hidden_inputs = {}
    for match in re.finditer(
        r'<input[^>]*type=["\']hidden["\'][^>]*>', html, re.IGNORECASE
    ):
        tag = match.group(0)
        id_match = re.search(r'id=["\']([^"\']+)["\']', tag)
        val_match = re.search(r'value=["\']([^"\']*)["\']', tag)
        if id_match and val_match:
            hidden_inputs[id_match.group(1)] = val_match.group(1)

    if not hidden_inputs:
        return None

    # Step 2: Find JS patterns that construct download URLs using hidden input IDs
    # Pattern A: window.location.href = "/path/" + dlj + "?param=" + signatureString
    #   from downPdf(): window.location.href = "/dlj/v7/downloadFile/" + dlj + "?signatureString=" + signatureString
    href_patterns = [
        # window.location.href = "..." + var + "..." + var
        r'window\.location\.href\s*=\s*"([^"]*)"(?:\s*\+\s*(\w+)(?:\s*\+\s*"([^"]*)"(?:\s*\+\s*(\w+))?)?)?',
        # Direct string: "/path/downloadFile/" + var + "?signatureString=" + var
        r'"(/[^"]*download[Ff]ile/)"?\s*\+\s*(\w+)\s*\+\s*"(\?[^"]*=)"\s*\+\s*(\w+)',
    ]

    for pattern in href_patterns:
        for match in re.finditer(pattern, html, re.IGNORECASE):
            groups = match.groups()
            # Build URL by substituting variable names with hidden input values
            url_parts = []
            for part in groups:
                if part is None:
                    continue
                if part in hidden_inputs:
                    url_parts.append(hidden_inputs[part])
                else:
                    url_parts.append(part)
            if url_parts:
                constructed = "".join(url_parts)
                # Must look like a download path (not just any JS expression)
                if "download" in constructed.lower() or "file" in constructed.lower():
                    return _resolve_url(constructed, base_url)

    # Step 3: Fallback — look for encodeURIComponent wrapping a download path
    # var downpath = encodeURIComponent("/dlj/v7/downloadFile/" + dlj + "?signatureString=" + sig + "&downflag=0&wjlx=.pdf")
    encode_pattern = r'encodeURIComponent\(\s*"([^"]*)"(?:\s*\+\s*(\w+)(?:\s*\+\s*"([^"]*)"(?:\s*\+\s*(\w+)(?:\s*\+\s*"([^"]*)")?)?)?)?'
    for match in re.finditer(encode_pattern, html, re.IGNORECASE):
        groups = match.groups()
        url_parts = []
        for part in groups:
            if part is None:
                continue
            if part in hidden_inputs:
                url_parts.append(hidden_inputs[part])
            else:
                url_parts.append(part)
        if url_parts:
            constructed = "".join(url_parts)
            if "download" in constructed.lower() or "file" in constructed.lower():
                return _resolve_url(constructed, base_url)

    return None


def _find_pdfjs_viewer_url(html: str, base_url: str) -> str | None:
    """Extract PDF URL from PDF.js viewer iframe or script assignment.

    Generic pattern: pages that embed PDFs using Mozilla's PDF.js viewer:
      <iframe src="/path/pdfjs/web/viewer.html?file=<encoded_url>">
    or via JavaScript:
      $('#iframe').attr('src', '/path/viewer.html?file=' + encodedUrl);

    Known platforms:
    - 51fapiao.cn: iframe src="/dlj/v7/pdfjs/web/viewer.html?file=..."

    The file= parameter is typically URL-encoded (sometimes double-encoded via
    encodeURIComponent). We decode it to get the actual PDF URL.
    """
    # Pattern 1: Find viewer.html?file= in iframe src or JS string
    patterns = [
        # iframe src="...viewer.html?file=..."
        r'src=["\']([^"\']*viewer\.html\?file=[^"\']*)["\']',
        # JS assignment: .attr('src', '...viewer.html?file=...')
        r"['\"]([^'\"]*viewer\.html\?file=[^'\"]*)['\"]",
    ]

    for pattern in patterns:
        matches = re.findall(pattern, html, re.IGNORECASE)
        for match in matches:
            # Extract the file= parameter value
            parsed = urllib.parse.urlparse(match)
            params = urllib.parse.parse_qs(parsed.query)
            file_url = params.get("file", [None])[0]
            if file_url:
                # URL-decode (handles encodeURIComponent double-encoding)
                decoded = urllib.parse.unquote(file_url)
                return _resolve_url(decoded, base_url)

    # Pattern 2: JS constructs viewer URL with file= from encodeURIComponent
    # var srcpath = "/path/viewer.html?file=" + downpath;
    # where downpath was already captured by Strategy 5
    # This is a fallback if Strategy 5 didn't match but we can find the viewer pattern
    viewer_js = re.search(
        r'viewer\.html\?file="\s*\+\s*(\w+)', html, re.IGNORECASE
    )
    if viewer_js:
        var_name = viewer_js.group(1)
        # Try to find the variable value in script
        var_pattern = rf'var\s+{re.escape(var_name)}\s*=\s*encodeURIComponent\(\s*"([^"]*)"'
        var_match = re.search(var_pattern, html)
        if var_match:
            encoded_path = var_match.group(1)
            return _resolve_url(encoded_path, base_url)

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
