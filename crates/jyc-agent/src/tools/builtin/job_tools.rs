//! Job management tools for the agent — create, list, delete, and toggle
//! scheduled jobs from within any topic.
//!
//! Jobs are stored per-topic in `<topic>/.jyc/jobs/<id>.json`. Each tool
//! creates a scoped `JobStore` from the `ToolContext.working_dir` (which is
//! the topic's directory) at execution time.

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use jyc_core::job_store::JobStore;
use jyc_types::JobConfig;
use serde_json::{Value, json};

use super::super::{Tool, ToolContext, ToolOutput};

/// Default max jobs per topic for agent tools (matches config default).
const DEFAULT_MAX_JOBS: usize = 10;

/// Helper to create a per-topic JobStore from the working directory.
async fn store_from_ctx(ctx: &ToolContext<'_>) -> Result<JobStore> {
    JobStore::new(
        ctx.current_topic.as_deref().unwrap_or(""),
        ctx.working_dir,
        DEFAULT_MAX_JOBS,
    )
    .await
}

/// List all scheduled jobs in the current topic.
pub struct JobListTool;

#[async_trait]
impl Tool for JobListTool {
    fn name(&self) -> &str {
        "job_list"
    }

    fn description(&self) -> &str {
        "List all scheduled jobs in this topic. Returns a JSON array of job \
         configurations including id, cron/at schedule, enabled status, prompt, \
         and next fire time. Use this to see what jobs exist before creating or \
         modifying them."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    async fn execute(&self, _input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput> {
        let store = store_from_ctx(ctx).await?;
        let jobs = store.list().await?;
        let summary: Vec<Value> = jobs
            .iter()
            .map(|j| {
                json!({
                    "id": j.id,
                    "cron": j.cron,
                    "at": j.at.map(|t| t.to_rfc3339()),
                    "enabled": j.enabled,
                    "topic_name": j.topic_name,
                    "channel_name": j.channel_name,
                    "channel": j.channel,
                    "prompt": j.prompt.chars().take(100).collect::<String>(),
                    "next_fire_at": j.next_fire_at.map(|t| t.to_rfc3339()),
                    "last_fired_at": j.last_fired_at.map(|t| t.to_rfc3339()),
                    "created_at": j.created_at.to_rfc3339(),
                })
            })
            .collect();

        Ok(ToolOutput::success(
            serde_json::to_string_pretty(&summary).unwrap_or_else(|_| "[]".to_string()),
        ))
    }
}

/// Create a new scheduled job in the current topic.
pub struct JobCreateTool;

#[async_trait]
impl Tool for JobCreateTool {
    fn name(&self) -> &str {
        "job_create"
    }

    fn description(&self) -> &str {
        "Create a new scheduled job. Exactly one of 'cron' or 'at' is required — \
         providing both or neither is an error. 'cron' is a 7-field expression for \
         recurring jobs ('sec min hour dom mon dow year', e.g. '0 0 8 * * * *' for \
         daily at 8 AM); 'at' is an ISO 8601 timestamp for one-time jobs (e.g. \
         '2026-06-22T08:00:00Z'). The job fires by injecting the provided prompt \
         into the originating topic. Returns the created job ID. Stop a job later \
         with job_delete (remove) or job_toggle (pause/keep)."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "cron": {
                    "type": "string",
                    "description": "Cron expression for a RECURRING job (7-field format: 'sec min hour dom mon dow year', e.g. '0 0 8 * * * *'). Provide either this or 'at', never both."
                },
                "at": {
                    "type": "string",
                    "description": "ISO 8601 timestamp for a ONE-TIME job (e.g. '2026-06-22T08:00:00Z'). Provide either this or 'cron', never both."
                },
                "prompt": {
                    "type": "string",
                    "description": "Instructions for the AI to execute when the job fires"
                }
            },
            "required": ["prompt"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput> {
        let prompt = input
            .get("prompt")
            .and_then(|p| p.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'prompt' parameter"))?
            .to_string();

        let cron = input.get("cron").and_then(|c| c.as_str());
        let at_str = input.get("at").and_then(|a| a.as_str());
        if cron.is_some() && at_str.is_some() {
            return Ok(ToolOutput::error(
                "provide either 'cron' or 'at', not both".to_string(),
            ));
        }

        // The topic name is the working directory's name (the topic dir).
        let topic_name = ctx
            .working_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string();

        // The channel name comes from the live turn context. It is NOT
        // derivable from the directory path: the agents-root channel's
        // workspace is the agents root itself, so path-walking walked past
        // the channel level and stamped the OS username as the channel.
        // The scheduler delivers fired messages via
        // `topic_managers[job.channel_name]`, so a wrong name makes the job
        // undeliverable.
        let Some(channel_name) = ctx.current_channel.clone() else {
            return Ok(ToolOutput::error(
                "job scheduling requires a channel context".to_string(),
            ));
        };

        let channel = channel_name.clone();

        let job = if let Some(cron_expr) = cron {
            // Check if the expression is valid by trying to compute next fire time.
            // The cron crate (jyc-types dependency) is used for actual parsing.
            let check = cron_expr.parse::<cron::Schedule>();
            if check.is_err() {
                return Ok(ToolOutput::error(format!(
                    "Invalid cron expression: '{}'. Use 7-field format: 'sec min hour dom mon dow year'",
                    cron_expr
                )));
            }
            JobConfig::new_recurring(cron_expr, topic_name, channel, channel_name, prompt)
        } else if let Some(at_str) = at_str {
            let at = match DateTime::parse_from_rfc3339(at_str) {
                Ok(dt) => dt.with_timezone(&Utc),
                Err(_) => match at_str.parse::<DateTime<Utc>>() {
                    Ok(dt) => dt,
                    Err(e) => {
                        return Ok(ToolOutput::error(format!(
                            "Invalid 'at' timestamp '{}': {}. Use ISO 8601 format (e.g. '2026-06-22T08:00:00Z')",
                            at_str, e
                        )));
                    }
                },
            };
            JobConfig::new_one_time(at, topic_name, channel, channel_name, prompt)
        } else {
            return Ok(ToolOutput::error(
                "Must provide either 'cron' (recurring) or 'at' (one-time) parameter".to_string(),
            ));
        };

        let store = store_from_ctx(ctx).await?;
        match store.create(&job).await {
            Ok(()) => {
                let schedule_info = if let Some(ref cron) = job.cron {
                    format!("cron='{}'", cron)
                } else if let Some(ref at) = job.at {
                    format!("at='{}'", at.to_rfc3339())
                } else {
                    "unknown schedule".to_string()
                };
                Ok(ToolOutput::success(format!(
                    "Job created successfully.\nID: {}\nSchedule: {}\nPrompt: {}",
                    job.id, schedule_info, job.prompt
                )))
            }
            Err(e) => Ok(ToolOutput::error(format!("Failed to create job: {e}"))),
        }
    }
}

/// Delete a scheduled job by ID from the current topic.
pub struct JobDeleteTool;

#[async_trait]
impl Tool for JobDeleteTool {
    fn name(&self) -> &str {
        "job_delete"
    }

    fn description(&self) -> &str {
        "Delete a scheduled job by ID from this topic. The job will no longer \
         fire. Returns success or an error if the job doesn't exist. \
         Use 'job_list' to find job IDs."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "The ID of the job to delete"
                }
            },
            "required": ["id"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput> {
        let id = input
            .get("id")
            .and_then(|i| i.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'id' parameter"))?;

        let store = store_from_ctx(ctx).await?;
        match store.delete(id).await {
            Ok(true) => Ok(ToolOutput::success(format!("Job '{}' deleted", id))),
            Ok(false) => Ok(ToolOutput::error(format!("Job '{}' not found", id))),
            Err(e) => Ok(ToolOutput::error(format!("Failed to delete job: {e}"))),
        }
    }
}

/// Toggle (enable/disable) a scheduled job.
pub struct JobToggleTool;

#[async_trait]
impl Tool for JobToggleTool {
    fn name(&self) -> &str {
        "job_toggle"
    }

    fn description(&self) -> &str {
        "Enable or disable a scheduled job by ID. When disabled, the job \
         will not fire until re-enabled. Use 'job_list' to find job IDs \
         and see their current enabled status."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "The ID of the job to toggle"
                },
                "enabled": {
                    "type": "boolean",
                    "description": "Whether the job should be enabled (true) or disabled (false)"
                }
            },
            "required": ["id", "enabled"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput> {
        let id = input
            .get("id")
            .and_then(|i| i.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'id' parameter"))?;

        let enabled = input
            .get("enabled")
            .and_then(|e| e.as_bool())
            .ok_or_else(|| anyhow::anyhow!("Missing 'enabled' parameter (true/false)"))?;

        let store = store_from_ctx(ctx).await?;
        let mut job = match store.get(id).await? {
            Some(job) => job,
            None => {
                return Ok(ToolOutput::error(format!("Job '{}' not found", id)));
            }
        };

        job.enabled = enabled;
        job.updated_at = Utc::now();

        store.update(&job).await?;

        let status = if enabled { "enabled" } else { "disabled" };
        Ok(ToolOutput::success(format!(
            "Job '{}' is now {}",
            id, status
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Job creation must stamp the channel from the live turn context, not
    /// derive it from the directory layout: the agents-root channel's
    /// workspace is the agents root itself, so path-walking walked past the
    /// channel level and stamped the OS username — and the scheduler delivers
    /// fired messages via `topic_managers[job.channel_name]`, so a wrong name
    /// makes the job undeliverable.
    #[tokio::test]
    async fn test_job_create_stamps_channel_from_context() {
        let tmp = tempfile::tempdir().unwrap();
        let topic_dir = tmp.path().join("stamp-topic");
        tokio::fs::create_dir_all(&topic_dir).await.unwrap();
        // JobStore resolves the state dir through the registry; in production
        // the topic registers at activation, tests register explicitly.
        jyc_types::state_dir::register("stamp-topic", &topic_dir.join(".jyc"));

        let tool = JobCreateTool;
        let at = (Utc::now() + chrono::Duration::minutes(5)).to_rfc3339();
        let input = json!({
            "at": at,
            "prompt": "smoke",
        });

        // Without a channel in context: a loud error, never a silently
        // mis-stamped job.
        let mut ctx = ToolContext::new(&topic_dir);
        ctx.current_topic = Some("stamp-topic".to_string());
        let out = tool.execute(input.clone(), &ctx).await.unwrap();
        assert!(
            out.is_error && out.content.contains("channel"),
            "unexpected output: {}",
            out.content
        );

        // With a channel: the job is stamped with exactly that channel.
        ctx.current_channel = Some("agents".to_string());
        let out = tool.execute(input, &ctx).await.unwrap();
        assert!(
            !out.is_error && out.content.contains("Job created successfully"),
            "unexpected output: {}",
            out.content
        );

        let store = store_from_ctx(&ctx).await.unwrap();
        let jobs = store.list().await.unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].channel_name, "agents");
        assert_eq!(jobs[0].channel, "agents");
        assert_eq!(jobs[0].topic_name, "stamp-topic");
    }

    /// 'cron' and 'at' are mutually exclusive: both at once must be rejected
    /// instead of silently preferring cron.
    #[tokio::test]
    async fn test_job_create_rejects_cron_and_at_together() {
        let tmp = tempfile::tempdir().unwrap();
        let topic_dir = tmp.path().join("xor-topic");
        tokio::fs::create_dir_all(&topic_dir).await.unwrap();
        jyc_types::state_dir::register("xor-topic", &topic_dir.join(".jyc"));

        let tool = JobCreateTool;
        let at = (Utc::now() + chrono::Duration::minutes(5)).to_rfc3339();
        let input = json!({
            "cron": "0 0 8 * * * *",
            "at": at,
            "prompt": "smoke",
        });

        let mut ctx = ToolContext::new(&topic_dir);
        ctx.current_topic = Some("xor-topic".to_string());
        ctx.current_channel = Some("agents".to_string());
        let out = tool.execute(input, &ctx).await.unwrap();
        assert!(
            out.is_error && out.content.contains("not both"),
            "unexpected output: {}",
            out.content
        );

        let store = store_from_ctx(&ctx).await.unwrap();
        assert!(store.list().await.unwrap().is_empty());
    }
}
