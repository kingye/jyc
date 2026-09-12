//! Durable central billing ledger.
//!
//! One line is appended per completed LLM call to
//! `<data_home>/billing/bill-YYYY-MM-DD.jsonl`, tagged with the topic
//! that produced it. Unlike `agent-session.json`s `session_cost` --
//! which is scoped to the session and zeroed on every reset -- this
//! ledger is never reset, rotated, or truncated, so it remains the
//! authoritative record of what a topic actually cost.
//!
//! The ledger is central, not per-topic, because topic directories are
//! deleted when a topic closes (`close_topic`); a per-topic ledger
//! would silently destroy the cost history it exists to keep.
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
//!
//! Concurrency: topics append to the same daily file via `O_APPEND` +
//! a single `writeln!` per entry, which is effectively atomic for
//! these small lines; readers skip a torn line rather than failing
//! (see `load_date`).

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
    /// Topic label (`channel/topic`, e.g. `"agents/jyc"`) that produced
    /// this call. `serde(default)` so legacy lines without the field
    /// still deserialize (they are skipped by the report).
    #[serde(default)]
    pub topic: String,
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
    /// `metered` (real spend) or `subscription` (notional API-equivalent
    /// value), copied from the provider's `billing` config at write
    /// time. Defaults to `metered` so ledgers written before this field
    /// existed load unchanged.
    #[serde(default = "default_billing_metered")]
    pub billing: String,
    /// Input rate per million tokens actually applied to this call —
    /// either the flat rate or the window rate, frozen at billing time.
    /// `serde(default)` so old ledger lines (which never wrote the
    /// field) deserialize as `0.0`.
    #[serde(default)]
    pub input_rate_per_million: f64,
    /// Output rate per million tokens applied to this call. Same
    /// defaulting as `input_rate_per_million`.
    #[serde(default)]
    pub output_rate_per_million: f64,
    /// Cache-hit rate per million tokens applied to this call. Same
    /// defaulting as `input_rate_per_million`.
    #[serde(default)]
    pub cache_hit_rate_per_million: f64,
    /// Label of the `time_windows` entry whose rates applied, e.g.
    /// `"16:30-00:30"`. `None` means flat rates (no `time_windows`
    /// configured, or the call fell outside every window).
    #[serde(default)]
    pub time_window: Option<String>,
    /// Fixed UTC offset (`"+08:00"` etc.) used to judge which window
    /// applied, taken from `pricing.utc_offset`. Empty string for the
    /// default UTC clock.
    #[serde(default)]
    pub utc_offset: String,
}

/// Ledger `kind` for a normal agent-loop LLM call.
pub const KIND_CALL: &str = "call";

/// Ledger `kind` for an ancillary summarization call (cycle-boundary
/// progress summary, or context compression on session reset).
pub const KIND_SUMMARY: &str = "summary";

fn default_kind() -> String {
    KIND_CALL.to_string()
}

fn default_billing_metered() -> String {
    "metered".to_string()
}

/// Append-only billing ledger, one file per UTC day in a central dir.
pub struct BillingLogStore;

impl BillingLogStore {
    /// Central ledger directory: `<data_home>/billing/`. `None` when no
    /// data home is resolvable (callers skip billing with a warning).
    pub fn billing_dir() -> Option<PathBuf> {
        jyc_utils::paths::data_home().map(|home| home.join("billing"))
    }

    /// Topic label recorded in ledger entries: the topic path relative
    /// to `data_home` (`agents/jyc`), matching how `/bill` labels
    /// topics, or the bare topic name for pinned topics living outside
    /// `data_home` (matching the state-dir registry's label).
    pub fn label_for(topic_name: &str, topic_path: &Path) -> String {
        jyc_utils::paths::data_home()
            .and_then(|home| topic_path.strip_prefix(home).ok().map(PathBuf::from))
            .map(|rel| rel.to_string_lossy().into_owned())
            .filter(|rel| !rel.is_empty())
            .unwrap_or_else(|| topic_name.to_string())
    }

    /// Path to the ledger file for a given `YYYY-MM-DD` date string.
    fn path_for_date(billing_dir: &Path, date: &str) -> PathBuf {
        billing_dir.join(format!("bill-{date}.jsonl"))
    }

    /// Todays date as `YYYY-MM-DD` in UTC.
    ///
    /// UTC matches `ChatLogStore`s file stamping, so a cost spike and
    /// the conversation that caused it land in the same days files.
    fn today() -> String {
        Utc::now().format("%Y-%m-%d").to_string()
    }

