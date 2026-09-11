//! /usage command — cross-topic usage/cost report.
//!
//! Aggregates every topic's billing ledger (`bill-*.jsonl`, written by
//! `BillingLogStore`) and renders one markdown report grouped by
//! provider → model → topic, so it works on every channel (the reply is
//! a normal command result, not TUI-only UI).

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;
use jyc_types::format_amount;

use super::handler::{CommandContext, CommandHandler, CommandResult};
use crate::billing_log_store::BillingLogStore;

pub struct UsageCommandHandler;

#[async_trait]
impl CommandHandler for UsageCommandHandler {
    fn name(&self) -> &str {
        "/usage"
    }

    fn description(&self) -> &str {
        "Usage/cost across topics (today | YYYY-MM | all)"
    }

    async fn execute(&self, context: CommandContext) -> Result<CommandResult> {
        let (prefix, label) = match parse_scope(&context.args) {
            Ok(scope) => scope,
            Err(message) => {
                return Ok(CommandResult {
                    success: false,
                    message,
                    ..Default::default()
                });
            }
        };
        let dirs = discover_state_dirs();
        Ok(CommandResult {
            success: true,
            message: render_report(&dirs, &prefix, &label),
            ..Default::default()
        })
    }
}

/// Parse the scope argument into a ledger-date prefix and a display label.
///
/// The prefix matches `bill-{prefix}*.jsonl` file names: `""` selects all
/// time, `"2026-09"` one month, `"2026-09-11"` one day.
fn parse_scope(args: &[String]) -> Result<(String, String), String> {
    match args.first().map(String::as_str) {
        None => {
            let today = Utc::now().format("%Y-%m-%d").to_string();
            Ok((today.clone(), format!("today ({today})")))
        }
        Some("all") => Ok((String::new(), "all time".to_string())),
        Some(month) if is_year_month(month) => Ok((month.to_string(), month.to_string())),
        Some(other) => Err(format!(
            "/usage: unknown scope '{other}' — use /usage, /usage YYYY-MM, or /usage all"
        )),
    }
}

fn is_year_month(s: &str) -> bool {
    let bytes = s.as_bytes();
    bytes.len() == 7
        && bytes[..4].iter().all(u8::is_ascii_digit)
        && bytes[4] == b'-'
        && bytes[5..].iter().all(u8::is_ascii_digit)
        && (1..=12).contains(&s[5..].parse::<u8>().unwrap_or(0))
}

/// Find every topic state dir that may hold billing ledgers.
///
/// Two sources, unioned (dedup by path):
/// - the state-dir registry: real topic names, covers pinned topics whose
///   `.jyc` lives outside `data_home`
/// - a recursive walk of `data_home`: covers ledgers of topics the current
///   process has not touched since startup (the registry only knows live
///   ones); labels derive from the path relative to `data_home`
///   (e.g. `agents/jyc`)
fn discover_state_dirs() -> Vec<(String, PathBuf)> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for (name, dir) in jyc_types::state_dir::registered_topics() {
        if seen.insert(dir.clone()) {
            out.push((name, dir));
        }
    }
    if let Some(home) = jyc_utils::paths::data_home() {
        walk_state_dirs(&home, &home, 0, &mut seen, &mut out);
    }
    out
}

fn walk_state_dirs(
    dir: &Path,
    home: &Path,
    depth: u8,
    seen: &mut HashSet<PathBuf>,
    out: &mut Vec<(String, PathBuf)>,
) {
    // State dirs sit at `data_home/<channel>/<topic>/.jyc`; six levels of
    // headroom cover nested custom layouts without risking a deep crawl.
    if depth > 6 {
        return;
    }
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in read_dir.map_while(Result::ok) {
        // file_type does not follow symlinks: no symlink-loop guard needed.
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let path = entry.path();
        if entry.file_name() == ".jyc" {
            if seen.insert(path.clone()) {
                let label = path
                    .parent()
                    .and_then(|p| p.strip_prefix(home).ok())
                    .map(|rel| rel.to_string_lossy().to_string())
                    .filter(|rel| !rel.is_empty())
                    .unwrap_or_else(|| path.display().to_string());
                out.push((label, path));
            }
        } else {
            walk_state_dirs(&path, home, depth + 1, seen, out);
        }
    }
}

