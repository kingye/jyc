# Plan: HTML Two-Level Download Support

## Problem

Some invoice emails contain a download link that points to an HTML page
(e.g. a React application) instead of a direct PDF. The HTML page has a
download button that requires JavaScript execution to obtain the real PDF URL.

## Current State

The existing flow handles:
- Email attachments (PDF/image) - works
- Direct download links to PDF - works
- Simple HTML with `href="*.pdf"` links - partially works (grep)

Fails on: JavaScript-heavy pages (e.g. Maycur/每刻云票 React app).

## Solution

Add two-level HTML parsing with automatic fallback:

```
1. Download URL content
2. Check file type
3. If PDF/image → done
4. If HTML:
   a. Try lightweight parsing (scripts/html_parser.py)
   b. If fails → automatically use Playwright (scripts/playwright_extractor.py)
   c. If Playwright not installed → save HTML file
5. User sees only the final result (no intermediate messages)
```

The switch between parsers is silent - the user never sees "switching to
Playwright" or "simple parsing failed". It is a fully automated flow.

## File Structure

```
.opencode/skills/invoice-processing/
├── SKILL.md                          # Main skill doc (references scripts)
├── scripts/
│   ├── html_parser.py               # Lightweight HTML parser (primary)
│   └── playwright_extractor.py      # Playwright-based extractor (fallback)
├── bin/
│   └── install-playwright.sh        # One-time installation script
├── template.xlsx
├── summary.xlsx
└── PLAN-html-download.md            # This file
```

## Module Design

### scripts/html_parser.py
- Input: HTML file path + original URL
- Output: JSON `{"success": true, "pdf_url": "..."}` or `{"success": false}`
- Strategy:
  - grep for direct PDF/image links in href/src attributes
  - Search for common JS variable patterns (downloadUrl, fileUrl, pdfUrl)
  - Search for API endpoint patterns (/api/download/, /invoice/pdf/)
- No external dependencies beyond Python stdlib

### scripts/playwright_extractor.py
- Input: URL of the HTML page
- Output: JSON `{"success": true, "pdf_url": "..."}` or `{"success": false}`
- Strategy:
  - Launch headless Chromium
  - Navigate to URL, wait for networkidle
  - Find download buttons (keywords: PDF, 下载, download, 发票)
  - Click and capture download URL
  - Search page content for PDF links
- Requires: playwright package + chromium browser

### bin/install-playwright.sh
- One-time setup for Debian headless servers
- Installs system dependencies (requires sudo)
- Installs Python playwright package
- Installs Chromium browser
- Runs verification test

## SKILL.md Changes

### Update Step 2: Two-level download handling
Replace current grep-only approach with:
1. Download file, check type
2. If HTML → run `scripts/html_parser.py`
3. If html_parser fails → run `scripts/playwright_extractor.py`
4. If playwright fails or not installed → save as HTML

### New section before Rules:
"### Browser Automation for Complex HTML Pages"
- Explains the automatic fallback mechanism
- Points to `bin/install-playwright.sh` for installation
- Installation steps for Debian headless (with sudo)

## Installation (Debian headless, one-time)

```bash
# Run the install script
bash .opencode/skills/invoice-processing/bin/install-playwright.sh
```

The script does:
```bash
sudo apt-get update
sudo apt-get install -y python3-pip libnss3 libatk-bridge2.0-0 \
    libdrm2 libxkbcommon0 libatspi2.0-0 libgbm1 libasound2
pip3 install playwright
python3 -m playwright install chromium
```

## Risk Assessment

| Risk | Impact | Mitigation |
|------|--------|------------|
| Playwright not installed | Cannot process complex HTML | Save HTML, log warning |
| Page structure changes | Extraction fails | Multiple extraction strategies |
| Memory usage (Chromium) | ~200MB | 8GB server is sufficient |
| Slow processing (5-10s) | Acceptable | Only used as fallback |
