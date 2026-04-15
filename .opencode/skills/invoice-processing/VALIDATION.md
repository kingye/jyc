# Step 4 & 9: Validation and Error Handling

This file covers field validation before writing to Excel (Step 4) and
error log viewing (Step 9).

---

## Step 4: Validate Extraction Before Writing to Excel

**CRITICAL: Do NOT write to invoices.xlsx if the invoice is invalid, extraction
failed, or mandatory fields are missing.**

### Mandatory Field Validation

Before proceeding to update Excel, validate that ALL conditions are met:

1. The invoice file is a valid format (PDF or image — JPG/PNG only)
2. The extraction was successful (non-empty data returned)
3. ALL 3 mandatory fields are present and valid

**Mandatory fields (invoice INVALID if ANY is missing):**

| Field | Validation Rule |
|-------|----------------|
| **销售方税号** (Seller Tax ID) | Must be non-empty, 18 characters, alphanumeric |
| **校验码** (Verification Code) | Must be non-empty, 20 numeric digits |
| **价税合计** (Total amount) | Must be a valid positive number |

**Recommended field (warn if missing but do NOT reject):**

| Field | Notes |
|-------|-------|
| 发票号码 (Invoice number) | Extract if possible; if missing, use sequential naming |

### Validation Logic

```python
def validate_invoice(data: dict) -> tuple[bool, list[str]]:
    """Validate mandatory fields. Returns (is_valid, missing_fields)."""
    missing = []

    # 销售方税号: 18 characters, alphanumeric
    tax_id = str(data.get('销售方税号', '') or '').strip()
    if not tax_id or len(tax_id) != 18:
        missing.append('销售方税号')

    # 校验码: 20 numeric digits
    verify_code = str(data.get('校验码', '') or '').strip()
    if not verify_code or len(verify_code) != 20 or not verify_code.isdigit():
        missing.append('校验码')

    # 价税合计: positive number
    total = data.get('价税合计')
    try:
        total_num = float(str(total).replace(',', '').replace('¥', '').replace('￥', ''))
        if total_num <= 0:
            missing.append('价税合计')
    except (ValueError, TypeError):
        missing.append('价税合计')

    return (len(missing) == 0, missing)
```

### Validation Flow

```
Extraction complete
    ↓
Check 销售方税号 (18 chars, alphanumeric)
    ├─ Missing or invalid → add to missing list
    └─ Valid → continue
    ↓
Check 校验码 (20 digits, numeric)
    ├─ Missing or invalid → add to missing list
    └─ Valid → continue
    ↓
Check 价税合计 (positive number)
    ├─ Missing or invalid → add to missing list
    └─ Valid → continue
    ↓
Any missing fields?
    ├─ YES → Log to errors.jsonl, do NOT write to Excel
    │        If current phase has more sources → try next source
    │        If all sources exhausted → FAILURE
    └─ NO → Proceed to Step 5 (Excel update)
```

**IMPORTANT:** Validation failure does NOT immediately stop processing.
The system tries ALL sources in the current phase before moving to the next phase.
Only when ALL sources in ALL phases fail is it a final failure.

### When to Log Errors

| Condition | Error Type | Action |
|-----------|------------|--------|
| File not PDF or image | `download_failed` | Do NOT save file, log error |
| File could not be downloaded | `download_failed` | Log error |
| HTML extraction failed (both parsers) | `download_failed` | Remove temp file, log error |
| PDF text extraction returned empty | `extraction_failed` | Try next source |
| Vision MCP returned empty/error | `extraction_failed` | Try next source |
| Missing 销售方税号 | `incomplete_data` | Try next source |
| Missing 校验码 | `incomplete_data` | Try next source |
| Missing 价税合计 | `incomplete_data` | Try next source |
| ALL sources in ALL phases failed | `all_sources_failed` | Final error, log everything |

**Note:** Only log to `errors.jsonl` at the END of processing when the final
outcome is determined. Do not log intermediate per-source failures — only log
the final failure with the full list of sources tried.

---

## Error Log Format

**File: `invoice_YYYY-MM/errors.jsonl`**

One JSON object per line (JSON Lines format). Append-only — never overwrite.

```bash
python3 << 'PYEOF'
import json, datetime

error_entry = {
    "timestamp": datetime.datetime.now().isoformat(),
    "error_type": "<download_failed|extraction_failed|incomplete_data|all_sources_failed>",
    "source": "<primary source: attachment filename or download URL>",
    "sender": "<sender email address>",
    "subject": "<email subject>",
    "file_saved_as": "<path to file if saved, or null>",
    "fields_extracted": {
        # Include whatever was successfully extracted (partial data)
        # e.g., "发票号码": "INV-2026-0042", "开票日期": "2026-04-10"
    },
    "fields_missing": ["<list of mandatory fields that are missing>"],
    "sources_tried": [
        # List of all sources attempted and their results
        # e.g., {"type": "pdf_attachment", "file": "invoice.pdf", "result": "missing_tax_id"}
    ],
    "error_detail": "<specific error message explaining what went wrong>"
}

MONTH = "2026-04"
with open(f'invoice_{MONTH}/errors.jsonl', 'a') as f:
    f.write(json.dumps(error_entry, ensure_ascii=False) + '\n')

print(f'Error logged to invoice_{MONTH}/errors.jsonl')
PYEOF
```