/// Aggregated counters for one (provider, model, topic) bucket.
#[derive(Default)]
struct UsageRow {
    calls: u64,
    input: u64,
    output: u64,
    cache: u64,
    /// Cost per currency (providers may bill in different currencies).
    costs: BTreeMap<String, f64>,
}

impl UsageRow {
    fn add(&mut self, entry: &crate::billing_log_store::BillingEntry) {
        self.calls += 1;
        self.input += entry.input_tokens;
        self.output += entry.output_tokens;
        // One cache column keeps the table narrow; the ledger keeps the
        // read/creation split for auditing.
        self.cache += entry.cache_hit_tokens + entry.cache_creation_tokens;
        *self.costs.entry(entry.currency.clone()).or_default() += entry.cost;
    }
}

/// provider → model → topic → counters. BTreeMaps keep the report sorted.
type UsageTable = BTreeMap<String, BTreeMap<String, BTreeMap<String, UsageRow>>>;

fn aggregate(dirs: &[(String, PathBuf)], date_prefix: &str) -> UsageTable {
    let mut table: UsageTable = BTreeMap::new();
    for (label, dir) in dirs {
        for entry in BillingLogStore::load_matching(dir, date_prefix) {
            let (provider, model) = entry
                .model
                .split_once('/')
                .map(|(p, m)| (p.to_string(), m.to_string()))
                .unwrap_or_else(|| ("other".to_string(), entry.model.clone()));
            table
                .entry(provider)
                .or_default()
                .entry(model)
                .or_default()
                .entry(label.clone())
                .or_default()
                .add(&entry);
        }
    }
    table
}

fn format_costs(costs: &BTreeMap<String, f64>) -> String {
    costs
        .iter()
        .map(|(currency, amount)| format_amount(*amount, currency))
        .collect::<Vec<_>>()
        .join(" + ")
}

