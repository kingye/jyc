# Step 2–3: Download and Extract Invoice Data

## ⚠️ CRITICAL: STRICT SEQUENTIAL ORDER — DO NOT SKIP STEPS

You MUST follow this exact order. Do NOT jump to image attachments if PDF
attachments are empty. You MUST try PDF URLs from the email body FIRST.

**Execute these steps one by one. Stop at the FIRST success.**

```
Step 2a: PDF Attachments        → Found PDF? → Extract with PdfReader → Valid? → STOP ✅
         No PDF attachment?     → Go to Step 2b (NOT Step 2d)
                                  ↓
Step 2b: PDF URLs from Email    → Match known platform? → Use platform method directly
         51fapiao.cn?           → curl with browser headers → html_parser.py → download PDF
         maycur.com?            → playwright_extractor.py → download PDF
         Unknown URL?           → Generic download+classify → html_parser/playwright if HTML
         All URLs failed?       → Go to Step 2c (NOT Step 2d)
                                  ↓
Step 2c: Tagged Image URLs      → Use vision MCP → Valid? → STOP ✅
         No tagged URLs?        → Go to Step 2d
                                  ↓
Step 2d: Image Attachments      → Use vision MCP → Valid? → STOP ✅
         No image attachment?   → Go to Step 2e
                                  ↓
Step 2e: Image URLs from Email  → Download → Use vision MCP → Valid? → STOP ✅
         All failed?            → FINAL FAILURE ❌ → Log to errors.jsonl
```

**COMMON MISTAKE:** When no PDF attachments exist, the AI skips directly to
image attachments (Step 2d). This is WRONG. You MUST go to Step 2b (PDF URLs)
first, because the email body often contains a download link to the PDF invoice.

**Key Restrictions:**
- PDF files → Python PdfReader (pypdf) ONLY for extraction. NEVER use vision MCP on PDFs.
- Image files → Vision MCP ONLY. NEVER use PdfReader on images.

---

## Shared Logic

These utilities are used by both PDF Phase and Image Phase.

### URL Extraction from Email Body

**⚠️ CRITICAL: The incoming message may be truncated — forwarded email content
is often stripped. You MUST use the bundled script to find invoice URLs.**

The invoice download URL is often in the **forwarded** part of the email, which
may not appear in the incoming message prompt. The full email body is saved in
`chat_history_*.md`. Use the bundled script to extract URLs from the **latest**
received message only:

```bash
# ⚠️ MANDATORY: Run this script FIRST in Step 2b to find invoice URLs.
# Do NOT use grep, do NOT search email_body.txt, do NOT invent your own approach.
# This script reads chat_history_*.md and extracts URLs from the LATEST message only.
python3 .opencode/skills/invoice-processing/scripts/extract_urls.py
```

**Output example:**
```
INVOICE_URL: https://dlj.51fapiao.cn/dlj/v7/740576b34c03e0c5948a07bae03c654e736270
```

Use the `INVOICE_URL` output to proceed with the platform-specific download method.
If output is `NO_URLS_FOUND`, proceed to Image Phase.

### Download and Classify

Download a URL and determine the file type.

**⚠️ ALWAYS use browser-like headers when downloading invoice URLs.**
Many invoice platforms (e.g., 51fapiao) return 405/403 errors or different
content when requests lack a `User-Agent` header. Always include one:

```bash
curl -sL \
    -H "User-Agent: Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36" \
    -H "Accept: text/html,application/xhtml+xml,application/xml;q=0.9,application/pdf,*/*;q=0.8" \
    -o "invoice_${MONTH}/temp_download" "<url>"

file_type=$(file --brief --mime-type "invoice_${MONTH}/temp_download")
case "$file_type" in
    application/pdf)
        echo "pdf"
        ;;
    image/jpeg|image/png)
        echo "image"
        ;;
    text/html|application/xhtml+xml)
        # Check if it's an error page (small HTML, contains error codes)
        file_size=$(wc -c < "invoice_${MONTH}/temp_download")
        if [ "$file_size" -lt 5000 ]; then
            # Likely an error page (405, 403, etc.) — check content
            if grep -qi "error\|40[0-9]\|not found\|forbidden\|expired" "invoice_${MONTH}/temp_download"; then
                echo "error_page"
            else
                echo "html"
            fi
        else
            echo "html"
        fi
        ;;
    *)
        echo "unknown"
        ;;
esac
```

