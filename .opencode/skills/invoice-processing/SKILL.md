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

```
Thread directory structure:
<thread_dir>/
  template.xlsx           ← Empty Excel template (always present)
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
Prompt: "Extract the following information from this invoice:
1. Invoice number (Rechnungsnummer)
2. Invoice date (Rechnungsdatum)
3. Vendor/Supplier name (Lieferant)
4. Total amount with currency (Gesamtbetrag)
5. Tax amount if present (Steuerbetrag/MwSt)
6. Payment due date if present (Fälligkeitsdatum)
7. Description/items summary (Beschreibung)

Return the values in a structured format, one per line."
```

For PDFs that the vision tool cannot process, try text extraction as fallback:
```bash
python3 -c "
import subprocess
result = subprocess.run(['pdftotext', '<file>', '-'], capture_output=True, text=True)
print(result.stdout[:3000])
"
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

# Write extracted values (adjust columns to match template)
ws.cell(row=next_row, column=1, value='<invoice_number>')
ws.cell(row=next_row, column=2, value='<invoice_date>')
ws.cell(row=next_row, column=3, value='<vendor_name>')
ws.cell(row=next_row, column=4, value=<total_amount>)
ws.cell(row=next_row, column=5, value=<tax_amount>)
ws.cell(row=next_row, column=6, value='<due_date>')
ws.cell(row=next_row, column=7, value='<description>')
ws.cell(row=next_row, column=8, value='<filename>')

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
✅ Invoice processed

| Field | Value |
|-------|-------|
| Invoice # | INV-2026-0042 |
| Date | 2026-04-10 |
| Vendor | ACME Corp |
| Total | €1,234.56 |
| Tax | €234.56 |
| Due | 2026-05-10 |

File: invoice_2026-04/invoice_003.pdf
Excel: invoice_2026-04/invoices.xlsx (row 4)
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
