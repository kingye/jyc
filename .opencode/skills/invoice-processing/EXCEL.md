# Step 5–8: SQLite/Excel, Reply, Summary & Export

This file covers writing validated invoice data to SQLite (Step 5), replying to
the user (Step 6), monthly summary generation (Step 7), and export (Step 8).

---

## Step 5: Insert to SQLite

**Only reached if Step 4 validation passes (all 2 mandatory fields present).**

**IMPORTANT: Check for duplicates before inserting.**

### Database Operations

Use Python with the db.py module to check and insert:

```bash
python3 << 'PYEOF'
import sys
import os

sys.path.insert(0, '.opencode/skills/invoice-processing/scripts')
from db import init_db, check_duplicate, insert_invoice

# Ensure database exists
init_db()

# Invoice data (extracted from PDF/image)
INVOICE_NO = '<发票号码>'       # May be empty if not extracted
SELLER_TAX_ID = '<销售方税号>'  # MANDATORY — 18 chars
VERIFY_CODE = '<校验码>'        # Optional — 20 digits
TOTAL = <价税合计>              # MANDATORY — positive number
MONTH = '2026-04'

# Build data dict
data = {
    'seq': None,  # Will be auto-assigned by database
    'invoice_no': INVOICE_NO if INVOICE_NO else None,
    'issue_date': '<开票日期>',      # e.g., "2026-04-10"
    'invoice_type': '<发票类型>',    # e.g., "增值税普通发票"
    'buyer_name': '<购买方名称>',
    'buyer_tax_id': '<购买方税号>',
    'seller_name': '<销售方名称>',
    'seller_tax_id': SELLER_TAX_ID,
    'service_name': '<服务项目名称>',
    'tax_rate': '<税率>',            # e.g., "6%"
    'amount': <金额>,                # Amount without tax
    'tax': <税额>,                   # Tax amount
    'total_amount': TOTAL,
    'remarks': '',                   # Additional remarks
    'filename': '<filename>',        # e.g., "INV-2026-0042.pdf"
    'verify_code': VERIFY_CODE if VERIFY_CODE else None,
    'source': 'pdf_attachment',      # or 'pdf_url', 'image_attachment', etc.
    'month': MONTH,
}

# Check for duplicate
existing_id = check_duplicate(
    data['invoice_no'] or '',
    data['seller_tax_id'],
    data['total_amount']
)

if existing_id:
    print(f'DUPLICATE: Invoice already exists with id={existing_id}')
    print('Skipping - do not add duplicate')
    sys.exit(0)

# Insert to SQLite
row_id = insert_invoice(data)
print(f'Inserted successfully, id={row_id}')
PYEOF
```

**The database schema maps directly to the Excel columns (A-O).**
See `scripts/db.py` for the full schema and field mappings.

---

## Step 6: Reply with Summary

Send a reply confirming the processing result.

### Success (new invoice)

```
✅ 发票已验证并保存

必填字段:
• 销售方税号: 91110108MA01XXXXXX ✓
• 校验码: 12345678901234567890 ✓
• 价税合计: ¥1,060.00 ✓

| 字段 | 值 |
|------|-----|
| 发票号码 | INV-2026-0042 |
| 开票日期 | 2026-04-10 |
| 发票类型 | 增值税普通发票 |
| 购买方 | XX有限公司 |
| 销售方 | YY有限公司 |
| 销售方税号 | 91110108MA01XXXXXX |
| 服务项目 | 信息技术服务 |
| 税率 | 6% |
| 金额 | ¥1,000.00 |
| 税额 | ¥60.00 |
| 价税合计 | ¥1,060.00 |

来源: PDF附件 (PdfReader文本提取)
文件: invoice_2026-04/INV-2026-0042.pdf
数据库: invoices.db (id=4)
```

### Success (from Image Phase)

```
✅ 发票已验证并保存

必填字段:
• 销售方税号: 91110108MA01XXXXXX ✓
• 校验码: 12345678901234567890 ✓
• 价税合计: ¥1,060.00 ✓

(same table as above)

来源: 图片附件 (Vision MCP识别)
文件: invoice_2026-04/INV-2026-0042.jpg
数据库: invoices.db (id=4)

⚠️ 注意: PDF阶段未找到有效发票，使用图片阶段处理
```

### Success (missing 发票号码 — warning only)

