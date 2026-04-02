use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Per-channel IMAP monitoring state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonitorState {
    pub last_sequence_number: u32,
    pub last_processed_uid: Option<u32>,
    pub last_processed_timestamp: Option<String>,
    pub uid_validity: Option<u32>,
}

impl Default for MonitorState {
    fn default() -> Self {
        Self {
            last_sequence_number: 0,
            last_processed_uid: None,
            last_processed_timestamp: None,
            uid_validity: None,
        }
    }
}

/// Manages per-channel IMAP state (sequence numbers, processed UIDs).
///
/// Each channel gets its own StateManager instance to prevent
/// race conditions when multiple channels run concurrently.
pub struct StateManager {
    /// Directory for state files: <channel>/.imap/
    state_dir: PathBuf,
    /// In-memory state
    state: MonitorState,
    /// Set of processed UIDs (loaded from .processed-uids.txt)
    processed_uids: HashSet<u32>,
}

impl StateManager {
    /// Compact when the processed UIDs set exceeds this many entries.
    const COMPACTION_THRESHOLD: usize = 5000;
    /// Keep UIDs within this many sequence numbers below `last_sequence_number`.
    const COMPACTION_KEEP_BUFFER: u32 = 1000;

    /// Create a StateManager for a specific channel.
    ///
    /// State files live in `<workdir>/<channel_name>/.imap/`.
    pub fn for_channel(workdir: &Path, channel_name: &str) -> Self {
        let state_dir = workdir.join(channel_name).join(".imap");
        Self {
            state_dir,
            state: MonitorState::default(),
            processed_uids: HashSet::new(),
        }
    }

    /// Initialize: create state directory and load existing state.
    pub async fn initialize(&mut self) -> Result<()> {
        tokio::fs::create_dir_all(&self.state_dir)
            .await
            .with_context(|| {
                format!(
                    "failed to create state directory: {}",
                    self.state_dir.display()
                )
            })?;

        self.load().await?;
        self.load_processed_uids().await?;
        Ok(())
    }

    /// Load state from .state.json.
    async fn load(&mut self) -> Result<()> {
        let state_file = self.state_dir.join(".state.json");
        if state_file.exists() {
            let content = tokio::fs::read_to_string(&state_file)
                .await
                .with_context(|| format!("failed to read {}", state_file.display()))?;
            self.state = serde_json::from_str(&content)
                .with_context(|| format!("failed to parse {}", state_file.display()))?;
            tracing::debug!(
                last_seq = self.state.last_sequence_number,
                last_uid = ?self.state.last_processed_uid,
                "Loaded monitoring state"
            );
        }
        Ok(())
    }

    /// Save state to .state.json.
    pub async fn save(&self) -> Result<()> {
        let state_file = self.state_dir.join(".state.json");
        let content = serde_json::to_string_pretty(&self.state)
            .context("failed to serialize state")?;
        tokio::fs::write(&state_file, content)
            .await
            .with_context(|| format!("failed to write {}", state_file.display()))?;
        Ok(())
    }

    /// Load the processed UIDs set from .processed-uids.txt.
    async fn load_processed_uids(&mut self) -> Result<()> {
        let uids_file = self.state_dir.join(".processed-uids.txt");
        if uids_file.exists() {
            let content = tokio::fs::read_to_string(&uids_file)
                .await
                .with_context(|| format!("failed to read {}", uids_file.display()))?;
            self.processed_uids = content
                .lines()
                .filter_map(|line| line.trim().parse::<u32>().ok())
                .collect();
            tracing::debug!(
                count = self.processed_uids.len(),
                "Loaded processed UIDs"
            );
        }
        Ok(())
    }

    /// Track a UID as processed (append to file and add to in-memory set).
    pub async fn track_uid(&mut self, uid: u32) -> Result<()> {
        self.processed_uids.insert(uid);

        let uids_file = self.state_dir.join(".processed-uids.txt");
        use tokio::io::AsyncWriteExt;
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&uids_file)
            .await
            .with_context(|| format!("failed to open {}", uids_file.display()))?;
        file.write_all(format!("{uid}\n").as_bytes()).await?;

        // Auto-compact when the set grows beyond a reasonable threshold.
        // This prevents unbounded memory growth over months of operation.
        if self.processed_uids.len() > Self::COMPACTION_THRESHOLD {
            self.compact().await.ok();
        }

