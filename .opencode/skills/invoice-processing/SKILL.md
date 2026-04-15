---
name: invoice-processing
description: |
  Process invoices from messages (PDF/image attachments or URLs).
  Extract key values using vision tool, organize by month, update Excel.
  Use when: receiving invoices, processing receipts, bookkeeping tasks.
---

## Invoice Processing Workflow

When you receive a message containing an invoice (PDF, image attachment, or URL):

### Step 1: Determine Current Month Folder

**IMPORTANT: The month folder is based on when the invoice is RECEIVED/processed, NOT the invoice date (开票日期).**

For example:
- Invoice dated 2026-03 (March) but received in 2026-04 (April) → file into `invoice_2026-04/`
- The 开票日期 is still recorded in the Excel, but the file goes to the current month

The template Excel files are bundled with this skill at:
- `.opencode/skills/invoice-processing/template.xlsx` — invoice record template
- `summary.xlsx` — summary template (placed in thread directory by user)

If the thread doesn't have `template.xlsx` yet, copy it from the skill:
```bash
if [ ! -f template.xlsx ]; then
  cp .opencode/skills/invoice-processing/template.xlsx template.xlsx
fi
```

```
Thread directory structure:
<thread_dir>/
  template.xlsx           ← Invoice record template (copied from skill)
  summary.xlsx            ← Summary template (placed by user)
  invoice_YYYY-MM/        ← Monthly folder (e.g., invoice_2026-04)
    invoices.xlsx          ← Invoice records for this month
    errors.jsonl           ← Failed invoice log (append-only, one JSON per line)
    summary.xlsx           ← Summary for this month (copied + filled when requested)
    INV-2026-0042.pdf      ← Downloaded invoices (named by invoice number)
    INV-2026-0043.jpg
    ...
```

Check if the current month's folder exists:
```bash
MONTH=$(date +%Y-%m)
FOLDER="invoice_${MONTH}"
if [ ! -d "$FOLDER" ]; then
  mkdir -p "$FOLDER"
  cp template.xlsx "$FOLDER/invoices.xlsx"
fi
```

### Step 2: Identify and Download the Invoice

**IMPORTANT: ALWAYS check the attachments directory first.**

Attachments from the email are automatically saved by the system to the
`attachments/` subdirectory in the thread workspace. Check there before
looking at the email body for download URLs.

**Priority order:**

1. **Check saved attachments first**:
   ```bash
   ls -la attachments/
   ```
   - Look for PDF files (`.pdf`) or large image files (`.jpg`, `.png`)
   - SKIP small images (< 50KB) — these are QR codes, NOT invoices
   - If a valid invoice file is found, copy it to the monthly folder and
     proceed to Step 3

2. **If no usable attachment found, search the email body for download URLs**:
   - Look for URLs ending in `.pdf`, `.PDF`
   - Look for URLs containing keywords: `download`, `invoice`, `fapiao`, `发票`
   - Look for URLs from known invoice platforms (e.g., `fapiao.com`, `einvoice`, `piaozone`)
   - These URLs are the actual invoice files

2. **Download from URL**:
   ```bash
   curl -sL -o "invoice_${MONTH}/temp_invoice" "<download_url>"
   ```
   Verify the downloaded file type:
   ```bash
   file "invoice_${MONTH}/temp_invoice"
   ```

   **Two-level download handling:**
   - If the downloaded file is HTML (not PDF/image), it is an intermediate page
   - Use the bundled scripts to extract the real PDF URL automatically:
     ```bash
     # 1. Try lightweight parsing first
     result=$(python3 .opencode/skills/invoice-processing/scripts/html_parser.py \
         "invoice_${MONTH}/temp_invoice" "<original_url>")

     # 2. If lightweight parsing fails, use Playwright (fallback)
     if echo "$result" | python3 -c "import sys,json; r=json.load(sys.stdin); exit(0 if r.get('success') else 1)" 2>/dev/null; then
         pdf_url=$(echo "$result" | python3 -c "import sys,json; print(json.load(sys.stdin)['pdf_url'])")
     else
         result=$(python3 .opencode/skills/invoice-processing/scripts/playwright_extractor.py "<original_url>")
         if echo "$result" | python3 -c "import sys,json; r=json.load(sys.stdin); exit(0 if r.get('success') else 1)" 2>/dev/null; then
             pdf_url=$(echo "$result" | python3 -c "import sys,json; print(json.load(sys.stdin)['pdf_url'])")
         fi
     fi

     # 3. Download the real PDF (if URL was extracted)
     if [ -n "$pdf_url" ]; then
         curl -sL -o "invoice_${MONTH}/temp_invoice" "$pdf_url"
     else
         # Cannot extract real PDF URL — this is a download failure.
         # Log to errors.jsonl and do NOT save the HTML file as an invoice.
         rm -f "invoice_${MONTH}/temp_invoice"
     fi
     ```
   - The switch between parsers is automatic — no user interaction
   - After download, **verify the file is PDF or image** (see validation below)

