use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

use crate::inspect::types::ActivityEntry;

const DEFAULT_MAX_ENTRIES: usize = 200;
const ROTATION_THRESHOLD: f64 = 1.5;

/// Persists activity entries to `.jyc/activity.jsonl` per thread.
///
/// Each thread's activity log is stored as one JSON line per entry,
/// enabling efficient append-only writes and bounded history retrieval.
pub struct ActivityLogStore;

impl ActivityLogStore {
    fn jsonl_path(thread_path: &Path) -> std::path::PathBuf {
        thread_path.join(".jyc").join("activity.jsonl")
    }

    /// Append an activity entry to the thread's JSONL log file.
    ///
    /// Creates the `.jyc/` directory if it doesn't exist. After writing,
    /// checks whether lazy rotation is needed (1.5x threshold).
    pub fn append(thread_path: &Path, entry: &ActivityEntry) -> anyhow::Result<()> {
        let path = Self::jsonl_path(thread_path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
        let json = serde_json::to_string(entry)?;
        writeln!(file, "{json}")?;
        file.flush()?;

        let metadata = file.metadata()?;
        let approx_lines = (metadata.len() / 80).max(1);
        if approx_lines as f64 > DEFAULT_MAX_ENTRIES as f64 * ROTATION_THRESHOLD {
            drop(file);
            Self::rotate_if_needed(thread_path)?;
        }
        Ok(())
    }

    /// Load the most recent activity entries from the thread's JSONL log.
    ///
    /// Returns up to `max_entries` entries in chronological order (oldest first).
    /// Returns an empty vec if the file doesn't exist or has no valid entries.
    pub fn load_recent(
        thread_path: &Path,
        max_entries: usize,
    ) -> anyhow::Result<Vec<ActivityEntry>> {
        let path = Self::jsonl_path(thread_path);
        if !path.exists() {
            return Ok(Vec::new());
        }
        let file = File::open(&path)?;
        let reader = BufReader::new(file);
        let lines: Vec<String> = reader.lines().filter_map(|l| l.ok()).collect();
        let start = lines.len().saturating_sub(max_entries);
        Ok(lines[start..]
            .iter()
            .filter_map(|line| serde_json::from_str::<ActivityEntry>(line).ok())
            .collect())
    }

    /// Rotate the JSONL file if it exceeds the default max entries.
    ///
    /// Keeps only the most recent `DEFAULT_MAX_ENTRIES` lines,
    /// rewriting the file in place.
    pub fn rotate_if_needed(thread_path: &Path) -> anyhow::Result<()> {
        Self::rotate_if_needed_with_max(thread_path, DEFAULT_MAX_ENTRIES)
    }

    fn rotate_if_needed_with_max(thread_path: &Path, max_entries: usize) -> anyhow::Result<()> {
        let path = Self::jsonl_path(thread_path);
        if !path.exists() {
            return Ok(());
        }
        let file = File::open(&path)?;
        let reader = BufReader::new(file);
        let lines: Vec<String> = reader.lines().filter_map(|l| l.ok()).collect();
        if lines.len() <= max_entries {
            return Ok(());
        }
        let keep = lines[lines.len().saturating_sub(max_entries)..].join("\n");
        let mut file = OpenOptions::new().write(true).truncate(true).open(&path)?;
        writeln!(file, "{keep}")?;
        file.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use tempfile::tempdir;

    fn make_entry(text: &str) -> ActivityEntry {
        ActivityEntry {
            time: "00:00:00".to_string(),
            text: text.to_string(),
            timestamp: Some(Utc::now().to_rfc3339()),
        }
    }

    #[test]
    fn test_append_and_load_recent() {
        let dir = tempdir().unwrap();
        let thread_path = dir.path().join("test-thread");
        std::fs::create_dir_all(thread_path.join(".jyc")).unwrap();

        for i in 0..5 {
            let entry = make_entry(&format!("entry {i}"));
            ActivityLogStore::append(&thread_path, &entry).unwrap();
        }

        let loaded = ActivityLogStore::load_recent(&thread_path, 3).unwrap();
        assert_eq!(loaded.len(), 3);
        assert!(loaded[0].text.contains("entry 2"));
        assert!(loaded[1].text.contains("entry 3"));
        assert!(loaded[2].text.contains("entry 4"));
    }

    #[test]
    fn test_load_empty_file() {
        let dir = tempdir().unwrap();
        let thread_path = dir.path().join("no-such-thread");
        let loaded = ActivityLogStore::load_recent(&thread_path, 10).unwrap();
        assert!(loaded.is_empty());
    }

    #[test]
    fn test_rotation() {
        let dir = tempdir().unwrap();
        let thread_path = dir.path().join("rot-test");
        std::fs::create_dir_all(thread_path.join(".jyc")).unwrap();

        for i in 0..300 {
            let entry = make_entry(&format!("entry {i}"));
            ActivityLogStore::append(&thread_path, &entry).unwrap();
        }

        ActivityLogStore::rotate_if_needed_with_max(&thread_path, 200).unwrap();
        let loaded = ActivityLogStore::load_recent(&thread_path, 1000).unwrap();
        assert_eq!(loaded.len(), 200);
        assert!(loaded[0].text.contains("entry 100"));
    }
}
