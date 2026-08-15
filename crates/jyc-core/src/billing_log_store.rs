//! Durable per-topic billing ledger.
//!
//! One line is appended per completed LLM call to
//! `.jyc/bill-YYYY-MM-DD.jsonl`. Unlike `agent-session.json`s
//! `session_cost` -- which is scoped to the session and zeroed on every
//! reset -- this ledger is never reset, rotated, or truncated, so it
//! remains the authoritative record of what a topic actually cost.
//!
//! Why date-stamped files rather than one `bill.jsonl`: the dashboard
//! polls twice per second, and each poll needs todays total. A single
//! append-only file would mean re-reading and re-parsing the entire
//! lifetime ledger at 2 Hz on a file that grows without bound.
//! Splitting per day bounds every read to one day of entries. This
//! mirrors `chat_history_YYYY-MM-DD.jsonl` (see `chat_log_store`), so
//! the two logs also agree on which day an event belongs to.
//!
//! No rotation (unlike `activity_log_store`, which caps at 200
//! entries): that store is a bounded debug buffer, whereas this one is
//! a financial record -- truncating it would destroy the totals it
//! exists to produce.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use chrono::Utc;
use serde::{Deserialize, Serialize};

/// Rendered when a topics entries span more than one currency, where
/// summing the amounts would produce a meaningless figure.
pub const MIXED_CURRENCY: &str = "mixed";

/// One completed LLM calls billing record.
///
/// Token counts are stored alongside the computed cost deliberately:
/// the cost pins the rate that was actually in effect at the time (so
/// editing a rate later does not silently rewrite history), while the
/// token counts keep the entry auditable and allow a corrected rate to
/// be replayed over past usage.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BillingEntry {
    /// RFC 3339 timestamp (UTC) of when the call completed.
    pub ts: String,
    /// Model identifier as `"provider/model"`.
    pub model: String,
    /// Provider-reported prompt tokens for this call, including any
    /// served from the prompt cache.
    pub input_tokens: u64,
    /// Provider-reported completion tokens for this call.
    pub output_tokens: u64,
    /// Portion of `input_tokens` served from the prompt cache.
    pub cache_hit_tokens: u64,
    /// Portion of `input_tokens` that **wrote** the prompt cache
    /// (Anthropic only — `cache_creation_input_tokens`). For every
    /// other provider this is `0`. `serde(default)` so old ledger
    /// files (which never wrote the field) deserialize as `0`.
    ///
    /// When set, this is billed at the configured
    /// `cache_creation_per_million` rate; otherwise it falls back to
    /// `cache_hit_per_million`. Storing it per-call means the cost
    /// can be replayed if the user later edits their pricing config.
    #[serde(default)]
    pub cache_creation_tokens: u64,
    /// Computed cost of this single call, in `currency`.
    pub cost: f64,
    /// Currency of `cost`, e.g. `"CNY"`.
    pub currency: String,
    /// What produced this call, so summarization overhead can be told
    /// apart from user-facing work: `"call"` for a main agent-loop turn,
    /// `"summary"` for the ancillary progress / context-compression
    /// calls. Defaults to `"call"` so ledger lines written before this
    /// field existed still deserialize.
    #[serde(default = "default_kind")]
    pub kind: String,
}

/// Ledger `kind` for a normal agent-loop LLM call.
pub const KIND_CALL: &str = "call";

/// Ledger `kind` for an ancillary summarization call (cycle-boundary
/// progress summary, or context compression on session reset).
pub const KIND_SUMMARY: &str = "summary";

fn default_kind() -> String {
    KIND_CALL.to_string()
}

/// Append-only billing ledger, one file per UTC day per topic.
pub struct BillingLogStore;

impl BillingLogStore {
    /// Path to the ledger file for a given `YYYY-MM-DD` date string.
    fn path_for_date(topic_path: &Path, date: &str) -> PathBuf {
        topic_path.join(".jyc").join(format!("bill-{date}.jsonl"))
    }

    /// Todays date as `YYYY-MM-DD` in UTC.
    ///
    /// UTC matches `ChatLogStore`s file stamping, so a cost spike and
    /// the conversation that caused it land in the same days files.
    fn today() -> String {
        Utc::now().format("%Y-%m-%d").to_string()
    }

    /// Append one entry to todays ledger, creating `.jyc/` if needed.
    pub fn append(topic_path: &Path, entry: &BillingEntry) -> anyhow::Result<()> {
        let path = Self::path_for_date(topic_path, &Self::today());
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
        writeln!(file, "{}", serde_json::to_string(entry)?)?;
        file.flush()?;
        Ok(())
    }

    /// Load all entries for a specific `YYYY-MM-DD` date.
    ///
    /// Returns an empty vec when that day has no ledger file. Malformed
    /// lines are skipped rather than failing the whole read, so a single
    /// truncated write (e.g. from a hard kill mid-append) cannot make a
    /// days costs unreadable.
    pub fn load_date(topic_path: &Path, date: &str) -> Vec<BillingEntry> {
        let path = Self::path_for_date(topic_path, date);
        let Ok(file) = File::open(&path) else {
            return Vec::new();
        };
        BufReader::new(file)
            .lines()
            .map_while(Result::ok)
            .filter_map(|line| serde_json::from_str::<BillingEntry>(&line).ok())
            .collect()
    }