**If `file_type` is `error_page`:**
- Log as `download_failed` with the HTTP error details
- Do NOT attempt HTML extraction on error pages — they contain no download URL
- Try next URL

### Two-Level HTML Extraction

When a URL returns HTML instead of a direct file, use the bundled scripts to
extract the real download URL:

```bash
# Level 1: Lightweight regex parsing (fast, no dependencies)
result=$(python3 .opencode/skills/invoice-processing/scripts/html_parser.py \
    "invoice_${MONTH}/temp_download" "<original_url>")

# Level 2: Playwright browser automation (fallback for JS-heavy pages)
if echo "$result" | python3 -c "import sys,json; r=json.load(sys.stdin); exit(0 if r.get('success') else 1)" 2>/dev/null; then
    real_url=$(echo "$result" | python3 -c "import sys,json; print(json.load(sys.stdin)['pdf_url'])")
else
    result=$(python3 .opencode/skills/invoice-processing/scripts/playwright_extractor.py "<original_url>")
    if echo "$result" | python3 -c "import sys,json; r=json.load(sys.stdin); exit(0 if r.get('success') else 1)" 2>/dev/null; then
        real_url=$(echo "$result" | python3 -c "import sys,json; print(json.load(sys.stdin)['pdf_url'])")
    fi
fi

# Re-download the real file (if URL was extracted)
if [ -n "$real_url" ]; then
    curl -sL -o "invoice_${MONTH}/temp_download" "$real_url"
    # Re-classify the downloaded file
    file_type=$(file --brief --mime-type "invoice_${MONTH}/temp_download")
else
    # HTML extraction failed — clean up
    rm -f "invoice_${MONTH}/temp_download"
fi
```

The switch between parsers is automatic — no user interaction needed.

### Known Invoice Platforms

**⚠️ IMPORTANT: All known invoice platforms listed below are PUBLIC links.
They do NOT require login or authentication. The URL itself contains all
the credentials needed (hash, code, signatureString, etc.).
NEVER assume an invoice URL requires login — always try to download first.**

| Platform | Domain | Extraction Method | Auth Required? |
|----------|--------|-------------------|----------------|
| 51发票 | **dlj**.51fapiao.cn | html_parser.py ONLY (no Playwright) | **NO** — URL hash is the credential |
| 每刻云票 (Maycur) | pms.maycur.com | playwright_extractor.py ONLY (no html_parser) | **NO** — `code=` param is the credential |

**⚠️ For 51fapiao: ONLY use `dlj.51fapiao.cn` URLs. Ignore `www.51fapiao.cn` (homepage).**
**⚠️ For 51fapiao: ONLY use html_parser.py. Do NOT use playwright_extractor.py.**
**⚠️ For Maycur: ONLY use playwright_extractor.py. html_parser.py auto-skips this domain.**

#### 51fapiao (51发票) — Concrete Example

**51fapiao does NOT require login.** The URL contains the full access hash.
Do NOT skip this URL. Do NOT tell the user it needs login. Just download it.
**Do NOT use playwright_extractor.py for 51fapiao — html_parser.py handles it.**

**⚠️ URL Pattern — use the CORRECT URL:**
- ✅ CORRECT: `https://dlj.51fapiao.cn/dlj/v7/<hash>` — this is the invoice download link
- ❌ WRONG: `https://www.51fapiao.cn/...` — this is the platform homepage, NOT an invoice
- ❌ WRONG: `https://ei.51fapiao.cn/...` — this is the CDN for images/JS, NOT an invoice
- The correct URL always starts with `dlj.51fapiao.cn/dlj/`