```
✅ 发票已验证并保存

必填字段:
• 销售方税号: 91110108MA01XXXXXX ✓
• 校验码: 12345678901234567890 ✓
• 价税合计: ¥1,060.00 ✓

⚠️ 发票号码未提取到，使用顺序编号: invoice_001.pdf

(table without 发票号码 row)

文件: invoice_2026-04/invoice_001.pdf
数据库: invoices.db (id=4)
```

### Duplicate

```
⚠️ 发票已忽略

发票号码 INV-2026-0042 已存在于 invoices.db (id=3)
跳过重复记录
```

### Failure (download error)

```
❌ 发票处理失败

来源: https://fapiao.com/download/abc123
发件人: vendor@example.com
错误: 下载失败，返回HTML页面而非PDF文件

已尝试:
  1. PDF URL → HTML页面，html_parser和playwright均未提取到PDF链接

已记录到 invoice_2026-04/errors.jsonl，请手动处理
```

### Failure (incomplete data — missing mandatory fields)

```
❌ 发票处理失败

来源: attachments/invoice.pdf
发件人: finance@corp.com
文件已保存: invoice_2026-04/unknown_001.pdf
已提取部分信息: 开票日期=2026-03-20, 销售方=XX公司, 价税合计=¥1,060.00
缺失必填字段: 销售方税号, 校验码

已尝试:
  1. PDF附件 → 缺少销售方税号和校验码
  2. 邮件URL → 未找到有效链接
  3. 图片附件 → 无图片附件

已记录到 invoice_2026-04/errors.jsonl，请手动处理
```

---

## Step 7: Monthly Summary (when requested)

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
  - Row 14: Rental fee (**FIXED VALUE — do NOT modify**)
  - Row 16: Airticket
  - (other rows available for additional categories)
- Row 38: Total (SUM formula, auto-calculates)
- Row 43: Date & Signature

**IMPORTANT:** Row 14 (Rental fee) contains a fixed value set by the user.
Do NOT overwrite or modify this row during summary generation.

### Process

1. Determine the target month (from user message or default to current month)
2. Verify the database `invoices.db` exists
3. Copy `summary.xlsx` template into the monthly folder (if not already there):
   ```bash
   MONTH="2026-04"
   if [ ! -f "invoice_${MONTH}/summary.xlsx" ]; then
     cp summary.xlsx "invoice_${MONTH}/summary.xlsx"
   fi
   ```
4. Read all invoices from SQLite using `get_invoices_by_month()`
5. Categorize each invoice by its 服务项目名称 (service/item name):
   - 餐饮/餐费/食品/外卖 → Food & Meals (row 12)
   - 洗衣/干洗 → Laundry (row 10)
   - 房租/租金/租赁 → Rental fee (row 14)
   - 机票/航空 → Airticket (row 16)
   - Other categories → use available rows (17-37)
6. Fill the summary:

```bash
python3 << 'PYEOF'
import sys
import os

sys.path.insert(0, '.opencode/skills/invoice-processing/scripts')
from db import get_invoices_by_month
from openpyxl import load_workbook

MONTH = "2026-04"

# Read invoice data from SQLite
invoices = get_invoices_by_month(MONTH)

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

# Process invoices from SQLite
for inv in invoices:
    service = str(inv.get('service_name') or '').lower()
    date = inv.get('issue_date')
    amount = inv.get('total_amount')

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

---

## Step 8: Export Monthly Invoices (when requested)

When the user asks to download or export all invoices for a month:

1. Determine the target month (from user message or default to current month)
2. Verify the folder exists
3. If the user explicitly requests `invoices.xlsx`, generate it from SQLite:
   ```bash
   MONTH="2026-04"
   python3 .opencode/skills/invoice-processing/scripts/generate_excel.py "$MONTH"
   ```
4. If summary has not been generated yet, run Step 7 first to create it
5. Zip the entire monthly folder (includes invoices.xlsx if generated, summary.xlsx, and all invoice files):
   ```bash
   MONTH="2026-04"
   cd <thread_dir>
   zip -r "invoice_${MONTH}.zip" "invoice_${MONTH}/"
   ```
6. Send the zip file as an attachment in the reply

If the user asks for a specific month that doesn't exist, reply with available months:
```bash
ls -d invoice_*/
```

Or query SQLite for months with data:
```bash
python3 -c "
import sys
sys.path.insert(0, '.opencode/skills/invoice-processing/scripts')
from db import get_all_months
print('Available months:', get_all_months())
"
```
