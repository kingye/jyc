#!/usr/bin/env python3
"""SQLite database operations for invoice processing."""
import os
import sqlite3
from typing import Optional, List, Dict, Any

DB_PATH = os.environ.get('INVOICES_DB_PATH', 'invoices.db')


def get_connection() -> sqlite3.Connection:
    db_path = DB_PATH
    if not os.path.isabs(db_path):
        db_path = os.path.join(os.getcwd(), DB_PATH)
    conn = sqlite3.connect(db_path)
    conn.row_factory = sqlite3.Row
    return conn


def init_db() -> None:
    conn = get_connection()
    conn.executescript("""
        CREATE TABLE IF NOT EXISTS invoices (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            seq INTEGER,
            invoice_no TEXT,
            issue_date TEXT,
            invoice_type TEXT,
            buyer_name TEXT,
            buyer_tax_id TEXT,
            seller_name TEXT,
            seller_tax_id TEXT NOT NULL,
            service_name TEXT,
            tax_rate TEXT,
            amount REAL,
            tax REAL,
            total_amount REAL NOT NULL,
            remarks TEXT,
            filename TEXT,
            verify_code TEXT,
            source TEXT,
            month TEXT NOT NULL,
            created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
        );
        CREATE UNIQUE INDEX IF NOT EXISTS idx_invoice_no
            ON invoices(invoice_no) WHERE invoice_no IS NOT NULL AND invoice_no != '';
        CREATE INDEX IF NOT EXISTS idx_month ON invoices(month);
        CREATE INDEX IF NOT EXISTS idx_seller_tax_id ON invoices(seller_tax_id);
        CREATE INDEX IF NOT EXISTS idx_created_at ON invoices(created_at);
    """)
    conn.commit()
    conn.close()


def check_duplicate(invoice_no: str, seller_tax_id: str, total_amount: float) -> Optional[int]:
    conn = get_connection()
    cursor = conn.cursor()
    if invoice_no and invoice_no.strip():
        cursor.execute("SELECT id FROM invoices WHERE invoice_no = ?", (invoice_no.strip(),))
        row = cursor.fetchone()
        if row:
            conn.close()
            return row['id']
    cursor.execute(
        "SELECT id FROM invoices WHERE seller_tax_id = ? AND total_amount = ?",
        (seller_tax_id.strip(), total_amount),
    )
    row = cursor.fetchone()
    conn.close()
    return row['id'] if row else None


def insert_invoice(data: Dict[str, Any]) -> int:
    conn = get_connection()
    cursor = conn.cursor()
    cursor.execute(
        """
        INSERT INTO invoices (seq, invoice_no, issue_date, invoice_type, buyer_name,
            buyer_tax_id, seller_name, seller_tax_id, service_name, tax_rate, amount,
            tax, total_amount, remarks, filename, verify_code, source, month)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
    """,
        (
            data.get('seq'),
            data.get('invoice_no'),
            data.get('issue_date'),
            data.get('invoice_type'),
            data.get('buyer_name'),
            data.get('buyer_tax_id'),
            data.get('seller_name'),
            data['seller_tax_id'],
            data.get('service_name'),
            data.get('tax_rate'),
            data.get('amount'),
            data.get('tax'),
            data['total_amount'],
            data.get('remarks'),
            data.get('filename'),
            data.get('verify_code'),
            data.get('source'),
            data['month'],
        ),
    )
    row_id = cursor.lastrowid
    conn.commit()
    conn.close()
    return row_id


def get_invoices_by_month(month: str) -> List[Dict[str, Any]]:
    conn = get_connection()
    cursor = conn.cursor()
    cursor.execute("SELECT * FROM invoices WHERE month = ? ORDER BY created_at", (month,))
    rows = cursor.fetchall()
    conn.close()
    return [dict(row) for row in rows]


def get_all_months() -> List[str]:
    conn = get_connection()
    cursor = conn.cursor()
    cursor.execute("SELECT DISTINCT month FROM invoices ORDER BY month DESC")
    rows = cursor.fetchall()
    conn.close()
    return [row['month'] for row in rows]


def get_stats_by_month(month: str) -> Dict[str, Any]:
    conn = get_connection()
    cursor = conn.cursor()
    cursor.execute(
        "SELECT COUNT(*) as count, SUM(total_amount) as total FROM invoices WHERE month = ?",
        (month,),
    )
    row = cursor.fetchone()
    conn.close()
    return {'count': row['count'] or 0, 'total': row['total'] or 0.0}