### Error Type Values

| error_type | When to use |
|------------|-------------|
| `download_failed` | URL returned error, file is empty, file is not PDF/image (e.g., HTML, XML, text), both HTML parsers failed |
| `extraction_failed` | Text extraction (PDF) or vision (image) returned empty output |
| `incomplete_data` | Extraction returned data but mandatory fields (销售方税号, 校验码, 价税合计) are missing |
| `all_sources_failed` | Both PDF Phase and Image Phase failed — no valid invoice found from any source |

### Example Error Entries

**Download failure (HTML page instead of PDF):**
```json
{"timestamp":"2026-04-15T14:30:00","error_type":"download_failed","source":"https://fapiao.com/download/abc123","sender":"vendor@example.com","subject":"发票-2026-04","file_saved_as":null,"fields_extracted":{},"fields_missing":["销售方税号","校验码","价税合计"],"sources_tried":[{"type":"pdf_url","url":"https://fapiao.com/download/abc123","result":"html_page_no_pdf_extracted"}],"error_detail":"URL returned HTML page, both html_parser.py and playwright_extractor.py failed to extract PDF URL"}
```

**Incomplete data (missing mandatory fields):**
```json
{"timestamp":"2026-04-15T15:00:00","error_type":"incomplete_data","source":"attachments/invoice.pdf","sender":"finance@corp.com","subject":"报销发票","file_saved_as":"invoice_2026-04/unknown_001.pdf","fields_extracted":{"发票号码":"INV-2026-0042","开票日期":"2026-03-20","销售方名称":"XX公司","价税合计":"1060.00"},"fields_missing":["销售方税号","校验码"],"sources_tried":[{"type":"pdf_attachment","file":"attachments/invoice.pdf","result":"missing_tax_id_and_verification_code"}],"error_detail":"PDF text extraction succeeded but 销售方税号 and 校验码 not found in extracted text"}
```

**All sources failed (both phases exhausted):**
```json
{"timestamp":"2026-04-15T16:00:00","error_type":"all_sources_failed","source":"(multiple sources)","sender":"supplier@example.com","subject":"4月发票","file_saved_as":"invoice_2026-04/unknown_002.pdf","fields_extracted":{"开票日期":"2026-04-01","价税合计":"500.00"},"fields_missing":["销售方税号","校验码"],"sources_tried":[{"type":"pdf_attachment","file":"attachments/doc.pdf","result":"missing_verification_code"},{"type":"pdf_url","url":"https://example.com/inv.pdf","result":"download_failed"},{"type":"image_attachment","file":"attachments/scan.jpg","result":"missing_tax_id"}],"error_detail":"All PDF and image sources tried, 销售方税号 and/or 校验码 missing from every source"}
```

### Saving Files on Failure

**If extraction fails but the file IS a valid format (PDF or image):**
- Save it to the monthly folder with a fallback name like `unknown_001.pdf`
- This allows the user to manually process it later
- Record the saved path in `file_saved_as`

**If the file is NOT a valid format (HTML, XML, text, etc.):**
- Do NOT save it to the monthly folder
- Set `file_saved_as` to `null` in the error log

---

## Step 9: List Errors (when requested)

When the user asks to see failed invoices, errors, or problems (e.g., "show errors",
"哪些发票失败了", "list failed invoices"), read and format the error log.

**Trigger phrases:** errors, 错误, 失败, failed, problems, 问题

### Process

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
3. Parse each JSON line and format as a readable list

### Reply Format

**If errors exist:**
```
📋 invoice_2026-04 处理失败记录 (共 2 条)

1. ❌ 下载失败
   来源: https://fapiao.com/download/abc123
   发件人: vendor@example.com
   邮件主题: 发票-2026-04
   错误: URL返回HTML页面，html_parser和playwright均未提取到PDF链接
   已尝试: pdf_url (html_page_no_pdf_extracted)
   文件: 未保存

2. ❌ 数据不完整
   来源: attachments/invoice.pdf
   发件人: finance@corp.com
   邮件主题: 报销发票
   错误: 缺失必填字段 — 销售方税号, 校验码
   已提取: 发票号码=INV-2026-0042, 开票日期=2026-03-20, 价税合计=¥1,060.00
   已尝试: pdf_attachment (missing_tax_id_and_verification_code)
   文件: invoice_2026-04/unknown_001.pdf

3. ❌ 所有来源均失败
   来源: (多个来源)
   发件人: supplier@example.com
   邮件主题: 4月发票
   错误: PDF和图片阶段均未找到完整发票
   缺失字段: 销售方税号, 校验码
   已尝试:
     - pdf_attachment: doc.pdf → 缺少校验码
     - pdf_url: https://example.com/inv.pdf → 下载失败
     - image_attachment: scan.jpg → 缺少销售方税号
   文件: invoice_2026-04/unknown_002.pdf
```

**If no errors:**
```
✅ invoice_2026-04 无失败记录
```

**If multiple months have errors and user doesn't specify a month:**
```bash
ls invoice_*/errors.jsonl 2>/dev/null
```