fn render_report(dirs: &[(String, PathBuf)], date_prefix: &str, label: &str) -> String {
    let table = aggregate(dirs, date_prefix);
    let mut out = format!("## Usage — {label}\n");
    if table.is_empty() {
        out.push_str("\nNo billing entries found.\n");
        return out;
    }
    let mut total = UsageRow::default();
    for (provider, models) in &table {
        let mut provider_total = UsageRow::default();
        out.push_str(&format!(
            "\n**{provider}**\n\
             | model | topic | calls | input | output | cache | cost |\n\
             |-------|-------|------:|------:|-------:|------:|-----:|\n"
        ));
        for (model, topics) in models {
            for (topic, row) in topics {
                out.push_str(&format!(
                    "| {model} | {topic} | {} | {} | {} | {} | {} |\n",
                    row.calls,
                    row.input,
                    row.output,
                    row.cache,
                    format_costs(&row.costs)
                ));
                provider_total.calls += row.calls;
                for (currency, amount) in &row.costs {
                    *provider_total.costs.entry(currency.clone()).or_default() += amount;
                }
            }
        }
        out.push_str(&format!(
            "\n**{provider} subtotal: {} calls, {}**\n",
            provider_total.calls,
            format_costs(&provider_total.costs)
        ));
        total.calls += provider_total.calls;
        for (currency, amount) in &provider_total.costs {
            *total.costs.entry(currency.clone()).or_default() += amount;
        }
    }
    out.push_str(&format!(
        "\n**Total: {} calls, {}**\n",
        total.calls,
        format_costs(&total.costs)
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::billing_log_store::BillingEntry;
    use tempfile::TempDir;

    fn entry(model: &str, cost: f64, currency: &str) -> BillingEntry {
        BillingEntry {
            ts: "2026-09-11T00:00:00Z".into(),
            model: model.into(),
            input_tokens: 100,
            output_tokens: 10,
            cache_hit_tokens: 50,
            cache_creation_tokens: 0,
            cost,
            currency: currency.into(),
            kind: "call".into(),
            input_rate_per_million: 0.0,
            output_rate_per_million: 0.0,
            cache_hit_rate_per_million: 0.0,
            time_window: None,
            utc_offset: String::new(),
        }
    }

    fn write_ledger(state_dir: &Path, date: &str, entries: &[BillingEntry]) {
        std::fs::create_dir_all(state_dir).unwrap();
        let body = entries
            .iter()
            .map(|e| serde_json::to_string(e).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(state_dir.join(format!("bill-{date}.jsonl")), body).unwrap();
    }

    #[test]
    fn scope_defaults_to_today() {
        let (prefix, label) = parse_scope(&[]).unwrap();
        assert_eq!(prefix, Utc::now().format("%Y-%m-%d").to_string());
        assert!(label.contains("today"));
    }

    #[test]
    fn scope_accepts_all_and_year_month() {
        assert_eq!(parse_scope(&["all".into()]).unwrap().0, "");
        assert_eq!(parse_scope(&["2026-09".into()]).unwrap().0, "2026-09");
        for bad in ["2026-13", "2026-9", "garbage", "2026-09-11"] {
            assert!(
                parse_scope(&[bad.into()]).is_err(),
                "{bad} must be rejected"
            );
        }
    }

    #[test]
    fn report_groups_by_provider_model_topic() {
        let tmp = TempDir::new().unwrap();
        let dir_a = tmp.path().join("agents/jyc/.jyc");
        let dir_b = tmp.path().join("agents/invoice/.jyc");
        write_ledger(
            &dir_a,
            "2026-09-10",
            &[
                entry("anthropic/claude-opus-4", 1.0, "USD"),
                entry("anthropic/claude-opus-4", 2.0, "USD"),
                entry("deepseek/deepseek-chat", 0.5, "CNY"),
            ],
        );
        write_ledger(
            &dir_b,
            "2026-09-10",
            &[entry("anthropic/claude-opus-4", 0.25, "USD")],
        );
        // Out-of-scope day must be excluded by the month/day prefix.
        write_ledger(
            &dir_a,
            "2026-08-31",
            &[entry("anthropic/claude-opus-4", 9.0, "USD")],
        );

        let dirs = vec![
            ("agents/jyc".to_string(), dir_a),
            ("agents/invoice".to_string(), dir_b),
        ];
        let report = render_report(&dirs, "2026-09", "2026-09");

        assert!(report.contains("## Usage — 2026-09"), "{report}");
        assert!(
            report.contains("| claude-opus-4 | agents/jyc | 2 | 200 | 20 | 100 | $3.0000 |"),
            "{report}"
        );
        assert!(
            report.contains("| claude-opus-4 | agents/invoice | 1 |"),
            "{report}"
        );
        assert!(
            report.contains("| deepseek-chat | agents/jyc | 1 |"),
            "{report}"
        );
        assert!(
            report.contains("**anthropic subtotal: 3 calls, $3.2500**"),
            "{report}"
        );
        assert!(
            report.contains("**deepseek subtotal: 1 calls, ¥0.5000**"),
            "{report}"
        );
        assert!(
            report.contains("**Total: 4 calls, $3.2500 + ¥0.5000**"),
            "{report}"
        );
        assert!(
            !report.contains("9.0000"),
            "August entry must be excluded: {report}"
        );
    }

    #[test]
    fn report_all_scope_includes_every_date() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("t/.jyc");
        write_ledger(&dir, "2026-01-05", &[entry("p/m", 1.0, "USD")]);
        write_ledger(&dir, "2026-09-10", &[entry("p/m", 2.0, "USD")]);
        let dirs = vec![("t".to_string(), dir)];
        assert!(render_report(&dirs, "", "all time").contains("$3.0000"));
        assert!(render_report(&dirs, "2026-09", "2026-09").contains("$2.0000"));
    }

    #[test]
    fn report_empty_when_no_entries() {
        let report = render_report(&[], "", "all time");
        assert!(report.contains("No billing entries found."), "{report}");
    }

    #[test]
    fn walk_labels_state_dirs_relative_to_home() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        std::fs::create_dir_all(home.join("agents/jyc/.jyc")).unwrap();
        std::fs::create_dir_all(home.join("agents/nested/topic/.jyc")).unwrap();
        std::fs::create_dir_all(home.join("agents/no-state")).unwrap();
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        walk_state_dirs(home, home, 0, &mut seen, &mut out);
        let mut labels: Vec<&str> = out.iter().map(|(l, _)| l.as_str()).collect();
        labels.sort_unstable(); // read_dir order is filesystem-dependent
        assert_eq!(labels, vec!["agents/jyc", "agents/nested/topic"]);
    }
}
