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

### Step 2: Download the Invoice

- **From attachment**: The attachment is already saved in the `attachments/` directory.
  Copy it to the monthly folder with a temporary name first.
- **From URL in message**: Download using `bash`:
  ```bash
  curl -sL "<url>" -o "invoice_${MONTH}/temp_invoice.pdf"
  ```

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

Use Python with openpyxl to add a row to the monthly Excel file:

```bash
python3 << 'PYEOF'
from openpyxl import load_workbook

wb = load_workbook('invoice_YYYY-MM/invoices.xlsx')
ws = wb.active

# Find next empty row
next_row = ws.max_row + 1

# Template columns:
# A:序号 B:发票号码 C:开票日期 D:发票类型 E:购买方名称
# F:购买方税号 G:销售方名称 H:销售方税号 I:服务项目名称
# J:税率 K:金额 L:税额 M:价税合计 N:备注 O:文件名
ws.cell(row=next_row, column=1, value=next_row - 1)        # 序号
ws.cell(row=next_row, column=2, value='<发票号码>')         # 发票号码
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

wb.save('invoice_YYYY-MM/invoices.xlsx')
print('Row added successfully')
PYEOF
```

IMPORTANT: Before writing to Excel, read the template headers first to understand
the column layout. Adapt the column mapping to match the actual template.

### Step 5: Reply with Summary

Send a reply confirming:
- Invoice file saved as: `invoice_YYYY-MM/invoice_NNN.ext`
- Extracted values (formatted as a table)
- Row added to `invoice_YYYY-MM/invoices.xlsx`

Example reply:
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

### Step 6: Monthly Summary (when requested)

When the user asks to summarize a month's invoices:

1. Determine the target month (from user message or default to current month)
2. Verify the monthly folder and `invoices.xlsx` exist
3. Copy `summary.xlsx` template into the monthly folder (if not already there):
   ```bash
   MONTH="2026-04"
   if [ ! -f "invoice_${MONTH}/summary.xlsx" ]; then
     cp summary.xlsx "invoice_${MONTH}/summary.xlsx"
   fi
   ```
4. Read all data from `invoice_${MONTH}/invoices.xlsx`
5. Fill `invoice_${MONTH}/summary.xlsx` based on the invoice data:
   - Read the summary template headers first to understand the layout
   - Aggregate values as needed (totals, counts, by vendor, by tax rate, etc.)
   - Use Python openpyxl to read invoices and write summary

```bash
python3 << 'PYEOF'
from openpyxl import load_workbook

# Read invoice data
inv_wb = load_workbook('invoice_YYYY-MM/invoices.xlsx')
inv_ws = inv_wb.active

# Read summary template
sum_wb = load_workbook('invoice_YYYY-MM/summary.xlsx')
sum_ws = sum_wb.active

# Read summary headers to understand layout
headers = [sum_ws.cell(row=1, column=c).value for c in range(1, sum_ws.max_column + 1)]
print(f"Summary headers: {headers}")

# Aggregate data from invoices and fill summary
# (adapt based on actual summary template layout)

sum_wb.save('invoice_YYYY-MM/summary.xlsx')
print('Summary updated')
PYEOF
```

6. Reply with the summary results

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
- ALWAYS use sequential naming for invoice files (invoice_001, invoice_002, ...)
- ALWAYS read the Excel template headers before writing to understand column layout
- If vision tool fails on a PDF, try text extraction with pdftotext as fallback
- If extraction is uncertain about a value, mark it with "?" and ask the user to confirm
- Report any extraction errors clearly
- Do NOT overwrite existing invoice files
- Do NOT modify the template.xlsx — only modify the copy in the monthly folder
