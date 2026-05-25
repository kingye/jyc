---
name: invoice-processing
description: |
  Process invoices from messages (PDF/image attachments or URLs).
  Extract key values, validate mandatory fields, organize by month, update Excel.
  Use when: receiving invoices, processing receipts, bookkeeping tasks.
---

## Invoice Processing Workflow

When you receive a message containing an invoice (PDF, image attachment, or URL),
follow the steps below. The detailed instructions for each step are in separate
files — read them before processing.

| Step | File | Description |
|------|------|-------------|
| 0 | (this file) | Initialize invoice database (first run only) |
| 1 | (this file) | Determine current month folder |
| 2–3 | [PROCESSING.md](PROCESSING.md) | Download invoice → Extract data (PDF Phase → Image Phase) |
| 4 | [VALIDATION.md](VALIDATION.md) | Validate mandatory fields before write |
| 5–6 | [EXCEL.md](EXCEL.md) | Insert via invoice_add, reply with summary |
| 7–8 | [EXCEL.md](EXCEL.md) | Monthly close/export via invoice_close/invoice_export (when requested) |
| 9 | [VALIDATION.md](VALIDATION.md) | List errors (when requested) |

---

## Mandatory Fields (发票必填字段)

A valid Chinese invoice (发票) MUST contain these 2 fields. If ANY is missing,
the invoice is **invalid** and must NOT be written to the database.

| Field | Description | Format | Required? |
|-------|-------------|--------|-----------|
| **销售方税号** (Seller Tax ID) | 销售方纳税人识别号 | 18 characters (alphanumeric) | **MANDATORY** |
| **价税合计** | Total amount (incl. tax) | Positive number | **MANDATORY** |

**Optional fields (extract if available, do NOT reject if missing):**
| Field | Description | Notes |
|-------|-------------|-------|
| 校验码 | Verification code (20 numeric digits) | Some invoices don't have it; still record in 备注 if present |
| 发票号码 | Invoice number | Extract if possible; warn if missing but do NOT reject |

---

## Processing Flow Overview

```
Email Received
    ↓
┌─────────────────────────────────────────────┐
│ PDF Phase (see PROCESSING.md)               │
│  1. PDF attachments (>50KB)                 │
│     → Extract with Python PdfReader         │
│     → Validate 2 mandatory fields           │
│     → If valid → SUCCESS, stop              │
│                                             │
│  2. Extract URLs from email body            │
│     → Download each URL (max 5)             │
│     → If PDF → extract with PdfReader       │
│     → If HTML → html_parser.py              │
│       → If fails → playwright_extractor.py  │
│       → Re-download extracted URL           │
│     → If Image → tag for Image Phase        │
│     → Validate 2 mandatory fields           │
│     → If valid → SUCCESS, stop              │
│                                             │
│  If ALL PDF sources fail → Image Phase      │
└─────────────────────────────────────────────┘
    ↓
┌─────────────────────────────────────────────┐
│ Image Phase (LAST RESORT, see PROCESSING.md)│
│  3. Tagged image URLs from PDF Phase        │
│     → Use vision MCP tool                   │
│     → Validate 2 mandatory fields           │
│     → If valid → SUCCESS, stop              │
│                                             │
│  4. Image attachments (>50KB, not QR codes) │
│     → Use vision MCP tool                   │
│     → Validate 2 mandatory fields           │
│     → If valid → SUCCESS, stop              │
│                                             │
│  5. Extract URLs from email body for images │
│     → Download each URL (max 5)             │
│     → If Image → use vision MCP             │
│     → If HTML → html_parser.py              │
│       → If fails → playwright_extractor.py  │
│     → Validate 2 mandatory fields           │
│     → If valid → SUCCESS, stop              │
│                                             │
│  If ALL image sources fail → FINAL FAILURE  │
└─────────────────────────────────────────────┘
    ↓
SUCCESS → Step 4 (validate) → Step 5 (insert via invoice_add) → Step 6 (reply)
FAILURE → Log to errors.jsonl → Reply with error details
```

---

## Step 0: Initialize Invoice Database (First Run Only)

The invoice skill now uses the `invoice` MCP for database storage.
Excel files are only generated on-demand when the user requests them.

