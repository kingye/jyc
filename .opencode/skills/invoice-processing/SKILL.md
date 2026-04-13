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

The template Excel file is bundled with this skill at:
`.opencode/skills/invoice-processing/template.xlsx`

If the thread doesn't have `template.xlsx` yet, copy it from the skill:
```bash
if [ ! -f template.xlsx ]; then
  cp .opencode/skills/invoice-processing/template.xlsx template.xlsx
fi
```

```
Thread directory structure:
<thread_dir>/
  template.xlsx           ← Excel template (copied from skill on first use)
  invoice_YYYY-MM/        ← Monthly folder (e.g., invoice_2026-04)
    invoices.xlsx          ← Excel with extracted data for this month
    invoice_001.pdf        ← Downloaded invoices
    invoice_002.jpg
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
  Copy it to the monthly folder with a sequential name.
- **From URL in message**: Download using `bash`:
  ```bash
  curl -sL "<url>" -o "invoice_${MONTH}/invoice_NNN.pdf"
  ```

Naming convention: `invoice_001.pdf`, `invoice_002.jpg`, etc.
Check existing files to determine the next sequence number:
```bash
ls invoice_${MONTH}/invoice_* 2>/dev/null | wc -l
```

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

### Step 6: Export Monthly Invoices (when requested)

When the user asks to download or export all invoices for a month:

1. Determine the target month (from user message or default to current month)
2. Verify the folder exists
3. Zip the entire monthly folder:
   ```bash
   MONTH="2026-04"
   cd <thread_dir>
   zip -r "invoice_${MONTH}.zip" "invoice_${MONTH}/"
   ```
4. Send the zip file as an attachment in the reply

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