**⚠️ You MUST use browser headers for 51fapiao.** Without `User-Agent`, the server
returns a 405 error page instead of the invoice viewer HTML.

When you find a URL matching `dlj.51fapiao.cn/dlj/v7/...`:

1. **Download with browser headers** (MANDATORY — plain `curl` gets 405 error):
   ```bash
   curl -sL \
       -H "User-Agent: Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36" \
       -H "Accept: text/html,application/xhtml+xml,application/xml;q=0.9,application/pdf,*/*;q=0.8" \
       -o "invoice_${MONTH}/temp_download" "<THE_dlj.51fapiao.cn_URL>"
   file --brief --mime-type "invoice_${MONTH}/temp_download"
   # → text/html (the invoice viewer page, NOT a 405 error)
   ```

2. **Run html_parser.py** — it extracts the PDF download URL from hidden inputs:
   ```bash
   result=$(python3 .opencode/skills/invoice-processing/scripts/html_parser.py \
       "invoice_${MONTH}/temp_download" "<THE_dlj.51fapiao.cn_URL>")
   # → {"success": true, "pdf_url": "https://dlj.51fapiao.cn/dlj/v7/downloadFile/<hash>?signatureString=<sig>"}
   ```
   **If html_parser.py fails:** check that you used the correct `dlj.51fapiao.cn` URL
   (not `www.51fapiao.cn`), and that you included browser headers in the curl command.
   Do NOT fall back to playwright — fix the URL and retry.

3. **Download the real PDF** using the extracted URL:
   ```bash
   pdf_url=$(echo "$result" | python3 -c "import sys,json; print(json.load(sys.stdin)['pdf_url'])")
   curl -sL -o "invoice_${MONTH}/temp_download" "$pdf_url"
   file --brief --mime-type "invoice_${MONTH}/temp_download"
   # → application/pdf ✅
   ```

4. **Extract data** from the PDF using Python PdfReader (Step 3a)

#### 每刻云票 (Maycur) — Concrete Example

When you see a URL like `https://pms.maycur.com/supply/#/invoice-download?code=...`:

1. **Download returns HTML** (React SPA shell — no useful content in static HTML)

2. **html_parser.py skips this domain automatically** (returns `{"success": false}`)

3. **playwright_extractor.py handles it**:
   ```bash
   result=$(python3 .opencode/skills/invoice-processing/scripts/playwright_extractor.py \
       "https://pms.maycur.com/supply/#/invoice-download?code=<code>")
   # Playwright renders the React app, finds button "PDF下载", clicks it,
   # captures the PDF download URL
   # → {"success": true, "pdf_url": "https://..."}
   ```

4. **Download the real PDF** and extract with PdfReader

`html_parser.py` automatically skips known Playwright-only platforms (e.g., Maycur)
to avoid wasted processing time — it returns failure immediately so the system
falls through to `playwright_extractor.py`.

**IMPORTANT: Time-limited signatures.**
Some platforms (e.g., 51fapiao) use `signatureString` parameters that may expire.
If a previously valid download URL returns 403/401, log as `download_failed`
with detail `"signature_expired"`. The user may need to re-send the email or
provide a fresh link.

### File Format Validation

Only PDF (`.pdf`) and image (`.jpg`, `.jpeg`, `.png`) files are valid invoice
vouchers. After downloading or locating an attachment, verify:

```bash
file_type=$(file --brief --mime-type "invoice_${MONTH}/temp_download")
case "$file_type" in
    application/pdf|image/jpeg|image/png)
        echo "Valid invoice file: $file_type"
        ;;
    *)
        echo "INVALID: $file_type — not a certified voucher format"
        rm -f "invoice_${MONTH}/temp_download"
        ;;
esac
```

**IMPORTANT:**
- Only PDF and image files (JPG/PNG) are valid certified vouchers (合规凭证)
- HTML, XML, text, or any other format is NOT a valid invoice
- Do NOT use the QR code image as the invoice
- Do NOT follow or scan QR code URLs

