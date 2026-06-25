//! Job management tools for the agent — create, list, delete, and toggle
//! scheduled jobs from within any thread.
//!
//! Jobs are stored per-thread in `<thread>/.jyc/jobs/<id>.json`. Each tool
//! creates a scoped `JobStore` from the `ToolContext.working_dir` (which is
//! the thread's directory) at execution time.

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use jyc_core::job_store::JobStore;
use jyc_types::JobConfig;
use serde_json::{Value, json};

use super::super::{Tool, ToolContext, ToolOutput};

/// Default max jobs per thread for agent tools (matches config default).
const DEFAULT_MAX_JOBS: usize = 10;

/// Helper to create a per-thread JobStore from the working directory.
async fn store_from_ctx(ctx: &ToolContext<'_>) -> Result<JobStore> {
    JobStore::new(ctx.working_dir, DEFAULT_MAX_JOBS).await
}

/// List all scheduled jobs in the current thread.
pub struct JobListTool;

#[async_trait]
impl Tool for JobListTool {
    fn name(&self) -> &str {
        "job_list"
    }

    fn description(&self) -> &str {
        "List all scheduled jobs in this thread. Returns a JSON array of job \
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
                    "thread_name": j.thread_name,
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

/// Create a new scheduled job in the current thread.
pub struct JobCreateTool;

#[async_trait]
impl Tool for JobCreateTool {
    fn name(&self) -> &str {
        "job_create"
    }

    fn description(&self) -> &str {
        "Create a new scheduled job. Provide either a 'cron' expression for \
         recurring jobs (7-field format: 'sec min hour dom mon dow year', e.g. \
         '0 0 8 * * * *' for daily at 8 AM) or an 'at' timestamp for one-time \
         jobs (ISO 8601 format). The job fires by injecting the provided prompt \
         into the originating thread. Returns the created job ID."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "cron": {
                    "type": "string",
                    "description": "Cron expression for recurring jobs (7-field format: 'sec min hour dom mon dow year'). Exactly one of cron or at must be provided."
                },
                "at": {
                    "type": "string",
                    "description": "ISO 8601 timestamp for one-time jobs (e.g. '2026-06-22T08:00:00Z'). Exactly one of cron or at must be provided."
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

        // Extract thread/channel info from working directory path.
        // Directory structure: <workdir>/<channel_name>/workspace/<thread_name>/
        let thread_name = ctx
            .working_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string();

        let channel_name = ctx
            .working_dir
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string();

        // The channel type (e.g. "email", "github") is not directly derivable
        // from the directory path — only the channel config name is. Use the
        // channel_name as the channel value; the channel_name is the key field
        // for ThreadManager lookup when the job fires.
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
            JobConfig::new_recurring(cron_expr, thread_name, channel, channel_name, prompt)
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
            JobConfig::new_one_time(at, thread_name, channel, channel_name, prompt)
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

/// Delete a scheduled job by ID from the current thread.
pub struct JobDeleteTool;

#[async_trait]
impl Tool for JobDeleteTool {
    fn name(&self) -> &str {
        "job_delete"
    }

    fn description(&self) -> &str {
        "Delete a scheduled job by ID from this thread. The job will no longer \
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
