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

Invoice emails typically contain:
- A **download URL** in the email body → this is the actual invoice (PDF)
- A small **QR code image** as attachment → this is NOT the invoice, IGNORE it

**Priority: always prefer the download URL over attachments.**

1. **Search the email body for download URLs** first:
   - Look for URLs ending in `.pdf`, `.PDF`
   - Look for URLs containing keywords: `download`, `invoice`, `fapiao`, `发票`
   - Look for URLs from known invoice platforms (e.g., `fapiao.com`, `einvoice`, `piaozone`)
   - These URLs are the actual invoice files

2. **Download from URL**:
   ```bash
   curl -sL -o "invoice_${MONTH}/temp_invoice.pdf" "<download_url>"
   ```
   Verify the downloaded file is actually a PDF (not HTML error page):
   ```bash
   file "invoice_${MONTH}/temp_invoice.pdf"
   ```

3. **From attachment (only if no download URL found)**:
   - SKIP small images (< 50KB) — these are QR codes, NOT invoices
   - Only use PDF attachments or large image attachments (> 100KB)
   - Copy to monthly folder with a temporary name

**IMPORTANT:**
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

Use the `vision_analyze_image` tool to extract key values from the invoice:

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

For PDFs that the vision tool cannot process, try text extraction as fallback:

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

Then extract values from the text output.

### Step 4: Update Excel

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

### Step 5: Reply with Summary

Send a reply confirming:
- If new: Invoice file saved as: `invoice_YYYY-MM/invoice_NNN.ext`
- Extracted values (formatted as a table)
- Row added to `invoice_YYYY-MM/invoices.xlsx`
- If duplicate: Skip with message

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

### Step 6: Monthly Summary (when requested)

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

### Step 7: Export Monthly Invoices (when requested)

When the user asks to download or export all invoices for a month:

1. Determine the target month (from user message or default to current month)
2. Verify the folder exists
3. If summary has not been generated yet, run Step 6 first to create it
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

### Rules

- ALWAYS check/create the monthly folder before processing
- ALWAYS copy template.xlsx to the new monthly folder as invoices.xlsx
- ALWAYS read the Excel template headers before writing to understand column layout
- If vision tool fails on a PDF, try text extraction with pdftotext as fallback
- If extraction is uncertain about a value, mark it with "?" and ask the user to confirm
- Report any extraction errors clearly
- Do NOT overwrite existing invoice files
- Do NOT modify the template.xlsx — only modify the copy in the monthly folder
- Do NOT process QR code images — invoice emails often contain small QR code images
  (payment links, verification codes). These are NOT invoices. Ignore small images
  (typically < 50KB) that are likely QR codes. Only process PDF files and large images
  that are actual invoice scans.
- Do NOT follow URLs embedded in QR codes