### File Naming

After successful extraction (all 3 mandatory fields present), rename the file:

```bash
mv "invoice_${MONTH}/temp_download.pdf" "invoice_${MONTH}/INV-2026-0042.pdf"
```

Naming rules:
- Use the extracted 发票号码 as the filename (e.g., `INV-2026-0042.pdf`)
- Keep the original file extension
- If 发票号码 cannot be extracted, fall back to sequential naming (`invoice_001.pdf`)
- If a file with the same name exists, append a suffix (`INV-2026-0042_2.pdf`)

---

## PDF Phase

Process all PDF sources first. Only proceed to Image Phase if ALL PDF sources fail.

### Step 2a: PDF Attachments

**IMPORTANT: ALWAYS check the attachments directory first.**

Attachments from the email are automatically saved by the system to the
`attachments/` subdirectory in the thread workspace.

```bash
# List PDF attachments (>50KB to skip QR code images)
ls -la attachments/*.pdf 2>/dev/null
```

For each PDF attachment found (>50KB):
1. Copy to monthly folder as temp file
2. Extract data using Python PdfReader (Step 3a below)
3. Validate 3 mandatory fields (销售方税号, 校验码, 价税合计)
4. If ALL 3 fields present → **SUCCESS**, stop processing entirely
5. If any field missing → try next PDF attachment

If no PDF attachments found or all fail validation → **proceed to Step 2b (PDF URLs), NOT Step 2d (Image Attachments)**.

### Step 2b: PDF URLs from Email Body

**⚠️ You MUST reach this step before trying any image sources.**
**If no PDF attachments were found, this is your NEXT step — NOT image attachments.**

**Step 2b-0: Extract URLs using the bundled script (MANDATORY FIRST STEP):**
```bash
python3 .opencode/skills/invoice-processing/scripts/extract_urls.py
```
This outputs `INVOICE_URL: <url>` lines. Use these URLs below.
Do NOT search for URLs yourself — the script handles chat_history parsing.

**Maximum 5 URLs per phase.**

For each URL (up to 5):

**1. FIRST: Check if URL matches a known platform (see Known Invoice Platforms above)**

   **If URL contains `dlj.51fapiao.cn` → Use 51fapiao method directly:**
   ```bash
   # Step 1: Download the viewer HTML with browser headers (MANDATORY)
   curl -sL \
       -H "User-Agent: Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36" \
       -H "Accept: text/html,application/xhtml+xml,application/xml;q=0.9,application/pdf,*/*;q=0.8" \
       -o "invoice_${MONTH}/temp_download" "<THE_dlj.51fapiao.cn_URL>"
   # Step 2: Run html_parser.py to extract the real PDF download URL
   result=$(python3 .opencode/skills/invoice-processing/scripts/html_parser.py \
       "invoice_${MONTH}/temp_download" "<THE_dlj.51fapiao.cn_URL>")
   # Step 3: Download the real PDF
   pdf_url=$(echo "$result" | python3 -c "import sys,json; print(json.load(sys.stdin)['pdf_url'])")
   curl -sL -o "invoice_${MONTH}/temp_download" "$pdf_url"
   # Step 4: Verify it's a PDF, then extract with PdfReader
   ```
   - The URL MUST start with `dlj.51fapiao.cn` — ignore `www.51fapiao.cn` or `ei.51fapiao.cn`
   - Browser headers are MANDATORY — plain `curl` gets 405 error
   - Do NOT use playwright_extractor.py for 51fapiao — html_parser.py handles it
   - If html_parser.py fails → check you used the correct `dlj.` URL and browser headers
   - If valid PDF → extract with PdfReader, validate → SUCCESS

   **If URL contains `pms.maycur.com` → Use Maycur method directly:**
   ```bash
   # Skip html_parser.py — go straight to Playwright
   result=$(python3 .opencode/skills/invoice-processing/scripts/playwright_extractor.py "<maycur_url>")
   pdf_url=$(echo "$result" | python3 -c "import sys,json; print(json.load(sys.stdin)['pdf_url'])")
   curl -sL -o "invoice_${MONTH}/temp_download" "$pdf_url"
   ```
   - Do NOT download HTML first — Playwright handles the full page
   - If valid PDF → extract with PdfReader, validate → SUCCESS