    /// Append one entry to todays ledger, creating the dir if needed.
    pub fn append(billing_dir: &Path, entry: &BillingEntry) -> anyhow::Result<()> {
        let path = Self::path_for_date(billing_dir, &Self::today());
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
    pub fn load_date(billing_dir: &Path, date: &str) -> Vec<BillingEntry> {
        let path = Self::path_for_date(billing_dir, date);
        let Ok(file) = File::open(&path) else {
            return Vec::new();
        };
        BufReader::new(file)
            .lines()
            .map_while(Result::ok)
            .filter_map(|line| serde_json::from_str::<BillingEntry>(&line).ok())
            .collect()
    }

    /// Total cost recorded today for one topic, with its currency.
    ///
    /// Returns `None` when today has no entries for the topic, so
    /// callers can omit the display entirely rather than showing a
    /// misleading `0.00`. When entries span multiple currencies the
    /// amounts are still summed but the currency is reported as
    /// [`MIXED_CURRENCY`], since adding unlike units would otherwise be
    /// presented as a real figure.
    pub fn today_total(billing_dir: &Path, topic: &str) -> Option<(f64, String)> {
        Self::date_total(billing_dir, topic, &Self::today())
    }

    /// Total cost for one topic on a specific date, with its currency.
    pub fn date_total(billing_dir: &Path, topic: &str, date: &str) -> Option<(f64, String)> {
        let entries: Vec<_> = Self::load_date(billing_dir, date)
            .into_iter()
            .filter(|e| e.topic == topic)
            .collect();
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

    /// Load entries from every ledger file in `dir` whose date
    /// starts with `date_prefix` (`"2026-09-11"` = one day, `"2026-09"`
    /// = one month, `""` = all time).
    ///
    /// Malformed files/lines are skipped, per `load_date`.
    pub fn load_matching(dir: &Path, date_prefix: &str) -> Vec<BillingEntry> {
        let file_prefix = format!("bill-{date_prefix}");
        let mut entries = Vec::new();
        let Ok(read_dir) = std::fs::read_dir(dir) else {
            return entries;
        };
        for file in read_dir.map_while(Result::ok) {
            let name = file.file_name();
            let Some(name) = name.to_str() else { continue };
            if !name.starts_with(&file_prefix) || !name.ends_with(".jsonl") {
                continue;
            }
            let Ok(file) = File::open(file.path()) else {
                continue;
            };
            entries.extend(
                BufReader::new(file)
                    .lines()
                    .map_while(Result::ok)
                    .filter_map(|line| serde_json::from_str::<BillingEntry>(&line).ok()),
            );
        }
        entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn entry(topic: &str, cost: f64, currency: &str) -> BillingEntry {
        BillingEntry {
            ts: Utc::now().to_rfc3339(),
            topic: topic.to_string(),
            model: "anthropic/claude-opus-4-7".to_string(),
            input_tokens: 1000,
            output_tokens: 100,
            cache_hit_tokens: 500,
            cache_creation_tokens: 0,
            cost,
            currency: currency.to_string(),
            kind: KIND_CALL.to_string(),
            billing: "metered".into(),
            input_rate_per_million: 0.0,
            output_rate_per_million: 0.0,
            cache_hit_rate_per_million: 0.0,
            time_window: None,
            utc_offset: String::new(),
        }
    }

    /// Backwards compatibility: ledger lines written before the rate
    /// provenance and topic fields existed must still deserialize, with
    /// the new fields defaulting to `0.0` / `None` / `""`.
    #[test]
    fn legacy_ledger_line_without_rate_fields_deserializes() {
        let dir = tempdir().unwrap();
        let legacy = r#"{"ts":"2026-01-01T00:00:00Z","model":"x/y","input_tokens":1,"output_tokens":2,"cache_hit_tokens":0,"cost":0.05,"currency":"USD"}"#;
        let path = BillingLogStore::path_for_date(dir.path(), &BillingLogStore::today());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, format!("{legacy}\n")).unwrap();
        let loaded = BillingLogStore::load_date(dir.path(), &BillingLogStore::today());
        assert_eq!(loaded.len(), 1);
        let e = &loaded[0];
        assert_eq!(e.cost, 0.05);
        assert_eq!(e.topic, "");
        assert_eq!(e.input_rate_per_million, 0.0);
        assert_eq!(e.output_rate_per_million, 0.0);
        assert_eq!(e.cache_hit_rate_per_million, 0.0);
        assert_eq!(e.time_window, None);
        assert_eq!(e.utc_offset, "");
    }

    /// A round-tripped entry with rates + window preserves them — the
    /// new fields are actually written and read back, not silently lost.
    #[test]
    fn round_trip_preserves_rate_provenance() {
        let dir = tempdir().unwrap();
        let mut e = entry("chan/t", 0.10, "USD");
        e.input_rate_per_million = 3.0;
        e.output_rate_per_million = 15.0;
        e.cache_hit_rate_per_million = 1.5;
        e.time_window = Some("16:30-00:30".to_string());
        e.utc_offset = "+08:00".to_string();
        BillingLogStore::append(dir.path(), &e).unwrap();
        let loaded = BillingLogStore::load_date(dir.path(), &BillingLogStore::today());
        assert_eq!(loaded[0], e);
    }

    #[test]
    fn append_then_read_round_trips() {
        let dir = tempdir().unwrap();
        let e = entry("chan/t", 0.05, "USD");
        BillingLogStore::append(dir.path(), &e).unwrap();

        let loaded = BillingLogStore::load_date(dir.path(), &BillingLogStore::today());
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0], e);
    }

