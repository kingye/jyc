//! Topic-level metadata persistence in `.jyc/topic.json`.
//!
//! Each topic directory stores a `topic.json` file under `.jyc/` that holds
//! channel-specific metadata. The top-level structure is generic (`channel_type`,
//! `version`), while the `data` field is opaque and channel-specific.
//!
//! Channels are responsible for defining their own `data` schema. The core
//! framework only provides read/write helpers.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Generic topic metadata stored in `.jyc/topic.json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TopicJson {
    /// Channel type that created this topic (e.g., "wecomkf", "email", "feishu").
    pub channel_type: String,
    /// Schema version for forward compatibility.
    pub version: u32,
    /// Opaque channel-specific data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl TopicJson {
    /// File name within the `.jyc/` directory.
    pub const FILENAME: &'static str = "topic.json";

    /// Construct the full path to `topic.json` for a given topic directory.
    pub fn path(topic_path: &Path) -> PathBuf {
        topic_path.join(".jyc").join(Self::FILENAME)
    }

    /// Write `topic.json` to disk, creating `.jyc/` if needed (async).
    pub async fn write(&self, topic_path: &Path) -> Result<()> {
        let path = Self::path(topic_path);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("failed to create .jyc dir: {}", parent.display()))?;
        }
        let content =
            serde_json::to_string_pretty(self).context("failed to serialize topic.json")?;
        tokio::fs::write(&path, content)
            .await
            .with_context(|| format!("failed to write topic.json: {}", path.display()))?;
        Ok(())
    }

    /// Write `topic.json` to disk, creating `.jyc/` if needed (sync).
    pub fn write_sync(&self, topic_path: &Path) -> Result<()> {
        let path = Self::path(topic_path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create .jyc dir: {}", parent.display()))?;
        }
        let content =
            serde_json::to_string_pretty(self).context("failed to serialize topic.json")?;
        std::fs::write(&path, content)
            .with_context(|| format!("failed to write topic.json: {}", path.display()))?;
        Ok(())
    }

    /// Read `topic.json` from disk if it exists (async).
    pub async fn read(topic_path: &Path) -> Result<Option<Self>> {
        let path = Self::path(topic_path);
        if !path.exists() {
            return Ok(None);
        }
        let content = tokio::fs::read_to_string(&path)
            .await
            .with_context(|| format!("failed to read topic.json: {}", path.display()))?;
        let value: Self = serde_json::from_str(&content)
            .with_context(|| format!("failed to parse topic.json: {}", path.display()))?;
        Ok(Some(value))
    }

    /// Read `topic.json` from disk if it exists (sync).
    pub fn read_sync(topic_path: &Path) -> Result<Option<Self>> {
        let path = Self::path(topic_path);
        if !path.exists() {
            return Ok(None);
        }
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read topic.json: {}", path.display()))?;
        let value: Self = serde_json::from_str(&content)
            .with_context(|| format!("failed to parse topic.json: {}", path.display()))?;
        Ok(Some(value))
    }

    /// Convenience: read `data` and deserialize into a channel-specific type.
    pub fn data_as<T: for<'de> Deserialize<'de>>(&self) -> Result<Option<T>> {
        match &self.data {
            Some(v) => serde_json::from_value(v.clone())
                .map(Some)
                .context("failed to deserialize topic.json data field"),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    struct TestChannelData {
        external_userid: String,
        user_name: String,
    }

    #[tokio::test]
    async fn test_write_and_read_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let meta = TopicJson {
            channel_type: "wecomkf".to_string(),
            version: 1,
            data: Some(serde_json::json!({
                "external_userid": "wm123",
                "user_name": "张三"
            })),
        };

        meta.write(tmp.path()).await.unwrap();

        let read_back = TopicJson::read(tmp.path()).await.unwrap().unwrap();
        assert_eq!(read_back.channel_type, "wecomkf");
        assert_eq!(read_back.version, 1);
        assert_eq!(
            read_back.data_as::<TestChannelData>().unwrap(),
            Some(TestChannelData {
                external_userid: "wm123".to_string(),
                user_name: "张三".to_string(),
            })
        );
    }

    #[tokio::test]
    async fn test_read_missing_file_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let result = TopicJson::read(tmp.path()).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_write_creates_jyc_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let meta = TopicJson {
            channel_type: "email".to_string(),
            version: 1,
            data: None,
        };

        meta.write(tmp.path()).await.unwrap();
        assert!(tmp.path().join(".jyc").is_dir());
        assert!(tmp.path().join(".jyc/topic.json").is_file());
    }

    #[tokio::test]
    async fn test_data_as_none_when_data_missing() {
        let meta = TopicJson {
            channel_type: "github".to_string(),
            version: 1,
            data: None,
        };
        let result: Option<TestChannelData> = meta.data_as().unwrap();
        assert!(result.is_none());
    }
}
