#!/bin/bash
# One-time Playwright installation for Debian headless servers.
# Run with: bash install-playwright.sh
# Requires sudo for system dependencies.
set -e

echo "=== Playwright Installation for Invoice Processing ==="
echo ""

# Step 1: System dependencies
echo "[1/3] Installing system dependencies..."
sudo apt-get update -qq
sudo apt-get install -y -qq \
    python3-pip \
    libnss3 \
    libatk-bridge2.0-0 \
    libdrm2 \
    libxkbcommon0 \
    libatspi2.0-0 \
    libgbm1 \
    libasound2 \
    > /dev/null 2>&1
echo "      Done."

# Step 2: Python package
echo "[2/3] Installing Playwright Python package..."
pip3 install --user playwright > /dev/null 2>&1
echo "      Done."

# Step 3: Chromium browser
echo "[3/3] Installing Chromium browser (this may take a minute)..."
python3 -m playwright install chromium
echo "      Done."

# Verify
echo ""
echo "=== Verification ==="
python3 -c "
from playwright.sync_api import sync_playwright
with sync_playwright() as p:
    browser = p.chromium.launch(headless=True)
    page = browser.new_page()
    page.goto('https://example.com')
    title = page.title()
    browser.close()
    print(f'Playwright OK - test page title: {title}')
"

echo ""
echo "Installation complete."