    /// The central dir is created on demand -- a fresh deployment must
    /// not error on its first billable call.
    #[test]
    fn append_creates_billing_dir() {
        let dir = tempdir().unwrap();
        let nested = dir.path().join("billing");
        BillingLogStore::append(&nested, &entry("chan/t", 0.01, "USD")).unwrap();
        assert!(nested.is_dir());
    }

    #[test]
    fn today_total_sums_one_topics_entries() {
        let dir = tempdir().unwrap();
        for c in [0.01, 0.02, 0.03] {
            BillingLogStore::append(dir.path(), &entry("chan/t", c, "USD")).unwrap();
        }
        // Another topic's spend lands in the same daily file and must
        // not leak into this topic's total.
        BillingLogStore::append(dir.path(), &entry("chan/other", 9.0, "USD")).unwrap();
        let (total, currency) = BillingLogStore::today_total(dir.path(), "chan/t").unwrap();
        assert!((total - 0.06).abs() < 1e-9, "got {total}");
        assert_eq!(currency, "USD");
    }

    /// No entries for the topic today -> `None`, not `Some(0.0)`, so
    /// the caller can omit the display instead of showing a misleading
    /// zero.
    #[test]
    fn today_total_is_none_when_no_entries() {
        let dir = tempdir().unwrap();
        assert!(BillingLogStore::today_total(dir.path(), "chan/t").is_none());
        BillingLogStore::append(dir.path(), &entry("chan/other", 1.0, "USD")).unwrap();
        assert!(BillingLogStore::today_total(dir.path(), "chan/t").is_none());
    }

    /// The core reason for date-stamped files: yesterdays spending must
    /// not leak into todays total.
    #[test]
    fn other_days_are_excluded_from_today() {
        let dir = tempdir().unwrap();

        // Hand-write a ledger for a date that is definitely not today.
        let yesterday = BillingLogStore::path_for_date(dir.path(), "2020-01-01");
        let old = serde_json::to_string(&entry("chan/t", 99.0, "USD")).unwrap();
        std::fs::write(&yesterday, format!("{old}\n")).unwrap();

        // Today is still empty...
        assert!(BillingLogStore::today_total(dir.path(), "chan/t").is_none());

        // ...and after todays first entry, the old day is not included.
        BillingLogStore::append(dir.path(), &entry("chan/t", 0.05, "USD")).unwrap();
        let (total, _) = BillingLogStore::today_total(dir.path(), "chan/t").unwrap();
        assert!((total - 0.05).abs() < 1e-9, "got {total}");

        // The old day is still readable on its own.
        let (old_total, _) =
            BillingLogStore::date_total(dir.path(), "chan/t", "2020-01-01").unwrap();
        assert!((old_total - 99.0).abs() < 1e-9);
    }

    /// Mixed currencies still sum, but are labelled so the UI does not
    /// present unlike units as a real figure.
    #[test]
    fn mixed_currencies_are_flagged() {
        let dir = tempdir().unwrap();
        BillingLogStore::append(dir.path(), &entry("chan/t", 1.0, "USD")).unwrap();
        BillingLogStore::append(dir.path(), &entry("chan/t", 2.0, "CNY")).unwrap();
        let (_, currency) = BillingLogStore::today_total(dir.path(), "chan/t").unwrap();
        assert_eq!(currency, MIXED_CURRENCY);
    }

    /// A truncated final line (hard kill mid-append) must not make the
    /// rest of the day unreadable.
    #[test]
    fn malformed_lines_are_skipped() {
        let dir = tempdir().unwrap();
        BillingLogStore::append(dir.path(), &entry("chan/t", 0.10, "USD")).unwrap();

        let path = BillingLogStore::path_for_date(dir.path(), &BillingLogStore::today());
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(f, "not valid json").unwrap();
        drop(f);

        let (total, _) = BillingLogStore::today_total(dir.path(), "chan/t").unwrap();
        assert!((total - 0.10).abs() < 1e-9, "valid entry must survive");
    }
}
