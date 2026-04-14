"""Playwright-based PDF URL extractor for complex invoice HTML pages.

This is the fallback extractor. It is used automatically when html_parser.py
fails to extract a PDF URL (e.g. JavaScript-heavy React pages).

Requires: pip install playwright && python3 -m playwright install chromium

Usage:
    python3 playwright_extractor.py <url>

Output (JSON to stdout):
    {"success": true, "pdf_url": "https://..."}
    {"success": false, "error": "..."}
"""

import json
import re
import sys
import urllib.parse


def extract_pdf_url(url: str) -> str | None:
    """Extract PDF download URL from a JavaScript-heavy HTML page.

    Strategies (in order):
    1. Intercept network requests for PDF files
    2. Find and click download buttons
    3. Search rendered DOM for PDF links
    4. Search page content for URL patterns
    """
    try:
        from playwright.sync_api import sync_playwright
    except ImportError:
        return None

    with sync_playwright() as p:
        browser = p.chromium.launch(headless=True)
        try:
            return _extract_with_browser(browser, url)
        finally:
            browser.close()


def _extract_with_browser(browser, url: str) -> str | None:
    """Run extraction strategies using a browser instance."""
    context = browser.new_context(accept_downloads=True)
    page = context.new_page()

    # Intercept network requests - capture any PDF download
    captured_pdf_url = []

    def on_response(response):
        ct = response.headers.get("content-type", "")
        if "application/pdf" in ct or "application/octet-stream" in ct:
            captured_pdf_url.append(response.url)

    page.on("response", on_response)

    try:
        page.goto(url, wait_until="networkidle", timeout=20000)
        page.wait_for_load_state("networkidle")

        # Check if a PDF was already captured during page load
        if captured_pdf_url:
            return captured_pdf_url[0]

        # Strategy 1: Find and click download buttons
        pdf_url = _click_download_button(page, captured_pdf_url)
        if pdf_url:
            return pdf_url

        # Strategy 2: Search rendered DOM for PDF links
        pdf_url = _find_pdf_in_dom(page)
        if pdf_url:
            return pdf_url

        # Strategy 3: Search page source for URL patterns
        pdf_url = _find_pdf_in_source(page)
        if pdf_url:
            return pdf_url

    except Exception:
        pass
    finally:
        page.close()
        context.close()

    return None


def _click_download_button(page, captured_pdf_url: list) -> str | None:
    """Find and click buttons that look like PDF download buttons."""
    keywords = ["PDF", "下载", "download", "下载PDF", "导出", "发票下载"]

    for keyword in keywords:
        # Try button elements
        for selector in [
            f'button:has-text("{keyword}")',
            f'a:has-text("{keyword}")',
            f'[role="button"]:has-text("{keyword}")',
        ]:
            try:
                locator = page.locator(selector).first
                if locator.count() > 0 and locator.is_visible():
                    try:
                        with page.expect_download(timeout=10000) as dl_info:
                            locator.click(timeout=5000)
                        download = dl_info.value
                        return download.url
                    except Exception:
                        # Click may have triggered a network request instead
                        if captured_pdf_url:
                            return captured_pdf_url[-1]
            except Exception:
                continue

    return None


def _find_pdf_in_dom(page) -> str | None:
    """Search rendered DOM for PDF links."""
    try:
        links = page.eval_on_selector_all(
            "a[href]",
            """elements => elements.map(e => ({
                href: e.href,
                text: (e.textContent || '').toLowerCase()
            }))""",
        )
        for link in links:
            href = link.get("href", "")
            text = link.get("text", "")
            if not href:
                continue
            if ".pdf" in href.lower():
                return href
            if any(kw in text for kw in ["pdf", "下载", "download", "发票"]):
                return href
    except Exception:
        pass
    return None


def _find_pdf_in_source(page) -> str | None:
    """Search page content for PDF URL patterns."""
    try:
        content = page.content()
    except Exception:
        return None

    patterns = [
        r'"download(?:Url|URL)"\s*:\s*"([^"]+)"',
        r'"file(?:Url|URL)"\s*:\s*"([^"]+)"',
        r'"pdf(?:Url|URL|Link)"\s*:\s*"([^"]+)"',
        r"(https?://[^\s\"']+\.pdf(?:\?[^\s\"']*)?)",
    ]
    for pattern in patterns:
        matches = re.findall(pattern, content, re.IGNORECASE)
        for match in matches:
            if match.startswith("http"):
                return match
            parsed = urllib.parse.urlparse(page.url)
            return f"{parsed.scheme}://{parsed.netloc}{match}"

    return None


def main():
    if len(sys.argv) < 2:
        print(json.dumps({"success": False, "error": "Usage: playwright_extractor.py <url>"}))
        sys.exit(1)

    url = sys.argv[1]
    pdf_url = extract_pdf_url(url)

    if pdf_url:
        print(json.dumps({"success": True, "pdf_url": pdf_url}))
    else:
        print(json.dumps({"success": False, "error": "Could not extract PDF URL"}))


if __name__ == "__main__":
    main()