**File format validation (MANDATORY):**

Only PDF (`.pdf`) and image (`.jpg`, `.jpeg`, `.png`) files are valid invoice
vouchers. After downloading or locating an attachment, verify the file type:
```bash
file_type=$(file --brief --mime-type "invoice_${MONTH}/temp_invoice")
case "$file_type" in
    application/pdf|image/jpeg|image/png)
        echo "Valid invoice file: $file_type"
        ;;
    *)
        echo "INVALID: $file_type — not a certified voucher format"
        # Log to errors.jsonl and remove the invalid file
        rm -f "invoice_${MONTH}/temp_invoice"
        ;;
esac
```

If the file is not PDF or image:
- Log to `errors.jsonl` with `error_type: "download_failed"` and detail explaining the actual file type
- Do NOT save it to the monthly folder
- Do NOT proceed to extraction
- Reply with an error message to the user

**IMPORTANT:**
- Only PDF and image files (JPG/PNG) are valid certified vouchers (合规凭证)
- HTML, XML, text, or any other format is NOT a valid invoice — treat as download failure
- Do NOT use the QR code image as the invoice
- Do NOT follow or scan the QR code URL
- The QR code is for the recipient to verify/download manually, not for AI processing

After extraction (Step 3), rename the file using the invoice number:
```bash
# Example: rename temp file to invoice number
mv "invoice_${MONTH}/temp_invoice.pdf" "invoice_${MONTH}/INV-2026-0042.pdf"
```

Naming rules:
- Use the extracted 发票号码 as the filename (e.g., `INV-2026-0042.pdf`)
- Keep the original file extension
- If 发票号码 cannot be extracted, fall back to sequential naming (`invoice_001.pdf`)
- If a file with the same name exists, append a suffix (`INV-2026-0042_2.pdf`)

### Step 3: Extract Invoice Data

**For PDF files**, first try text extraction (fast, no vision API cost).
If text extraction fails or returns incomplete data, fall back to vision MCP.

**Step 3a: Try PDF text extraction first**

**Option A: pypdf (preferred — pure Python, no system dependencies)**
```bash
python3 << 'PYEOF'
from pypdf import PdfReader
reader = PdfReader('<file>')
for page in reader.pages:
    text = page.extract_text()
    if text:
        print(text[:3000])
PYEOF
```

**Option B: pdftotext (requires poppler-utils)**
```bash
pdftotext '<file>' - | head -100
```

If text extraction succeeds (non-empty output with recognizable invoice fields),
parse the extracted text to get the invoice values. Then proceed to Step 4 (validation).

**Step 3b: Fall back to vision MCP (for scanned PDFs or images)**

If text extraction fails, returns empty text, or the extracted text does not
contain recognizable invoice fields, use the `vision_analyze_image` tool:

```
Prompt: "Extract the following information from this Chinese invoice (发票):
1. 发票号码 (Invoice number)
2. 开票日期 (Invoice date)
3. 发票类型 (Invoice type, e.g., 增值税专用发票/增值税普通发票/增值税电子普通发票)
4. 购买方名称 (Buyer name)
5. 购买方税号 (Buyer tax ID)
6. 销售方名称 (Seller name)
7. 销售方税号 (Seller tax ID)
8. 服务项目名称 (Service/item name)
9. 税率 (Tax rate, e.g., 6%, 13%)
10. 金额 (Amount, excl. tax)
11. 税额 (Tax amount)
12. 价税合计 (Total, incl. tax)

Return each value on a separate line in format: field_name: value"
```

