//! Topic template initialization.
//!
//! Extracted from the monolithic `topic_manager.rs`; initializes a topic
//! directory from a template and guards against template mismatch.

use anyhow::{Context, Result};
use std::path::Path;

use crate::template_utils::copy_template_files;

#[derive(Debug, thiserror::Error)]
#[error(
    "topic '{topic}' was initialized from template '{existing}' but pattern requires template '{requested}'; refusing to overwrite. Configure distinct `topic_prefix` values for these patterns."
)]
pub struct TemplateMismatch {
    pub topic: String,
    pub existing: String,
    pub requested: String,
}

pub(crate) async fn initialize_topic_from_template(
    topic_path: &Path,
    template_name: &str,
    template_dirs: &crate::template_dirs::TemplateDirs,
) -> Result<()> {
    let jyc_dir = topic_path.join(".jyc");
    let template_marker = jyc_dir.join("template");

    if jyc_dir.exists() {
        match tokio::fs::read_to_string(&template_marker).await {
            Ok(existing) => {
                let existing = existing.trim();
                if existing == template_name {
                    return Ok(());
                }
                let topic_label = topic_path
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| topic_path.display().to_string());
                return Err(TemplateMismatch {
                    topic: topic_label,
                    existing: existing.to_string(),
                    requested: template_name.to_string(),
                }
                .into());
            }
            Err(_) => {
                return Ok(());
            }
        }
    }

    let Some(template_src) = template_dirs.resolve_with_topic(topic_path, template_name) else {
        tracing::warn!(
            template = %template_name,
            "Template directory does not exist in any templates layer"
        );
        return Ok(());
    };

    copy_template_files(&template_src, topic_path).await?;

    tokio::fs::create_dir_all(&jyc_dir).await?;

    tokio::fs::write(&template_marker, template_name)
        .await
        .context("failed to write template name")?;

    tracing::info!(template = %template_name, "Topic initialized from template");

    Ok(())
}

#[cfg(test)]
mod template_init_tests {
    use super::*;
    use tempfile::tempdir;

    async fn make_template(template_dir: &Path, name: &str, body: &str) {
        let dir = template_dir.join(name);
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("AGENTS.md"), body).await.unwrap();
    }

    #[tokio::test]
    async fn fresh_topic_writes_marker() {
        let tmp = tempdir().unwrap();
        let template_dir = tmp.path().join("templates");
        let workspace = tmp.path().join("workspace");
        tokio::fs::create_dir_all(&workspace).await.unwrap();
        make_template(&template_dir, "github-planner", "PLANNER").await;

        let topic_path = workspace.join("issue-1");
        initialize_topic_from_template(&topic_path, "github-planner", &template_dir.clone().into())
            .await
            .unwrap();

        let marker = tokio::fs::read_to_string(topic_path.join(".jyc/template"))
            .await
            .unwrap();
        assert_eq!(marker.trim(), "github-planner");
        assert_eq!(
            tokio::fs::read_to_string(topic_path.join("AGENTS.md"))
                .await
                .unwrap(),
            "PLANNER"
        );
    }

    #[tokio::test]
    async fn matching_template_is_idempotent() {
        let tmp = tempdir().unwrap();
        let template_dir = tmp.path().join("templates");
        let workspace = tmp.path().join("workspace");
        tokio::fs::create_dir_all(&workspace).await.unwrap();
        make_template(&template_dir, "github-planner", "PLANNER").await;

        let topic_path = workspace.join("issue-1");
        initialize_topic_from_template(&topic_path, "github-planner", &template_dir.clone().into())
            .await
            .unwrap();

        // Second call with the same template is a no-op.
        initialize_topic_from_template(&topic_path, "github-planner", &template_dir.clone().into())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn template_mismatch_is_refused() {
        let tmp = tempdir().unwrap();
        let template_dir = tmp.path().join("templates");
        let workspace = tmp.path().join("workspace");
        tokio::fs::create_dir_all(&workspace).await.unwrap();
        make_template(&template_dir, "github-high-level-planner", "HLP").await;
        make_template(&template_dir, "github-planner", "PLANNER").await;

        let topic_path = workspace.join("issue-1");
        // First, init with HLP.
        initialize_topic_from_template(
            &topic_path,
            "github-high-level-planner",
            &template_dir.clone().into(),
        )
        .await
        .unwrap();

        // Then, request a different template for the same topic → must error.
        let err = initialize_topic_from_template(
            &topic_path,
            "github-planner",
            &template_dir.clone().into(),
        )
        .await
        .expect_err("expected TemplateMismatch");
        assert!(
            err.downcast_ref::<TemplateMismatch>().is_some(),
            "expected TemplateMismatch, got: {:#}",
            err
        );

        // AGENTS.md must not have been overwritten.
        let body = tokio::fs::read_to_string(topic_path.join("AGENTS.md"))
            .await
            .unwrap();
        assert_eq!(body, "HLP");
    }
}
