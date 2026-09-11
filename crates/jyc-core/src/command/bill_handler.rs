//! /bill command — cross-topic usage/cost report.
//!
//! Aggregates every topic's billing ledger (`bill-*.jsonl`, written by
//! `BillingLogStore`) and renders one markdown report grouped by
//! provider → model → topic, so it works on every channel (the reply is
//! a normal command result, not TUI-only UI).

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::Result;
use async_trait::async_trait;
use chrono::{NaiveDate, Utc};
use jyc_types::config::BillingMode;
use jyc_types::{AppConfig, format_amount};

use super::handler::{CommandContext, CommandHandler, CommandResult};
use crate::billing_log_store::BillingLogStore;

pub struct BillCommandHandler;

#[async_trait]
impl CommandHandler for BillCommandHandler {
    fn name(&self) -> &str {
        "/bill"
    }

    fn description(&self) -> &str {
        "Usage/cost across topics (today | YYYY-MM | all)"
    }

    async fn execute(&self, context: CommandContext) -> Result<CommandResult> {
        let scope = match parse_scope(&context.args) {
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
            message: render_report(&dirs, &scope, &collect_subscription_fees(&context.config)),
            ..Default::default()
        })
    }
}

/// Report scope, parsed from the `/bill` argument.
enum Scope {
    /// Today (UTC — the ledger's stamping clock).
    Today,
    /// One month: `(year, month)`.
    Month(i32, u32),
    /// Everything on disk.
    All,
}

impl Scope {
    /// Ledger filename matcher: `""` selects all time, `"2026-09"` one
    /// month, `"2026-09-11"` one day.
    fn prefix(&self) -> String {
        match self {
            Scope::Today => Utc::now().format("%Y-%m-%d").to_string(),
            Scope::Month(year, month) => format!("{year:04}-{month:02}"),
            Scope::All => String::new(),
        }
    }

    /// Header label.
    fn label(&self) -> String {
        match self {
            Scope::Today => format!("today ({})", Utc::now().format("%Y-%m-%d")),
            Scope::Month(year, month) => format!("{year:04}-{month:02}"),
            Scope::All => "all time".to_string(),
        }
    }

    /// Days the monthly fee is prorated over, so utilization means the
    /// same thing for every scope: notional value vs fee paid *for this
    /// period*. `All` uses the observed span of subscription entries.
    fn days(&self, span_days: Option<i64>) -> f64 {
        match self {
            Scope::Today => 1.0,
            Scope::Month(year, month) => days_in_month(*year, *month) as f64,
            Scope::All => span_days.unwrap_or(1).max(1) as f64,
        }
    }
}

fn days_in_month(year: i32, month: u32) -> u32 {
    let (next_year, next_month) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    let first = NaiveDate::from_ymd_opt(year, month, 1);
    let next = NaiveDate::from_ymd_opt(next_year, next_month, 1);
    match (first, next) {
        (Some(first), Some(next)) => (next - first).num_days() as u32,
        _ => 30,
    }
}

/// Parse the scope argument.
fn parse_scope(args: &[String]) -> Result<Scope, String> {
    match args.first().map(String::as_str) {
        None => Ok(Scope::Today),
        Some("all") => Ok(Scope::All),
        Some(month) if is_year_month(month) => Ok(Scope::Month(
            month[..4].parse().unwrap_or(0),
            month[5..].parse().unwrap_or(1),
        )),
        Some(other) => Err(format!(
            "/bill: unknown scope '{other}' — use /bill, /bill YYYY-MM, or /bill all"
        )),
    }
}

