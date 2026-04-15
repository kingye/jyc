# Step 5–8: Excel, Reply, Summary & Export

This file covers writing validated invoice data to Excel (Step 5), replying to
the user (Step 6), monthly summary generation (Step 7), and export (Step 8).

---

## Step 5: Update Excel

**Only reached if Step 4 validation passes (all 3 mandatory fields present).**

**IMPORTANT: Check for duplicate invoice numbers before adding.**

### 校验码 Handling

The 校验码 (Verification Code) is written at the end of the 备注 (Remarks)
column (column N). Format:

```
校验码: 12345678901234567890
```

If there are other remarks, append 校验码 after them:
```
其他备注内容 校验码: 12345678901234567890
```

### Excel Writing

Use Python with openpyxl to check and add a row:

```bash
python3 << 'PYEOF'
from openpyxl import load_workbook

INVOICE_NO = '<发票号码>'       # May be empty if not extracted
SELLER_TAX_ID = '<销售方税号>'  # MANDATORY — 18 chars
VERIFY_CODE = '<校验码>'        # MANDATORY — 20 digits
TOTAL = <价税合计>              # MANDATORY — positive number
MONTH = '2026-04'

wb = load_workbook(f'invoice_{MONTH}/invoices.xlsx')
ws = wb.active

# Check for duplicate: use 销售方税号 + 价税合计 as primary key
# (发票号码 may not always be available)
existing_rows = []
for row in range(2, ws.max_row + 1):
    existing_tax_id = str(ws.cell(row=row, column=8).value or '').strip()
    existing_total = ws.cell(row=row, column=13).value
    existing_inv_no = str(ws.cell(row=row, column=2).value or '').strip()

    # Check by invoice number if available
    if INVOICE_NO and existing_inv_no == INVOICE_NO.strip():
        existing_rows.append(row)
    # Also check by tax ID + total combination
    elif existing_tax_id == SELLER_TAX_ID.strip() and existing_total == TOTAL:
        existing_rows.append(row)

if existing_rows:
    print(f'DUPLICATE: Invoice already exists at row(s): {existing_rows}')
    print('Skipping - do not add duplicate')
    exit(0)

# Find next empty row
next_row = ws.max_row + 1

# Build remarks with 校验码 appended
remarks = ''
if VERIFY_CODE:
    remarks = f'校验码: {VERIFY_CODE}'

# Template columns:
# A:序号 B:发票号码 C:开票日期 D:发票类型 E:购买方名称
# F:购买方税号 G:销售方名称 H:销售方税号 I:服务项目名称
# J:税率 K:金额 L:税额 M:价税合计 N:备注 O:文件名
ws.cell(row=next_row, column=1, value=next_row - 1)        # 序号
ws.cell(row=next_row, column=2, value=INVOICE_NO or '')     # 发票号码 (may be empty)
ws.cell(row=next_row, column=3, value='<开票日期>')         # 开票日期
ws.cell(row=next_row, column=4, value='<发票类型>')         # 发票类型
ws.cell(row=next_row, column=5, value='<购买方名称>')       # 购买方名称
ws.cell(row=next_row, column=6, value='<购买方税号>')       # 购买方税号
ws.cell(row=next_row, column=7, value='<销售方名称>')       # 销售方名称
ws.cell(row=next_row, column=8, value=SELLER_TAX_ID)       # 销售方税号 [MANDATORY]
ws.cell(row=next_row, column=9, value='<服务项目名称>')     # 服务项目名称
ws.cell(row=next_row, column=10, value='<税率>')            # 税率
ws.cell(row=next_row, column=11, value='<金额>')            # 金额
ws.cell(row=next_row, column=12, value='<税额>')            # 税额
ws.cell(row=next_row, column=13, value=TOTAL)               # 价税合计 [MANDATORY]
ws.cell(row=next_row, column=14, value=remarks)             # 备注 (校验码 appended)
ws.cell(row=next_row, column=15, value='<filename>')        # 文件名

wb.save(f'invoice_{MONTH}/invoices.xlsx')
print('Row added successfully')
PYEOF
```

**The column mapping above is FIXED — do NOT read Excel headers to inspect the layout.**
The template always has these 15 columns in this exact order. Just use the script above directly.

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
Excel: invoice_2026-04/invoices.xlsx (第4行)
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
Excel: invoice_2026-04/invoices.xlsx (第4行)

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
Excel: invoice_2026-04/invoices.xlsx (第4行)
```

### Duplicate

```
⚠️ 发票已忽略

发票号码 INV-2026-0042 已存在于 invoice_2026-04/invoices.xlsx (第3行)
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

---

## Step 8: Export Monthly Invoices (when requested)

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
