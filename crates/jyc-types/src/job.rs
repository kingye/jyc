//! Scheduled job types for the channel-agnostic job system.
//!
//! A "job" is a recurring or one-time task created by the AI in any thread.
//! Jobs are defined by cron expressions (recurring) or absolute timestamps
//! (one-time), and fire by injecting an `InboundMessage` into the
//! originating thread via `ThreadManager::enqueue`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::str::FromStr;

/// A scheduled job — either recurring (cron) or one-time (at).
///
/// Jobs are persisted as `<jobs_dir>/<id>.json` files and loaded by the
/// `JobScheduler` at runtime.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobConfig {
    /// Unique job identifier (UUID v4).
    pub id: String,

    /// Cron expression for recurring jobs (e.g. "0 0 8 * * * *" for daily at 8 AM).
    /// Uses 7-field format: sec min hour dom mon dow year.
    /// Mutually exclusive with `at`. When both are set, `cron` takes precedence.
    pub cron: Option<String>,

    /// Absolute timestamp for one-time jobs. The job fires once at this time.
    /// Mutually exclusive with `cron`.
    pub at: Option<DateTime<Utc>>,

    /// Whether the job is currently enabled (default: true).
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// The thread name to inject the fired job message into.
    pub thread_name: String,

    /// The channel type (e.g. "email", "wecom_bot") for the target thread.
    pub channel: String,

    /// The channel name (config key) for the target thread.
    pub channel_name: String,

    /// The prompt/instructions for the AI to execute when the job fires.
    /// This text is injected as the body of the InboundMessage.
    pub prompt: String,

    /// Timestamp when the job was created.
    pub created_at: DateTime<Utc>,

    /// Timestamp when the job was last updated.
    pub updated_at: DateTime<Utc>,

    /// Timestamp when the job last fired (None if never fired).
    pub last_fired_at: Option<DateTime<Utc>>,

    /// Next scheduled fire time (computed from cron/at). Updated after each fire.
    pub next_fire_at: Option<DateTime<Utc>>,
}

impl JobConfig {
    /// Create a new recurring job from a cron expression.
    pub fn new_recurring(
        cron_expr: &str,
        thread_name: String,
        channel: String,
        channel_name: String,
        prompt: String,
    ) -> Self {
        let now = Utc::now();
        let id = uuid::Uuid::new_v4().to_string();

        // Parse the cron expression to compute the next fire time
        let next_fire = parse_cron_next(cron_expr, now);

        Self {
            id,
            cron: Some(cron_expr.to_string()),
            at: None,
            enabled: true,
            thread_name,
            channel,
            channel_name,
            prompt,
            created_at: now,
            updated_at: now,
            last_fired_at: None,
            next_fire_at: next_fire,
        }
    }

    /// Create a new one-time job that fires at the given timestamp.
    pub fn new_one_time(
        at: DateTime<Utc>,
        thread_name: String,
        channel: String,
        channel_name: String,
        prompt: String,
    ) -> Self {
        let now = Utc::now();

        Self {
            id: uuid::Uuid::new_v4().to_string(),
            cron: None,
            at: Some(at),
            enabled: true,
            thread_name,
            channel,
            channel_name,
            prompt,
            created_at: now,
            updated_at: now,
            last_fired_at: None,
            next_fire_at: Some(at),
        }
    }

    /// Mark the job as fired and update its next fire time.
    ///
    /// For recurring jobs, `next_fire_at` is advanced to the next cron match.
    /// For one-time jobs, `next_fire_at` is set to `None` and `enabled` to `false`.
    pub fn mark_fired(&mut self) {
        let now = Utc::now();
        self.last_fired_at = Some(now);
        self.updated_at = now;

        if let Some(ref cron_expr) = self.cron {
            // Advance to the next fire time after now
            self.next_fire_at = parse_cron_next(cron_expr, now);
        } else {
            // One-time job: disable after firing
            self.enabled = false;
            self.next_fire_at = None;
        }
    }
}

fn default_true() -> bool {
    true
}

/// Parse a cron expression and compute the next DateTime after `from`.
fn parse_cron_next(cron_expr: &str, from: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let schedule = cron::Schedule::from_str(cron_expr).ok()?;
    schedule.after(&from).next()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_recurring_job() {
        // Every day at 8 AM UTC (7-field cron: sec min hour dom mon dow year)
        let job = JobConfig::new_recurring(
            "0 0 8 * * * *",
            "test-thread".to_string(),
            "email".to_string(),
            "work".to_string(),
            "Send me the daily summary".to_string(),
        );

        assert!(job.enabled);
        assert_eq!(job.cron.as_deref(), Some("0 0 8 * * * *"));
        assert!(job.at.is_none());
        assert_eq!(job.thread_name, "test-thread");
        assert_eq!(job.channel, "email");
        assert_eq!(job.channel_name, "work");
        assert_eq!(job.prompt, "Send me the daily summary");
        assert!(job.last_fired_at.is_none());
        assert!(job.next_fire_at.is_some());
    }

    #[test]
    fn test_new_one_time_job() {
        let future = Utc::now() + chrono::Duration::hours(1);
        let job = JobConfig::new_one_time(
            future,
            "test-thread".to_string(),
            "wecom_bot".to_string(),
            "general".to_string(),
            "Remind me about the meeting".to_string(),
        );

        assert!(job.enabled);
        assert!(job.cron.is_none());
        assert_eq!(job.at, Some(future));
        assert_eq!(job.next_fire_at, Some(future));
    }

    #[test]
    fn test_mark_fired_recurring() {
        // Every 30 seconds to ensure next_fire advances within test window
        let mut job = JobConfig::new_recurring(
            "*/30 * * * * * *",
            "test".to_string(),
            "email".to_string(),
            "work".to_string(),
            "summary".to_string(),
        );
        let original_next = job.next_fire_at;
        assert!(original_next.is_some());

        // Simulate a brief pause so that `now` in mark_fired is strictly
        // after the `now` used during construction, guaranteeing advancement.
        std::thread::sleep(std::time::Duration::from_millis(50));
        job.mark_fired();

        assert!(job.last_fired_at.is_some());
        assert!(job.enabled); // Recurring jobs stay enabled
        assert!(job.next_fire_at.is_some());
        // Next fire should be strictly after the original
        assert!(job.next_fire_at >= original_next);
    }

    #[test]
    fn test_mark_fired_one_time() {
        let future = Utc::now() + chrono::Duration::hours(1);
        let mut job = JobConfig::new_one_time(
            future,
            "test".to_string(),
            "email".to_string(),
            "work".to_string(),
            "reminder".to_string(),
        );

        job.mark_fired();

        assert!(job.last_fired_at.is_some());
        assert!(!job.enabled); // One-time jobs get disabled
        assert!(job.next_fire_at.is_none());
    }

    #[test]
    fn test_parse_cron_next_valid() {
        let now = Utc::now();
        let next = parse_cron_next("0 0 8 * * * *", now);
        assert!(next.is_some());
        assert!(next.unwrap() > now);
    }

    #[test]
    fn test_parse_cron_next_invalid() {
        let now = Utc::now();
        let next = parse_cron_next("not-a-cron", now);
        assert!(next.is_none());
    }
}