**For image files** (JPG/PNG), use the vision tool directly (skip text extraction).

### Step 4: Validate Extraction Before Writing to Excel

**CRITICAL: Do NOT write to invoices.xlsx if the invoice is invalid, extraction failed, or data is incomplete.**

Before proceeding to update Excel, validate that:
1. The invoice file is a valid format (PDF or image — JPG/PNG only)
2. The extraction was successful
3. All required fields are present
The following fields are **required** — if any are missing, log to `errors.jsonl`
instead of writing to `invoices.xlsx`:

**Required fields:**
- `发票号码` (Invoice number) — must be non-empty
- `价税合计` (Total amount) — must be a valid number

**Validation logic:**
- If the invoice file is not PDF or image (JPG/PNG) → log error with `download_failed`, skip
- If the invoice file could not be downloaded → log error with `download_failed`, skip
- If text extraction AND vision extraction both failed → log error with `extraction_failed`, skip
- If required fields are missing or clearly invalid → log error with `incomplete_data`, skip

**Error log format: `invoice_YYYY-MM/errors.jsonl`**

One JSON object per line (JSON Lines format). Append-only — never overwrite.

```bash
python3 << 'PYEOF'
import json, datetime

error_entry = {
    "timestamp": datetime.datetime.now().isoformat(),
    "error_type": "<download_failed|extraction_failed|incomplete_data>",
    "source": "<attachment filename or download URL>",
    "sender": "<sender email address>",
    "subject": "<email subject>",
    "file_saved_as": "<path to file if saved, or null>",
    "fields_extracted": {
        # Include whatever was successfully extracted (partial data)
        # e.g., "发票号码": "INV-2026-0042", "开票日期": "2026-04-10"
    },
    "fields_missing": ["<list of required fields that are missing>"],
    "error_detail": "<specific error message explaining what went wrong>"
}

MONTH = "2026-04"
with open(f'invoice_{MONTH}/errors.jsonl', 'a') as f:
    f.write(json.dumps(error_entry, ensure_ascii=False) + '\n')

print(f'Error logged to invoice_{MONTH}/errors.jsonl')
PYEOF
```

**Error type values:**
| error_type | When to use |
|------------|-------------|
| `download_failed` | URL returned error, file is empty, file is not PDF/image (e.g., HTML, XML, text), Playwright extraction failed |
| `extraction_failed` | Both text extraction and vision failed, or returned empty output |
| `incomplete_data` | Extraction returned some data but required fields are missing |

**Example error entries:**
```json
{"timestamp":"2026-04-15T14:30:00","error_type":"download_failed","source":"https://fapiao.com/download/abc123","sender":"vendor@example.com","subject":"发票-2026-04","file_saved_as":null,"fields_extracted":{},"fields_missing":["发票号码","价税合计"],"error_detail":"Download returned HTML page, Playwright extraction also failed"}
{"timestamp":"2026-04-15T15:00:00","error_type":"incomplete_data","source":"attachments/invoice.pdf","sender":"finance@corp.com","subject":"报销发票","file_saved_as":"invoice_2026-04/unknown_001.pdf","fields_extracted":{"开票日期":"2026-03-20","销售方名称":"XX公司"},"fields_missing":["发票号码","价税合计"],"error_detail":"Vision API returned partial data, invoice number and total amount not recognized"}
```

**IMPORTANT:** If extraction fails but the file IS a valid format (PDF or image),
still save it to the monthly folder with a fallback name like `unknown_001.pdf`.
This allows the user to manually process it later. Record the saved path in
`file_saved_as` so the user can find it.

If the file is NOT a valid format (HTML, XML, text, etc.), do NOT save it —
set `file_saved_as` to `null` in the error log.

### Step 5: Update Excel

**Only reached if Step 4 validation passes.**

**IMPORTANT: Check for duplicate invoice numbers before adding.**

Use Python with openpyxl to check and add a row:

```bash
python3 << 'PYEOF'
from openpyxl import load_workbook

INVOICE_NO = '<发票号码>'
MONTH = '2026-04'

wb = load_workbook(f'invoice_{MONTH}/invoices.xlsx')
ws = wb.active

# Check for duplicate invoice number (column B = 发票号码)
existing_rows = []
for row in range(2, ws.max_row + 1):
    existing_no = ws.cell(row=row, column=2).value
    if existing_no and str(existing_no).strip() == INVOICE_NO.strip():
        existing_rows.append(row)

if existing_rows:
    print(f'DUPLICATE: Invoice {INVOICE_NO} already exists at row(s): {existing_rows}')
    print('Skipping - do not add duplicate')
    exit(0)

# Find next empty row
next_row = ws.max_row + 1

# Template columns:
# A:序号 B:发票号码 C:开票日期 D:发票类型 E:购买方名称
# F:购买方税号 G:销售方名称 H:销售方税号 I:服务项目名称
# J:税率 K:金额 L:税额 M:价税合计 N:备注 O:文件名
ws.cell(row=next_row, column=1, value=next_row - 1)        # 序号
ws.cell(row=next_row, column=2, value=INVOICE_NO)          # 发票号码
ws.cell(row=next_row, column=3, value='<开票日期>')         # 开票日期
ws.cell(row=next_row, column=4, value='<发票类型>')         # 发票类型
ws.cell(row=next_row, column=5, value='<购买方名称>')       # 购买方名称
ws.cell(row=next_row, column=6, value='<购买方税号>')       # 购买方税号
ws.cell(row=next_row, column=7, value='<销售方名称>')       # 销售方名称
ws.cell(row=next_row, column=8, value='<销售方税号>')       # 销售方税号
ws.cell(row=next_row, column=9, value='<服务项目名称>')     # 服务项目名称
ws.cell(row=next_row, column=10, value='<税率>')            # 税率
ws.cell(row=next_row, column=11, value=<金额>)              # 金额
ws.cell(row=next_row, column=12, value=<税额>)              # 税额
ws.cell(row=next_row, column=13, value=<价税合计>)          # 价税合计
ws.cell(row=next_row, column=14, value='')                  # 备注
ws.cell(row=next_row, column=15, value='<filename>')        # 文件名

wb.save(f'invoice_{MONTH}/invoices.xlsx')
print('Row added successfully')
PYEOF
```

IMPORTANT: Before writing to Excel, read the template headers first to understand
the column layout. Adapt the column mapping to match the actual template.

### Step 6: Reply with Summary

Send a reply confirming:
- If new: Invoice file saved as: `invoice_YYYY-MM/invoice_NNN.ext`
- Extracted values (formatted as a table)
- Row added to `invoice_YYYY-MM/invoices.xlsx`
- If duplicate: Skip with message
- If failed: Error logged with details

Example reply (new invoice):
```
✅ 发票已处理

| 字段 | 值 |
|------|-----|
| 发票号码 | INV-2026-0042 |
| 开票日期 | 2026-04-10 |
| 发票类型 | 增值税普通发票 |
| 购买方 | XX有限公司 |
| 销售方 | YY有限公司 |
| 服务项目 | 信息技术服务 |
| 税率 | 6% |
| 金额 | ¥1,000.00 |
| 税额 | ¥60.00 |
| 价税合计 | ¥1,060.00 |

文件: invoice_2026-04/invoice_003.pdf
Excel: invoice_2026-04/invoices.xlsx (第4行)
```

Example reply (duplicate):
```
⚠️ 发票已忽略

发票号码 INV-2026-0042 已存在于 invoice_2026-04/invoices.xlsx (第3行)
跳过重复记录
```

Example reply (failed — download error):
```
❌ 发票处理失败

来源: https://fapiao.com/download/abc123
发件人: vendor@example.com
错误: 下载失败，返回HTML页面而非PDF文件

已记录到 invoice_2026-04/errors.jsonl，请手动处理
```

Example reply (failed — incomplete extraction):
```
❌ 发票处理失败

来源: attachments/invoice.pdf
发件人: finance@corp.com
文件已保存: invoice_2026-04/unknown_001.pdf
已提取部分信息: 开票日期=2026-03-20, 销售方=XX公司
缺失必填字段: 发票号码, 价税合计

已记录到 invoice_2026-04/errors.jsonl，请手动处理
```