**2. If URL does NOT match any known platform → Use generic download+classify:**

   Download and classify the file type (see Shared Logic: Download and Classify).

   Based on file type:

   **If PDF:**
   - Extract data using Python PdfReader (Step 3a)
   - Validate 3 mandatory fields
   - If valid → **SUCCESS**, stop processing
   - If invalid → try next URL

   **If Image:**
   - **Do NOT process here** — tag this URL for the Image Phase
   - Record: `{"url": "<url>", "path": "<downloaded_path>", "tagged_for": "image_phase"}`
   - Continue to next URL

   **If HTML:**
   - This may be an unknown invoice platform
   - Use Two-Level HTML Extraction (see Shared Logic above):
     1. Run `html_parser.py` with the downloaded HTML file and the original URL
     2. If it returns `{"success": true, "pdf_url": "..."}` → download the `pdf_url`
     3. If it returns `{"success": false}` → run `playwright_extractor.py` with the original URL
     4. If Playwright also fails → log as `download_failed`, try next URL
   - After extraction, re-download the real URL and re-classify:
     - If PDF → extract with PdfReader, validate
     - If Image → tag for Image Phase
     - If still HTML or unknown → log as `download_failed`, try next URL

   **If error_page (405/403/etc.):**
   - Log as `download_failed` with error details
   - Try next URL

   **If unknown/invalid:**
   - Log as `download_failed`
   - Remove temp file
   - Try next URL

3. If all URLs processed and no valid PDF found → proceed to Image Phase

---

## Image Phase (LAST RESORT)

**⚠️ ONLY reach this phase after Step 2a (PDF Attachments) AND Step 2b (PDF URLs)
have BOTH been tried and BOTH failed. If you skipped Step 2b, GO BACK and do it.**

**IMPORTANT: Do NOT use Python PdfReader in this phase. Vision MCP ONLY.**

### Step 2c: Tagged Image URLs from PDF Phase

Process any image URLs that were tagged during PDF Phase (Step 2b).

For each tagged image URL:
1. The file may already be downloaded — check `path` from the tag record
2. Use vision MCP tool for data extraction (Step 3b below)
3. Validate 3 mandatory fields
4. If valid → **SUCCESS**, stop processing
5. If invalid → try next tagged URL

### Step 2d: Image Attachments

Check attachments directory for image files.

```bash
# List image attachments (>50KB to skip QR codes)
ls -la attachments/*.jpg attachments/*.jpeg attachments/*.png 2>/dev/null
```

**SKIP small images (< 50KB)** — these are QR codes, NOT invoices.

For each image attachment (>50KB):
1. Copy to monthly folder as temp file
2. Extract data using vision MCP tool (Step 3b below)
3. Validate 3 mandatory fields
4. If valid → **SUCCESS**, stop processing
5. If invalid → try next image attachment

### Step 2e: Image URLs from Email Body

Extract URLs from email body specifically looking for image downloads.

**Maximum 5 URLs per phase.** Skip URLs already processed in PDF Phase.

For each URL (up to 5):
1. Download and classify
2. Based on file type:

   **If Image:**
   - Extract with vision MCP tool (Step 3b)
   - Validate 3 mandatory fields
   - If valid → **SUCCESS**
   - If invalid → try next URL

   **If HTML:**
   - Use Two-Level HTML Extraction
   - Re-download and re-classify
   - If Image → extract with vision, validate
   - Otherwise → log as `download_failed`, try next URL

   **If PDF:**
   - Skip (already processed in PDF Phase)
   - Try next URL

   **If unknown/invalid:**
   - Log as `download_failed`, try next URL