    /// Total cost recorded today, with its currency.
    ///
    /// Returns `None` when today has no entries, so callers can omit the
    /// display entirely rather than showing a misleading `0.00`. When
    /// entries span multiple currencies the amounts are still summed but
    /// the currency is reported as [`MIXED_CURRENCY`], since adding
    /// unlike units would otherwise be presented as a real figure.
    pub fn today_total(topic_path: &Path) -> Option<(f64, String)> {
        Self::date_total(topic_path, &Self::today())
    }

    /// Total cost for a specific date, with its currency.
    pub fn date_total(topic_path: &Path, date: &str) -> Option<(f64, String)> {
        let entries = Self::load_date(topic_path, date);
        if entries.is_empty() {
            return None;
        }
        let total = entries.iter().map(|e| e.cost).sum();
        let first = &entries[0].currency;
        let currency = if entries.iter().all(|e| &e.currency == first) {
            first.clone()
        } else {
            MIXED_CURRENCY.to_string()
        };
        Some((total, currency))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn entry(cost: f64, currency: &str) -> BillingEntry {
        BillingEntry {
            ts: Utc::now().to_rfc3339(),
            model: "anthropic/claude-opus-4-7".to_string(),
            input_tokens: 1000,
            output_tokens: 100,
            cache_hit_tokens: 500,
            cache_creation_tokens: 0,
            cost,
            currency: currency.to_string(),
            kind: KIND_CALL.to_string(),
        }
    }

    #[test]
    fn append_then_read_round_trips() {
        let dir = tempdir().unwrap();
        let e = entry(0.05, "USD");
        BillingLogStore::append(dir.path(), &e).unwrap();

        let loaded = BillingLogStore::load_date(dir.path(), &BillingLogStore::today());
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0], e);
    }

    /// `.jyc/` is created on demand -- a brand-new topic must not error.
    #[test]
    fn append_creates_jyc_dir() {
        let dir = tempdir().unwrap();
        let nested = dir.path().join("fresh-topic");
        BillingLogStore::append(&nested, &entry(0.01, "USD")).unwrap();
        assert!(nested.join(".jyc").is_dir());
    }

    #[test]
    fn today_total_sums_all_entries() {
        let dir = tempdir().unwrap();
        for c in [0.01, 0.02, 0.03] {
            BillingLogStore::append(dir.path(), &entry(c, "USD")).unwrap();
        }
        let (total, currency) = BillingLogStore::today_total(dir.path()).unwrap();
        assert!((total - 0.06).abs() < 1e-9, "got {total}");
        assert_eq!(currency, "USD");
    }

    /// No file for today -> `None`, not `Some(0.0)`, so the caller can
    /// omit the display instead of showing a misleading zero.
    #[test]
    fn today_total_is_none_when_no_entries() {
        let dir = tempdir().unwrap();
        assert!(BillingLogStore::today_total(dir.path()).is_none());
    }

    /// The core reason for date-stamped files: yesterdays spending must
    /// not leak into todays total.
    #[test]
    fn other_days_are_excluded_from_today() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".jyc")).unwrap();

        // Hand-write a ledger for a date that is definitely not today.
        let yesterday = BillingLogStore::path_for_date(dir.path(), "2020-01-01");
        let old = serde_json::to_string(&entry(99.0, "USD")).unwrap();
        std::fs::write(&yesterday, format!("{old}\n")).unwrap();

        // Today is still empty...
        assert!(BillingLogStore::today_total(dir.path()).is_none());

        // ...and after todays first entry, the old day is not included.
        BillingLogStore::append(dir.path(), &entry(0.05, "USD")).unwrap();
        let (total, _) = BillingLogStore::today_total(dir.path()).unwrap();
        assert!((total - 0.05).abs() < 1e-9, "got {total}");

        // The old day is still readable on its own.
        let (old_total, _) = BillingLogStore::date_total(dir.path(), "2020-01-01").unwrap();
        assert!((old_total - 99.0).abs() < 1e-9);
    }

    /// Mixed currencies still sum, but are labelled so the UI does not
    /// present unlike units as a real figure.
    #[test]
    fn mixed_currencies_are_flagged() {
        let dir = tempdir().unwrap();
        BillingLogStore::append(dir.path(), &entry(1.0, "USD")).unwrap();
        BillingLogStore::append(dir.path(), &entry(2.0, "CNY")).unwrap();
        let (_, currency) = BillingLogStore::today_total(dir.path()).unwrap();
        assert_eq!(currency, MIXED_CURRENCY);
    }

    /// A truncated final line (hard kill mid-append) must not make the
    /// rest of the day unreadable.
    #[test]
    fn malformed_lines_are_skipped() {
        let dir = tempdir().unwrap();
        BillingLogStore::append(dir.path(), &entry(0.10, "USD")).unwrap();

        let path = BillingLogStore::path_for_date(dir.path(), &BillingLogStore::today());
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(f, "not valid json").unwrap();
        drop(f);

        let (total, _) = BillingLogStore::today_total(dir.path()).unwrap();
        assert!((total - 0.10).abs() < 1e-9, "valid entry must survive");
    }
}