### Step 7: Monthly Summary (when requested)

When the user asks to summarize a month's invoices:

The summary template (`summary.xlsx`) is an IIT deduction claim form with predefined categories.

**Template structure (Sheet1):**
- Row 1: Title "Bills for Expat IIT deduction"
- Row 4: "Handed in by:" + name
- Row 6: "Month:" + month (e.g., "April of 2026")
- Row 9: Headers (DATE, AMOUNT)
- Rows 10-37: Categories with DATE and AMOUNT columns:
  - Row 10: Laundry
  - Row 12: Food & Meals
  - Row 14: Rental fee
  - Row 14: Rental fee (**FIXED VALUE — do NOT modify**)
  - Row 16: Airticket
  - (other rows available for additional categories)
- Row 38: Total (SUM formula, auto-calculates)
- Row 43: Date & Signature

**IMPORTANT:** Row 14 (Rental fee) contains a fixed value set by the user.
Do NOT overwrite or modify this row during summary generation.

**Process:**

1. Determine the target month (from user message or default to current month)
2. Verify the monthly folder and `invoices.xlsx` exist
3. Copy `summary.xlsx` template into the monthly folder (if not already there):
   ```bash
   MONTH="2026-04"
   if [ ! -f "invoice_${MONTH}/summary.xlsx" ]; then
     cp summary.xlsx "invoice_${MONTH}/summary.xlsx"
   fi
   ```
4. Read all invoices from `invoices.xlsx`
5. Categorize each invoice by its 服务项目名称 (service/item name):
   - 餐饮/餐费/食品/外卖 → Food & Meals (row 12)
   - 洗衣/干洗 → Laundry (row 10)
   - 房租/租金/租赁 → Rental fee (row 14)
   - 机票/航空 → Airticket (row 16)
   - Other categories → use available rows (17-37)
6. Fill the summary:

```bash
python3 << 'PYEOF'
from openpyxl import load_workbook

MONTH = "2026-04"

# Read invoice data
inv_wb = load_workbook(f'invoice_{MONTH}/invoices.xlsx')
inv_ws = inv_wb.active

# Read summary template
sum_wb = load_workbook(f'invoice_{MONTH}/summary.xlsx')
sum_ws = sum_wb.active

# Update month
sum_ws['B6'] = 'April of 2026'  # Adjust month name

# Category mapping: row number → category keywords
# NOTE: Row 14 (Rental fee) is a FIXED value — do NOT include it here
categories = {
    10: ['洗衣', '干洗', 'laundry'],
    12: ['餐饮', '餐费', '食品', '外卖', 'food', 'meal'],
    16: ['机票', '航空', 'airticket', 'flight'],
}

# Read invoices (skip header row)
for row in range(2, inv_ws.max_row + 1):
    service = str(inv_ws.cell(row=row, column=9).value or '').lower()  # 服务项目名称
    date = inv_ws.cell(row=row, column=3).value  # 开票日期
    amount = inv_ws.cell(row=row, column=13).value  # 价税合计

    if not amount:
        continue

    # Find matching category
    target_row = None
    for cat_row, keywords in categories.items():
        if any(kw in service for kw in keywords):
            target_row = cat_row
            break

    if target_row:
        # Append amount to existing value
        existing = sum_ws.cell(row=target_row, column=3).value or 0
        if isinstance(existing, str):
            existing = 0
        sum_ws.cell(row=target_row, column=3, value=existing + float(amount))
        # Set date if empty
        if not sum_ws.cell(row=target_row, column=2).value:
            sum_ws.cell(row=target_row, column=2, value=date)

sum_wb.save(f'invoice_{MONTH}/summary.xlsx')
print('Summary updated')
PYEOF
```

7. Reply with a summary of categorized amounts

IMPORTANT: Read the actual summary.xlsx headers and structure before writing.
The template may have been customized — adapt the category mapping accordingly.

### Step 8: Export Monthly Invoices (when requested)

When the user asks to download or export all invoices for a month:

1. Determine the target month (from user message or default to current month)
2. Verify the folder exists
3. If summary has not been generated yet, run Step 7 first to create it
4. Zip the entire monthly folder (includes invoices.xlsx, summary.xlsx, and all invoice files):
   ```bash
   MONTH="2026-04"
   cd <thread_dir>
   zip -r "invoice_${MONTH}.zip" "invoice_${MONTH}/"
   ```
5. Send the zip file as an attachment in the reply

If the user asks for a specific month that doesn't exist, reply with available months:
```bash
ls -d invoice_*/
```

### Step 9: List Errors (when requested)

When the user asks to see failed invoices, errors, or problems (e.g., "show errors",
"哪些发票失败了", "list failed invoices"), read and format the error log.

**Trigger phrases:** errors, 错误, 失败, failed, problems, 问题

**Process:**

1. Determine the target month (from user message or default to current month)
2. Check if `errors.jsonl` exists:
   ```bash
   MONTH="2026-04"
   if [ -f "invoice_${MONTH}/errors.jsonl" ]; then
       cat "invoice_${MONTH}/errors.jsonl"
   else
       echo "No errors recorded for ${MONTH}"
   fi
   ```
3. Parse each JSON line and format as a readable table

**Reply format:**

If errors exist:
```
📋 invoice_2026-04 处理失败记录 (共 2 条)

1. ❌ 下载失败
   来源: https://fapiao.com/download/abc123
   发件人: vendor@example.com
   邮件主题: 发票-2026-04
   错误: 下载返回HTML页面，非PDF/图片格式
   文件: 未保存

2. ❌ 数据不完整
   来源: attachments/invoice.pdf
   发件人: finance@corp.com
   邮件主题: 报销发票
   错误: 缺失必填字段 — 发票号码, 价税合计
   已提取: 开票日期=2026-03-20, 销售方=XX公司
   文件: invoice_2026-04/unknown_001.pdf
```

If no errors:
```
✅ invoice_2026-04 无失败记录
```

If multiple months have errors and user doesn't specify a month, list all:
```bash
ls invoice_*/errors.jsonl 2>/dev/null
```

### Browser Automation for Complex HTML Pages

Some invoice platforms serve JavaScript-heavy pages (e.g. React apps) instead
of direct PDF links. The system handles this automatically:

1. **Lightweight parser** (`scripts/html_parser.py`) — tries regex-based
   extraction first (fast, no dependencies beyond Python stdlib)
2. **Playwright fallback** (`scripts/playwright_extractor.py`) — launches
   a headless Chromium browser to render the page and extract the PDF URL

The switch is fully automatic. The user only sees the final result.

**One-time Playwright setup** (Debian headless server):
```bash
bash .opencode/skills/invoice-processing/bin/install-playwright.sh
```

### Rules

- ALWAYS check/create the monthly folder before processing
- ALWAYS copy template.xlsx to the new monthly folder as invoices.xlsx
- ALWAYS read the Excel template headers before writing to understand column layout
- ALWAYS validate file format — only PDF and image (JPG/PNG) are valid certified vouchers
- ALWAYS validate required fields (发票号码, 价税合计) before writing to invoices.xlsx
- NEVER write incomplete or failed invoices to invoices.xlsx — log to errors.jsonl instead
- NEVER save non-PDF/non-image files to the monthly folder — they are not valid vouchers
- If vision tool fails on a PDF, try text extraction with pdftotext as fallback
- If both extraction methods fail, log to errors.jsonl with error_type "extraction_failed"
- If extraction returns partial data missing required fields, log with "incomplete_data"
- If download fails or file is wrong format, log with "download_failed"
- If file is valid PDF/image but extraction fails, save it for manual processing
- Report any extraction errors clearly in the reply
- Do NOT overwrite existing invoice files
- Do NOT modify the template.xlsx — only modify the copy in the monthly folder
- Do NOT process QR code images — invoice emails often contain small QR code images
  (payment links, verification codes). These are NOT invoices. Ignore small images
  (typically < 50KB) that are likely QR codes. Only process PDF files and large images
  that are actual invoice scans.
- Do NOT follow URLs embedded in QR codes