**On first use (or if `.invoice/` directory doesn't exist):**
```
Use the invoice_init MCP tool to initialize the database.
```

The invoice database is stored at `.invoice/invoice.db` within the thread directory.
It stores all invoice records across all months in a single file.

**Database management:**
- `invoice_init` — Creates SQLite database and downloads OCR models (if needed)
- `invoice_add` — Inserts a new invoice record
- `invoice_list` — Queries invoices (supports `--month`, `--year`, `--category` filters)
- `invoice_show` — Shows details of a specific invoice
- `invoice_close` — Monthly/yearly closing, generates Excel reports and ZIP archive
- `invoice_export` — Exports reports (without closing)

---

## Step 1: Determine Current Month Folder

**IMPORTANT: The month folder is based on when the invoice is RECEIVED/processed, NOT the invoice date (开票日期).**

For example:
- Invoice dated 2026-03 (March) but received in 2026-04 (April) → file into `invoice_2026-04/`
- The 开票日期 is still recorded in the Excel, but the file goes to the current month

The Excel files are now generated on-demand by invoice MCP tools (`invoice_export`, `invoice_close`)
using `rust_xlsxwriter` internally — no template copying needed.

```
Thread directory structure:
<thread_dir>/
  .invoice/                 ← invoice MCP 数据目录
    invoice.db              ← SQLite 数据库（invoice MCP 管理）
    data/                   ← 附件文件（invoice MCP 管理）
  invoice_YYYY-MM/          ← 按月文件夹（保留，存储下载的 PDF）
    errors.jsonl            ← 失败记录（保留）
    INV-2026-0042.pdf       ← 下载的发票 PDF
    INV-2026-0043.jpg
    ...
  invoice_list_YYYY-MM.xlsx  ← Generated on-demand (when user requests export)
  invoice_summary_YYYY-MM.xlsx ← Generated on-demand (when user requests summary)
```

Check if the current month's folder exists:
```bash
MONTH=$(date +%Y-%m)
FOLDER="invoice_${MONTH}"

# Ensure invoice database exists (initialize if needed)
if [ ! -d ".invoice" ]; then
  # Use invoice_init MCP tool to initialize database
  echo "Run invoice_init MCP tool to initialize database"
fi

# Create monthly folder (if needed for invoice files)
if [ ! -d "$FOLDER" ]; then
  mkdir -p "$FOLDER"
fi
```

---

## Rules

### Processing Rules
- ALWAYS check/create the monthly folder before processing
- ALWAYS initialize invoice database via `invoice_init` if `.invoice/` directory does not exist (Step 0)
- ALWAYS validate file format — only PDF and image (JPG/PNG) are valid certified vouchers (合规凭证)
- ALWAYS validate 2 mandatory fields (销售方税号, 价税合计) before writing via `invoice_add`
- ALWAYS follow the STRICT sequential order: PDF attachments → PDF URLs → Image sources (see PROCESSING.md)
- **NEVER skip PDF URL extraction (Step 2b) — if no PDF attachment found, you MUST try to extract URLs from the email body BEFORE processing any image sources**
- NEVER write incomplete or failed invoices to the database — log to errors.jsonl instead
- NEVER save non-PDF/non-image files to the monthly folder — they are not valid vouchers
- NEVER use vision MCP tool on PDF files — use Python PdfReader (pypdf) only for PDFs
- Vision MCP is ONLY for image files (JPG/PNG)

### Extraction Rules
- For PDF files: use Python PdfReader (pypdf) text extraction ONLY
- For image files: use vision MCP tool ONLY
- If PDF text extraction fails → mark that PDF source as failed, try next source
- Do NOT fall back to vision for PDFs — proceed to the next source instead
- If ALL PDF sources fail → proceed to Image Phase
- If ALL image sources fail → log to errors.jsonl as final failure
- **NEVER assume an invoice URL requires login** — all known platforms (51fapiao, Maycur) use public links where the URL itself contains the access credential (hash, code, signatureString). Always try to download first.
- **ALWAYS search `chat_history_*.md` for invoice URLs** — the incoming message prompt may be truncated (forwarded content stripped). The full email body including forwarded URLs is saved in the chat history file. Search ONLY the **latest** received message block (the last `type:received` entry), NOT the entire file. See PROCESSING.md "URL Extraction from Email Body" for the exact script.

### Validation Rules
- 销售方税号 (Seller Tax ID): 18 characters, alphanumeric — **MANDATORY**
- 价税合计 (Total amount): positive number — **MANDATORY**
- 校验码 (Verification Code): 20 numeric digits — stored in dedicated `verify_code` field
- 发票号码 (Invoice number): recommended, warn if missing, but NOT mandatory
- If 校验码 is present, store in dedicated `verify_code` field (not just in 备注)

### File Handling Rules
- Do NOT overwrite existing invoice files
- Do NOT process QR code images — small images (< 50KB) are likely QR codes
- Do NOT follow or scan QR code URLs
- Only PDF and image files (JPG/PNG) are valid — HTML, XML, text are NOT valid invoices
- Maximum 5 URLs processed per phase (PDF Phase and Image Phase each)
- ALWAYS clean up processed attachments from `attachments/` after successful database insert via `invoice_add`
- Do NOT clean up attachments on failure — keep them for manual processing or retry

### Error Handling Rules
- If extraction fails but file IS valid format (PDF/image), save it for manual processing
- If file is NOT valid format (HTML, XML, etc.), do NOT save it
- Report all errors clearly in the reply
- See VALIDATION.md for error log format and error types

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
