"""Extract invoice download URLs from the latest received message in chat history.

The incoming message prompt may be truncated (forwarded content stripped).
The full email body including forwarded URLs is saved in chat_history_*.md.
This script reads the LATEST received message and extracts invoice URLs.

Usage:
    python3 extract_urls.py [<thread_directory>]

    If no directory given, uses the current working directory.

Output (one line per URL found):
    INVOICE_URL: https://dlj.51fapiao.cn/dlj/v7/...
    FILE_URL: https://example.com/invoice.pdf
    NO_URLS_FOUND
    NO_CHAT_HISTORY
    NO_RECEIVED_MESSAGE
"""

from __future__ import annotations

import glob
import re
import sys
from pathlib import Path


def extract_urls(thread_dir: str = ".") -> list[tuple[str, str]]:
    """Extract invoice URLs from the latest received message.

    Returns list of (type, url) tuples where type is INVOICE_URL or FILE_URL.
    """
    # Find the latest chat history file
    pattern = str(Path(thread_dir) / "chat_history_*.md")
    files = sorted(glob.glob(pattern))
    if not files:
        return [("NO_CHAT_HISTORY", "")]

    content = open(files[-1], encoding="utf-8").read()

    # Split by --- separator and find the last "type:received" block
    blocks = content.split("\n---\n")
    last_received = None
    for block in reversed(blocks):
        if "type:received" in block:
            last_received = block
            break

    if not last_received:
        return [("NO_RECEIVED_MESSAGE", "")]

    # Extract URLs from both markdown link syntax [text](url) and plain text
    md_urls = re.findall(r'\[.*?\]\((https?://[^)]+)\)', last_received)
    plain_urls = re.findall(r'https?://[^\s<>"\')\]]+', last_received)

    # Combine and deduplicate, preserving order
    seen: set[str] = set()
    urls: list[str] = []
    for url in md_urls + plain_urls:
        if url not in seen:
            seen.add(url)
            urls.append(url)

    # Classify URLs — prioritize actual invoice download links
    results: list[tuple[str, str]] = []
    for url in urls:
        lower = url.lower()
        # Skip: platform homepages, login pages, logos, icons, tracking, CDN assets
        if any(skip in lower for skip in [
            'www.51fapiao', 'ei.51fapiao',   # 51fapiao homepage and CDN
            'app.aisino.cn',                  # 51fapiao ad/image CDN
            'tydl-login', 'login', 'register',  # login/register pages
            'logo', 'icon', 'favicon', 'pixel', 'track', 'unsubscribe',
            '.css', '.js', '.ico',            # static assets
            'ad_slot', 'ad_resource',         # advertisement URLs
        ]):
            continue
        # 51fapiao invoice download links: dlj.51fapiao.cn/dlj/v7/<hash>
        if 'dlj.51fapiao.cn/dlj/' in lower:
            results.append(("INVOICE_URL", url))
        # Maycur invoice links: pms.maycur.com/supply/#/invoice-download
        elif 'maycur.com' in lower and 'invoice' in lower:
            results.append(("INVOICE_URL", url))
        # Other download/invoice URLs (unknown platforms)
        elif any(kw in lower for kw in ['download', 'invoice']):
            if not any(skip in lower for skip in ['51fapiao', 'maycur']):
                results.append(("INVOICE_URL", url))
        # Direct file links
        elif lower.endswith(('.pdf', '.jpg', '.png')):
            results.append(("FILE_URL", url))

    if not results:
        return [("NO_URLS_FOUND", "")]
    return results


def main():
    thread_dir = sys.argv[1] if len(sys.argv) > 1 else "."
    results = extract_urls(thread_dir)
    for tag, url in results:
        if url:
            print(f"{tag}: {url}")
        else:
            print(tag)


if __name__ == "__main__":
    main()