        Ok(())
    }

    /// Compact the processed UIDs set by removing UIDs below a safe floor.
    ///
    /// Keeps only UIDs >= (last_sequence_number - COMPACTION_KEEP_BUFFER) to
    /// prevent unbounded growth while still protecting against reprocessing
    /// of recent messages.
    async fn compact(&mut self) -> Result<()> {
        let floor = self
            .state
            .last_sequence_number
            .saturating_sub(Self::COMPACTION_KEEP_BUFFER);

        if floor == 0 {
            return Ok(());
        }

        let before = self.processed_uids.len();
        self.processed_uids.retain(|&uid| uid >= floor);
        let after = self.processed_uids.len();

        if before == after {
            return Ok(());
        }

        // Rewrite the file with the compacted set
        let uids_file = self.state_dir.join(".processed-uids.txt");
        let content: String = self
            .processed_uids
            .iter()
            .map(|uid| format!("{uid}\n"))
            .collect();
        tokio::fs::write(&uids_file, content)
            .await
            .with_context(|| format!("failed to write compacted {}", uids_file.display()))?;

        tracing::info!(
            before = before,
            after = after,
            floor = floor,
            "Compacted processed UIDs"
        );

        Ok(())
    }

    /// Check if a UID has been processed.
    pub fn is_processed(&self, uid: u32) -> bool {
        self.processed_uids.contains(&uid)
    }

    /// Get the last sequence number.
    pub fn last_sequence_number(&self) -> u32 {
        self.state.last_sequence_number
    }

    /// Update the sequence number and optionally the last processed UID.
    pub fn update_sequence(&mut self, seq: u32, uid: Option<u32>) {
        self.state.last_sequence_number = seq;
        if let Some(uid) = uid {
            self.state.last_processed_uid = Some(uid);
        }
        self.state.last_processed_timestamp =
            Some(chrono::Utc::now().to_rfc3339());
    }

    /// Get the UID validity value.
    pub fn uid_validity(&self) -> Option<u32> {
        self.state.uid_validity
    }

    /// Update the UID validity.
    pub fn update_uid_validity(&mut self, validity: u32) {
        self.state.uid_validity = Some(validity);
    }

    /// Get the current state (for display).
    pub fn state(&self) -> &MonitorState {
        &self.state
    }

    /// Get the number of processed UIDs.
    pub fn processed_uid_count(&self) -> usize {
        self.processed_uids.len()
    }

    /// Reset all state (for --reset flag).
    pub async fn reset(&mut self) -> Result<()> {
        self.state = MonitorState::default();
        self.processed_uids.clear();

        let state_file = self.state_dir.join(".state.json");
        if state_file.exists() {
            tokio::fs::remove_file(&state_file).await?;
        }

        let uids_file = self.state_dir.join(".processed-uids.txt");
        if uids_file.exists() {
            tokio::fs::remove_file(&uids_file).await?;
        }

        tracing::info!("Monitoring state reset");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_state_manager_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let mut sm = StateManager::for_channel(tmp.path(), "test-channel");
        sm.initialize().await.unwrap();

        // Initial state
        assert_eq!(sm.last_sequence_number(), 0);
        assert!(!sm.is_processed(100));

        // Update and save
        sm.update_sequence(42, Some(100));
        sm.track_uid(100).await.unwrap();
        sm.track_uid(101).await.unwrap();
        sm.save().await.unwrap();

        // Reload
        let mut sm2 = StateManager::for_channel(tmp.path(), "test-channel");
        sm2.initialize().await.unwrap();

        assert_eq!(sm2.last_sequence_number(), 42);
        assert!(sm2.is_processed(100));
        assert!(sm2.is_processed(101));
        assert!(!sm2.is_processed(102));
        assert_eq!(sm2.processed_uid_count(), 2);
    }

    #[tokio::test]
    async fn test_state_manager_reset() {
        let tmp = tempfile::tempdir().unwrap();
        let mut sm = StateManager::for_channel(tmp.path(), "test-channel");
        sm.initialize().await.unwrap();

        sm.update_sequence(42, Some(100));
        sm.track_uid(100).await.unwrap();
        sm.save().await.unwrap();

        sm.reset().await.unwrap();

        assert_eq!(sm.last_sequence_number(), 0);
        assert!(!sm.is_processed(100));
        assert_eq!(sm.processed_uid_count(), 0);
    }

    #[tokio::test]
    async fn test_uid_validity() {
        let tmp = tempfile::tempdir().unwrap();
        let mut sm = StateManager::for_channel(tmp.path(), "test-channel");
        sm.initialize().await.unwrap();

        assert_eq!(sm.uid_validity(), None);
        sm.update_uid_validity(12345);
        assert_eq!(sm.uid_validity(), Some(12345));
    }
}