3. If all URLs processed and no valid image found → **FINAL FAILURE**
   - Log to errors.jsonl with all attempted sources
   - Reply with error details (see VALIDATION.md)

---

## Step 3: Extract Invoice Data

### Step 3a: PDF Text Extraction (PDF Phase ONLY)

**Use Python PdfReader (pypdf) exclusively for PDF files.**

```bash
python3 << 'PYEOF'
from pypdf import PdfReader

reader = PdfReader('<file>')
text = ''
for page in reader.pages:
    page_text = page.extract_text()
    if page_text:
        text += page_text + '\n'

if text.strip():
    print(text[:5000])
else:
    print('EXTRACTION_FAILED: No text content')
PYEOF
```

If text extraction succeeds (non-empty output with recognizable invoice fields),
parse the extracted text to find:

**Mandatory fields (must find ALL 3):**
1. **销售方税号** (Seller Tax ID) — 纳税人识别号, 18 characters
2. **校验码** (Verification Code) — 20 numeric digits
3. **价税合计** (Total amount) — positive number

**Other fields to extract if available:**
- 发票号码 (Invoice number)
- 开票日期 (Invoice date)
- 发票类型 (Invoice type)
- 购买方名称 (Buyer name)
- 购买方税号 (Buyer tax ID)
- 销售方名称 (Seller name)
- 服务项目名称 (Service/item name)
- 税率 (Tax rate)
- 金额 (Amount excl. tax)
- 税额 (Tax amount)

**If text extraction fails or returns empty:**
- Mark this PDF source as failed
- Continue to next PDF source (do NOT fall back to vision)
- Do NOT use vision MCP on PDF files

### Step 3b: Vision MCP Extraction (Image Phase ONLY)

**Use vision MCP tool ONLY for image files (JPG/PNG). NEVER for PDFs.**

```
Prompt: "Extract the following information from this Chinese invoice image (发票):

以下3个字段为必填项，务必仔细识别：
1. 销售方税号 (Seller Tax ID / 纳税人识别号) — 18 characters [MANDATORY]
2. 校验码 (Verification Code) — 20 numeric digits [MANDATORY]
3. 价税合计 (Total amount, incl. tax) — positive number [MANDATORY]

其他字段尽量提取：
4. 发票号码 (Invoice number)
5. 开票日期 (Invoice date)
6. 发票类型 (Invoice type, e.g., 增值税专用发票/增值税普通发票/增值税电子普通发票)
7. 购买方名称 (Buyer name)
8. 购买方税号 (Buyer tax ID)
9. 销售方名称 (Seller name)
10. 服务项目名称 (Service/item name)
11. 税率 (Tax rate, e.g., 6%, 13%)
12. 金额 (Amount, excl. tax)
13. 税额 (Tax amount)

Return each value on a separate line in format: field_name: value
If a mandatory field cannot be found, return: field_name: NOT_FOUND"
```

**If vision extraction fails:**
- Mark this image source as failed
- Continue to next image source
- If ALL image sources fail → FINAL FAILURE

---

## Attachment Cleanup After Successful Processing

After an invoice is successfully validated and written to Excel (Step 5 completes),
clean up the attachment files from `attachments/` that belong to this email.

This prevents re-processing the same attachments on subsequent messages and keeps
the workspace tidy.

```bash
# Remove processed attachment files
# Only remove files that were part of this email's processing
rm -f "attachments/<processed_pdf_filename>"
rm -f "attachments/<processed_image_filename>"
```

**Rules:**
- Only clean up attachments AFTER successful Excel write (Step 5)
- Only remove files that belong to the current email being processed
- Do NOT remove files from other emails or unrelated files
- Do NOT clean up on failure — failed attachments stay for manual processing or retry
- The invoice file itself is already saved in `invoice_YYYY-MM/` folder, so the
  attachment copy is no longer needed
