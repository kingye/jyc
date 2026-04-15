# Step 2–3: Download and Extract Invoice Data

This file covers the two-phase processing flow for downloading and extracting
invoice data from emails.

**Key Principle:** PDF sources are processed first. Only if ALL PDF sources fail
does the system proceed to the Image Phase.

**Key Restriction:** PDF files use Python PdfReader (pypdf) ONLY for extraction.
Vision MCP is ONLY for image files (JPG/PNG). Do NOT use vision on PDFs.

---

## Shared Logic

These utilities are used by both PDF Phase and Image Phase.

### URL Extraction from Email Body

Search the email body for invoice download URLs. This function is called once
and the results are used by both phases.

```bash
# Look for URLs in the email body:
# - URLs ending in .pdf, .PDF, .jpg, .png
# - URLs containing keywords: download, invoice, fapiao, 发票
# - URLs from known platforms: fapiao.com, einvoice, piaozone
```

### Download and Classify

Download a URL and determine the file type:

```bash
curl -sL -o "invoice_${MONTH}/temp_download" "<url>"

file_type=$(file --brief --mime-type "invoice_${MONTH}/temp_download")
case "$file_type" in
    application/pdf)
        echo "pdf"
        ;;
    image/jpeg|image/png)
        echo "image"
        ;;
    text/html|application/xhtml+xml)
        echo "html"
        ;;
    *)
        echo "unknown"
        ;;
esac
```

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

If no PDF attachments found or all fail validation → proceed to Step 2b.

### Step 2b: PDF URLs from Email Body

Extract URLs from the email body and attempt to download PDFs.

**Maximum 5 URLs per phase.**

For each URL (up to 5):
1. Download and classify the file type
2. Based on file type:

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
   - Use Two-Level HTML Extraction (see Shared Logic above)
   - After extraction, re-download the real URL
   - Re-classify the downloaded file:
     - If PDF → extract with PdfReader, validate
     - If Image → tag for Image Phase
     - If still HTML or unknown → log as `download_failed`, try next URL

   **If unknown/invalid:**
   - Log as `download_failed`
   - Remove temp file
   - Try next URL

3. If all URLs processed and no valid PDF found → proceed to Image Phase

---

## Image Phase (LAST RESORT)

Only reached when ALL PDF sources have failed. Uses vision MCP tool only.

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