/// `model_label` → `(monthly_fee, currency)` for every configured
/// subscription model. Aliases whose `model_id` differs from the config
/// key never reach the ledger (pricing lookup would already have failed
/// at billing time), so scanning config keys matches ledger labels
/// exactly.
fn collect_subscription_fees(config: &AppConfig) -> BTreeMap<String, (f64, String)> {
    let mut fees = BTreeMap::new();
    for (provider_name, provider) in &config.ai.providers {
        for (model_key, model) in &provider.models {
            if let Some(pricing) = &model.pricing
                && pricing.billing == BillingMode::Subscription
                && let Some(fee) = pricing.monthly_fee
            {
                fees.insert(
                    format!("{provider_name}/{model_key}"),
                    (fee, pricing.currency_label().to_string()),
                );
            }
        }
    }
    fees
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
struct BillRow {
    calls: u64,
    input: u64,
    output: u64,
    cache: u64,
    /// Cost per currency (providers may bill in different currencies).
    costs: BTreeMap<String, f64>,
}

impl BillRow {
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

/// (billing mode, provider, model) → topic → counters. BTreeMaps keep
/// the report sorted; `"metered"` sorts before `"subscription"`, which
/// is the section order the report wants.
type BillTable = BTreeMap<(String, String, String), BTreeMap<String, BillRow>>;

/// Aggregate every matching ledger entry. Also returns the day span of
/// subscription entries (for prorating the monthly fee in `All` scope).
fn aggregate(dirs: &[(String, PathBuf)], date_prefix: &str) -> (BillTable, Option<i64>) {
    let mut table: BillTable = BTreeMap::new();
    let mut sub_min: Option<NaiveDate> = None;
    let mut sub_max: Option<NaiveDate> = None;
    for (label, dir) in dirs {
        for entry in BillingLogStore::load_matching(dir, date_prefix) {
            let (provider, model) = entry
                .model
                .split_once('/')
                .map(|(p, m)| (p.to_string(), m.to_string()))
                .unwrap_or_else(|| ("other".to_string(), entry.model.clone()));
            table
                .entry((entry.billing.clone(), provider, model))
                .or_default()
                .entry(label.clone())
                .or_default()
                .add(&entry);
            if entry.billing == "subscription"
                && let Ok(date) =
                    NaiveDate::parse_from_str(&entry.ts[..10.min(entry.ts.len())], "%Y-%m-%d")
            {
                sub_min = Some(sub_min.map_or(date, |m: NaiveDate| m.min(date)));
                sub_max = Some(sub_max.map_or(date, |m: NaiveDate| m.max(date)));
            }
        }
    }
    let span = match (sub_min, sub_max) {
        (Some(min), Some(max)) => Some((max - min).num_days() + 1),
        _ => None,
    };
    (table, span)
}

fn format_costs(costs: &BTreeMap<String, f64>) -> String {
    costs
        .iter()
        .map(|(currency, amount)| format_amount(*amount, currency))
        .collect::<Vec<_>>()
        .join(" + ")
}

fn render_report(
    dirs: &[(String, PathBuf)],
    scope: &Scope,
    fees: &BTreeMap<String, (f64, String)>,
) -> String {
    let (table, sub_span) = aggregate(dirs, &scope.prefix());
    let mut out = format!("## Bill — {}\n", scope.label());
    if table.is_empty() {
        out.push_str("\nNo billing entries found.\n");
        return out;
    }
    let has_subscription = table
        .keys()
        .any(|(billing, _, _)| billing == "subscription");
    let days = scope.days(sub_span);

    for mode in ["metered", "subscription"] {
        let providers: std::collections::BTreeSet<&String> = table
            .keys()
            .filter(|(billing, _, _)| billing == mode)
            .map(|(_, provider, _)| provider)
            .collect();
        if providers.is_empty() {
            continue;
        }
        if has_subscription {
            out.push_str(match mode {
                "metered" => "\n### Metered（真实支出）\n",
                _ => "\n### Subscription（影子成本）\n",
            });
        }
        let mut section_total = BillRow::default();
        // (provider, model) → notional total, for subscription fee lines.
        let mut model_totals: BTreeMap<(String, String), BillRow> = BTreeMap::new();
        for provider in providers {
            let mut provider_total = BillRow::default();
            out.push_str(&format!(
                "\n**{provider}**\n\
                 | model | topic | calls | input | output | cache | cost |\n\
                 |-------|-------|------:|------:|-------:|------:|-----:|\n"
            ));
            for ((billing, row_provider, model), topics) in &table {
                if billing != mode || row_provider != provider {
                    continue;
                }
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
                    let model_total = model_totals
                        .entry((provider.clone(), model.clone()))
                        .or_default();
                    for (currency, amount) in &row.costs {
                        *provider_total.costs.entry(currency.clone()).or_default() += amount;
                        *model_total.costs.entry(currency.clone()).or_default() += amount;
                    }
                }
            }
            out.push_str(&format!(
                "\n**{provider} subtotal: {} calls, {}**\n",
                provider_total.calls,
                format_costs(&provider_total.costs)
            ));
            section_total.calls += provider_total.calls;
            for (currency, amount) in &provider_total.costs {
                *section_total.costs.entry(currency.clone()).or_default() += amount;
            }
        }
        if mode == "subscription" {
            for ((provider, model), row) in &model_totals {
                let model_label = format!("{provider}/{model}");
                let Some((fee, currency)) = fees.get(&model_label) else {
                    continue;
                };
                let prorated = fee / 30.0 * days;
                let notional = row.costs.get(currency).copied().unwrap_or(0.0);
                let utilization = if prorated > 0.0 {
                    notional / prorated * 100.0
                } else {
                    0.0
                };
                out.push_str(&format!(
                    "\n{model_label}: 月费 {} · 本期摊 {} · 利用率 {utilization:.0}%",
                    format_amount(*fee, currency),
                    format_amount(prorated, currency),
                ));
            }
            if !model_totals.is_empty() {
                out.push('\n');
            }
        }
        if has_subscription {
            out.push_str(&format!(
                "\n**{}: {} calls, {}**\n",
                if mode == "metered" {
                    "Metered total"
                } else {
                    "Subscription notional total"
                },
                section_total.calls,
                format_costs(&section_total.costs)
            ));
        } else {
            out.push_str(&format!(
                "\n**Total: {} calls, {}**\n",
                section_total.calls,
                format_costs(&section_total.costs)
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::billing_log_store::BillingEntry;
    use tempfile::TempDir;

    fn entry(model: &str, cost: f64, currency: &str, billing: &str) -> BillingEntry {
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
            billing: billing.into(),
            input_rate_per_million: 0.0,
            output_rate_per_million: 0.0,
            cache_hit_rate_per_million: 0.0,
            time_window: None,
            utc_offset: String::new(),
        }
    }

    fn metered(model: &str, cost: f64, currency: &str) -> BillingEntry {
        entry(model, cost, currency, "metered")
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
        let scope = parse_scope(&[]).unwrap();
        assert!(matches!(scope, Scope::Today));
        assert_eq!(scope.prefix(), Utc::now().format("%Y-%m-%d").to_string());
        assert!(scope.label().contains("today"));
    }

    #[test]
    fn scope_accepts_all_and_year_month() {
        let scope = parse_scope(&["all".into()]).unwrap();
        assert!(matches!(scope, Scope::All));
        assert_eq!(scope.prefix(), "");
        let scope = parse_scope(&["2026-09".into()]).unwrap();
        assert!(matches!(scope, Scope::Month(2026, 9)));
        assert_eq!(scope.prefix(), "2026-09");
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
                metered("anthropic/claude-opus-4", 1.0, "USD"),
                metered("anthropic/claude-opus-4", 2.0, "USD"),
                metered("deepseek/deepseek-chat", 0.5, "CNY"),
            ],
        );
        write_ledger(
            &dir_b,
            "2026-09-10",
            &[metered("anthropic/claude-opus-4", 0.25, "USD")],
        );
        // Out-of-scope day must be excluded by the month/day prefix.
        write_ledger(
            &dir_a,
            "2026-08-31",
            &[metered("anthropic/claude-opus-4", 9.0, "USD")],
        );

        let dirs = vec![
            ("agents/jyc".to_string(), dir_a),
            ("agents/invoice".to_string(), dir_b),
        ];
        let report = render_report(&dirs, &Scope::Month(2026, 9), &BTreeMap::new());

        assert!(report.contains("## Bill — 2026-09"), "{report}");
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
        // Currencies iterate in BTreeMap (alphabetical) order: CNY < USD.
        assert!(
            report.contains("**Total: 4 calls, ¥0.5000 + $3.2500**"),
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
        write_ledger(&dir, "2026-01-05", &[metered("p/m", 1.0, "USD")]);
        write_ledger(&dir, "2026-09-10", &[metered("p/m", 2.0, "USD")]);
        let dirs = vec![("t".to_string(), dir)];
        assert!(render_report(&dirs, &Scope::All, &BTreeMap::new()).contains("$3.0000"));
        assert!(render_report(&dirs, &Scope::Month(2026, 9), &BTreeMap::new()).contains("$2.0000"));
    }

    #[test]
    fn report_empty_when_no_entries() {
        let report = render_report(&[], &Scope::All, &BTreeMap::new());
        assert!(report.contains("No billing entries found."), "{report}");
    }

    #[test]
    fn subscription_entries_split_into_own_section_with_utilization() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("agents/jyc/.jyc");
        write_ledger(
            &dir,
            "2026-09-10",
            &[
                metered("deepseek/deepseek-chat", 0.5, "CNY"),
                entry("glm/glm-4.6", 30.0, "CNY", "subscription"),
            ],
        );
        let dirs = vec![("agents/jyc".to_string(), dir)];
        let fees = BTreeMap::from([("glm/glm-4.6".to_string(), (49.0, "CNY".to_string()))]);
        // Month scope: September has 30 days, so the fee is not prorated.
        let report = render_report(&dirs, &Scope::Month(2026, 9), &fees);

        assert!(report.contains("### Metered（真实支出）"), "{report}");
        assert!(report.contains("### Subscription（影子成本）"), "{report}");
        assert!(
            report.contains("| deepseek-chat | agents/jyc | 1 |"),
            "{report}"
        );
        assert!(report.contains("| glm-4.6 | agents/jyc | 1 |"), "{report}");
        assert!(
            report.contains("**Metered total: 1 calls, ¥0.5000**"),
            "{report}"
        );
        assert!(
            report.contains("**Subscription notional total: 1 calls, ¥30.0000**"),
            "{report}"
        );
        // 30 / 49 ≈ 61% utilization against the full-month fee.
        assert!(
            report.contains("glm/glm-4.6: 月费 ¥49.0000 · 本期摊 ¥49.0000 · 利用率 61%"),
            "{report}"
        );
        // With sections, there is no combined grand total mixing real
        // spend and notional value.
        assert!(!report.contains("**Total:"), "{report}");
    }

    #[test]
    fn subscription_fee_line_prorates_for_today_scope() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("t/.jyc");
        let today = Utc::now().format("%Y-%m-%d").to_string();
        write_ledger(
            &dir,
            &today,
            &[entry("glm/glm-4.6", 4.9, "CNY", "subscription")],
        );
        let dirs = vec![("t".to_string(), dir)];
        let fees = BTreeMap::from([("glm/glm-4.6".to_string(), (49.0, "CNY".to_string()))]);
        let report = render_report(&dirs, &Scope::Today, &fees);
        // 4.9 / (49/30 × 1) = 300%.
        assert!(
            report.contains("glm/glm-4.6: 月费 ¥49.0000 · 本期摊 ¥1.6333 · 利用率 300%"),
            "{report}"
        );
    }

    #[test]
    fn subscription_without_monthly_fee_omits_fee_line() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("t/.jyc");
        write_ledger(
            &dir,
            "2026-09-10",
            &[entry("glm/glm-4.6", 1.0, "CNY", "subscription")],
        );
        let dirs = vec![("t".to_string(), dir)];
        let report = render_report(&dirs, &Scope::Month(2026, 9), &BTreeMap::new());
        assert!(report.contains("### Subscription（影子成本）"), "{report}");
        assert!(!report.contains("月费"), "{report}");
    }

    #[test]
    fn metered_only_report_keeps_flat_layout() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("t/.jyc");
        write_ledger(&dir, "2026-09-10", &[metered("p/m", 1.0, "USD")]);
        let dirs = vec![("t".to_string(), dir)];
        let report = render_report(&dirs, &Scope::Month(2026, 9), &BTreeMap::new());
        assert!(!report.contains("### Metered"), "{report}");
        assert!(report.contains("**Total: 1 calls, $1.0000**"), "{report}");
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
