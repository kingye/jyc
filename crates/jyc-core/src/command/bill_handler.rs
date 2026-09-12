//! /bill command — cross-topic usage/cost report.
//!
//! Aggregates the central billing ledger (`<data_home>/billing/
//! bill-*.jsonl`, written by `BillingLogStore`) and renders one
//! markdown report grouped by provider → model → topic, so it works on
//! every channel (the reply is a normal command result, not TUI-only
//! UI). The ledger survives topic deletion, so closed topics still
//! contribute their historical cost.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Result;
use async_trait::async_trait;
use chrono::{NaiveDate, Utc};
use jyc_types::config::{BillingMode, DEFAULT_CURRENCY};
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
        let billing_dir = BillingLogStore::billing_dir();
        Ok(CommandResult {
            success: true,
            message: render_report(
                billing_dir.as_deref(),
                &scope,
                &collect_subscription_fees(&context.config),
            ),
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

/// Provider name → `(monthly_fee, currency)` for every configured
/// subscription provider. The fee is provider-level: a flat-fee coding
/// plan covers every model under the provider, so `/bill` compares one
/// fee against the provider's combined notional value. Fee currency comes
/// from the provider-level `pricing.currency` (default
/// [`DEFAULT_CURRENCY`]).
fn collect_subscription_fees(config: &AppConfig) -> BTreeMap<String, (f64, String)> {
    let mut fees = BTreeMap::new();
    for (provider_name, provider) in &config.ai.providers {
        if provider.billing == BillingMode::Subscription
            && let Some(fee) = provider.monthly_fee
        {
            fees.insert(
                provider_name.clone(),
                (
                    fee,
                    provider
                        .pricing
                        .as_ref()
                        .map(|p| p.currency_label().to_string())
                        .unwrap_or_else(|| DEFAULT_CURRENCY.to_string()),
                ),
            );
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

/// Aggregated counters for one (provider, model, topic) bucket.
#[derive(Default)]
struct BillRow {
    calls: u64,
    input: u64,
    output: u64,
    cache: u64,
    /// Cache-hit tokens only (a subset of `input`, which already
    /// includes them) — the numerator of the cache-utilization column.
    cache_hit: u64,
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
        self.cache_hit += entry.cache_hit_tokens;
        *self.costs.entry(entry.currency.clone()).or_default() += entry.cost;
    }

    /// Cache hits as a percentage of total input. 0 when no input was
    /// recorded; checked arithmetic guards the `*100` against wrapping.
    fn cache_util_pct(&self) -> u64 {
        self.cache_hit
            .checked_mul(100)
            .and_then(|v| v.checked_div(self.input))
            .unwrap_or(0)
    }
}

/// (billing mode, provider, model) → topic → counters. BTreeMaps keep
/// the report sorted; `"metered"` sorts before `"subscription"`, which
/// is the section order the report wants.
type BillTable = BTreeMap<(String, String, String), BTreeMap<String, BillRow>>;

/// Aggregate every matching ledger entry from the central ledger.
/// Also returns, per subscription provider, the day span of its
/// entries (for prorating the monthly fee in `All` scope).
fn aggregate(billing_dir: Option<&Path>, date_prefix: &str) -> (BillTable, BTreeMap<String, i64>) {
    let mut table: BillTable = BTreeMap::new();
    let mut sub_spans: BTreeMap<String, (NaiveDate, NaiveDate)> = BTreeMap::new();
    let entries = billing_dir
        .map(|d| BillingLogStore::load_matching(d, date_prefix))
        .unwrap_or_default();
    for entry in entries {
        // Entries without a topic label predate the central ledger and
        // were never migrated; they cannot be grouped into the report.
        if entry.topic.is_empty() {
            continue;
        }
        let (provider, model) = entry
            .model
            .split_once('/')
            .map(|(p, m)| (p.to_string(), m.to_string()))
            .unwrap_or_else(|| ("other".to_string(), entry.model.clone()));
        // `get(..10)` not `ts[..10]`: a corrupted ledger line whose
        // first bytes are multi-byte UTF-8 must not panic the handler.
        if entry.billing == BillingMode::Subscription.as_str()
            && let Ok(date) =
                NaiveDate::parse_from_str(entry.ts.get(..10).unwrap_or(&entry.ts), "%Y-%m-%d")
        {
            sub_spans
                .entry(provider.clone())
                .and_modify(|(min, max)| {
                    *min = (*min).min(date);
                    *max = (*max).max(date);
                })
                .or_insert((date, date));
        }
        table
            .entry((entry.billing.clone(), provider, model))
            .or_default()
            .entry(entry.topic.clone())
            .or_default()
            .add(&entry);
    }
    let spans = sub_spans
        .into_iter()
        .map(|(p, (min, max))| (p, (max - min).num_days() + 1))
        .collect();
    (table, spans)
}

fn format_costs(costs: &BTreeMap<String, f64>) -> String {
    costs
        .iter()
        .map(|(currency, amount)| format_amount(*amount, currency))
        .collect::<Vec<_>>()
        .join(" + ")
}

fn render_report(
    billing_dir: Option<&Path>,
    scope: &Scope,
    fees: &BTreeMap<String, (f64, String)>,
) -> String {
    let (table, sub_spans) = aggregate(billing_dir, &scope.prefix());
    let mut out = format!("## Bill — {}\n", scope.label());
    if table.is_empty() {
        out.push_str("\nNo billing entries found.\n");
        return out;
    }
    let has_subscription = table
        .keys()
        .any(|(billing, _, _)| billing == BillingMode::Subscription.as_str());

    for mode in [
        BillingMode::Metered.as_str(),
        BillingMode::Subscription.as_str(),
    ] {
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
                m if m == BillingMode::Metered.as_str() => "\n### Metered（真实支出）\n",
                _ => "\n### Subscription（影子成本）\n",
            });
        }
        let mut section_total = BillRow::default();
        // provider → notional total, for subscription fee lines.
        let mut provider_totals: BTreeMap<String, BillRow> = BTreeMap::new();
        for provider in providers {
            let mut provider_total = BillRow::default();
            out.push_str(&format!(
                "\n**{provider}**\n\
                 | model | topic | calls | input | output | cache | cache util | cost |\n\
                 |-------|-------|------:|------:|-------:|------:|-----------:|-----:|\n"
            ));
            for ((billing, row_provider, model), topics) in &table {
                if billing != mode || row_provider != provider {
                    continue;
                }
                for (topic, row) in topics {
                    out.push_str(&format!(
                        "| {model} | {topic} | {} | {} | {} | {} | {}% | {} |\n",
                        row.calls,
                        row.input,
                        row.output,
                        row.cache,
                        row.cache_util_pct(),
                        format_costs(&row.costs)
                    ));
                    provider_total.calls += row.calls;
                    let fee_total = provider_totals.entry(provider.clone()).or_default();
                    for (currency, amount) in &row.costs {
                        *provider_total.costs.entry(currency.clone()).or_default() += amount;
                        *fee_total.costs.entry(currency.clone()).or_default() += amount;
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
        if mode == BillingMode::Subscription.as_str() {
            for (provider, row) in &provider_totals {
                let Some((fee, currency)) = fees.get(provider) else {
                    continue;
                };
                let days = scope.days(sub_spans.get(provider).copied());
                let prorated = fee / 30.0 * days;
                let notional = row.costs.get(currency).copied().unwrap_or(0.0);
                let utilization = if prorated > 0.0 {
                    notional / prorated * 100.0
                } else {
                    0.0
                };
                out.push_str(&format!(
                    "\n{provider}: 月费 {} · 本期摊 {} · 利用率 {utilization:.0}%",
                    format_amount(*fee, currency),
                    format_amount(prorated, currency),
                ));
            }
            if !provider_totals.is_empty() {
                out.push('\n');
            }
        }
        if has_subscription {
            out.push_str(&format!(
                "\n**{}: {} calls, {}**\n",
                if mode == BillingMode::Metered.as_str() {
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

    fn entry(topic: &str, model: &str, cost: f64, currency: &str, billing: &str) -> BillingEntry {
        BillingEntry {
            ts: "2026-09-11T00:00:00Z".into(),
            topic: topic.into(),
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

    fn metered(topic: &str, model: &str, cost: f64, currency: &str) -> BillingEntry {
        entry(topic, model, cost, currency, "metered")
    }

    fn write_ledger(billing_dir: &Path, date: &str, entries: &[BillingEntry]) {
        std::fs::create_dir_all(billing_dir).unwrap();
        let body = entries
            .iter()
            .map(|e| serde_json::to_string(e).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(billing_dir.join(format!("bill-{date}.jsonl")), body).unwrap();
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
        let dir = tmp.path();
        write_ledger(
            dir,
            "2026-09-10",
            &[
                metered("agents/jyc", "anthropic/claude-opus-4", 1.0, "USD"),
                metered("agents/jyc", "anthropic/claude-opus-4", 2.0, "USD"),
                metered("agents/jyc", "deepseek/deepseek-chat", 0.5, "CNY"),
                metered("agents/invoice", "anthropic/claude-opus-4", 0.25, "USD"),
            ],
        );
        // Out-of-scope day must be excluded by the month/day prefix.
        write_ledger(
            dir,
            "2026-08-31",
            &[metered("agents/jyc", "anthropic/claude-opus-4", 9.0, "USD")],
        );

        let report = render_report(Some(dir), &Scope::Month(2026, 9), &BTreeMap::new());

        assert!(report.contains("## Bill — 2026-09"), "{report}");
        assert!(
            report.contains("| claude-opus-4 | agents/jyc | 2 | 200 | 20 | 100 | 50% | $3.0000 |"),
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
        let dir = tmp.path();
        write_ledger(dir, "2026-01-05", &[metered("t", "p/m", 1.0, "USD")]);
        write_ledger(dir, "2026-09-10", &[metered("t", "p/m", 2.0, "USD")]);
        assert!(render_report(Some(dir), &Scope::All, &BTreeMap::new()).contains("$3.0000"));
        assert!(
            render_report(Some(dir), &Scope::Month(2026, 9), &BTreeMap::new()).contains("$2.0000")
        );
    }

    #[test]
    fn report_empty_when_no_entries() {
        let report = render_report(None, &Scope::All, &BTreeMap::new());
        assert!(report.contains("No billing entries found."), "{report}");
    }

    /// Ledger lines written before the central ledger carry no topic
    /// label; unmigrated strays must be skipped, not panic the report.
    #[test]
    fn unlabelled_entries_are_skipped() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut unlabelled = metered("t", "p/m", 5.0, "USD");
        unlabelled.topic.clear();
        write_ledger(
            dir,
            "2026-09-10",
            &[unlabelled, metered("t", "p/m", 1.0, "USD")],
        );
        let report = render_report(Some(dir), &Scope::All, &BTreeMap::new());
        assert!(report.contains("**Total: 1 calls, $1.0000**"), "{report}");
    }

    #[test]
    fn subscription_entries_split_into_own_section_with_utilization() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        write_ledger(
            dir,
            "2026-09-10",
            &[
                metered("agents/jyc", "deepseek/deepseek-chat", 0.5, "CNY"),
                entry("agents/jyc", "glm/glm-4.6", 30.0, "CNY", "subscription"),
            ],
        );
        let fees = BTreeMap::from([("glm".to_string(), (49.0, "CNY".to_string()))]);
        // Month scope: September has 30 days, so the fee is not prorated.
        let report = render_report(Some(dir), &Scope::Month(2026, 9), &fees);

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
            report.contains("glm: 月费 ¥49.0000 · 本期摊 ¥49.0000 · 利用率 61%"),
            "{report}"
        );
        // With sections, there is no combined grand total mixing real
        // spend and notional value.
        assert!(!report.contains("**Total:"), "{report}");
    }

    #[test]
    fn subscription_fee_line_prorates_for_today_scope() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let today = Utc::now().format("%Y-%m-%d").to_string();
        write_ledger(
            dir,
            &today,
            &[entry("t", "glm/glm-4.6", 4.9, "CNY", "subscription")],
        );
        let fees = BTreeMap::from([("glm".to_string(), (49.0, "CNY".to_string()))]);
        let report = render_report(Some(dir), &Scope::Today, &fees);
        // 4.9 / (49/30 × 1) = 300%.
        assert!(
            report.contains("glm: 月费 ¥49.0000 · 本期摊 ¥1.6333 · 利用率 300%"),
            "{report}"
        );
    }

    /// A flat-fee plan covers every model under the provider: two models
    /// of the same subscription provider aggregate into ONE fee line
    /// whose notional is their combined cost.
    #[test]
    fn subscription_fee_aggregates_all_models_under_provider() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        write_ledger(
            dir,
            "2026-09-10",
            &[
                entry("t", "kimi/k2", 20.0, "CNY", "subscription"),
                entry("t", "kimi/k3", 10.0, "CNY", "subscription"),
            ],
        );
        let fees = BTreeMap::from([("kimi".to_string(), (30.0, "CNY".to_string()))]);
        let report = render_report(Some(dir), &Scope::Month(2026, 9), &fees);
        // (20 + 10) / 30 = 100% utilization on one shared fee line.
        assert!(
            report.contains("kimi: 月费 ¥30.0000 · 本期摊 ¥30.0000 · 利用率 100%"),
            "{report}"
        );
        assert!(!report.contains("kimi/k2: 月费"), "{report}");
        assert!(!report.contains("kimi/k3: 月费"), "{report}");
    }

    /// Fees come from provider-level `billing`/`monthly_fee`, with the
    /// currency taken from the provider-level pricing block.
    #[test]
    fn subscription_fees_collected_per_provider() {
        let cfg: AppConfig = toml::from_str(
            r#"
            [agent]
            [agent.providers.kimi]
            type = "anthropic"
            billing = "subscription"
            monthly_fee = 25.0
            pricing = { input_per_million = 2.0, output_per_million = 8.0, currency = "USD" }
            [agent.providers.kimi.models.k2]
            [agent.providers.kimi.models.k3]
            [agent.providers.deepseek]
            type = "openai-compatible"
            [agent.providers.kimi2]
            type = "anthropic"
            billing = "subscription"
        "#,
        )
        .unwrap();
        let fees = collect_subscription_fees(&cfg);
        assert_eq!(fees.len(), 1);
        assert_eq!(fees.get("kimi"), Some(&(25.0, "USD".to_string())));
    }

    #[test]
    fn subscription_without_monthly_fee_omits_fee_line() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        write_ledger(
            dir,
            "2026-09-10",
            &[entry("t", "glm/glm-4.6", 1.0, "CNY", "subscription")],
        );
        let report = render_report(Some(dir), &Scope::Month(2026, 9), &BTreeMap::new());
        assert!(report.contains("### Subscription（影子成本）"), "{report}");
        assert!(!report.contains("月费"), "{report}");
    }

    #[test]
    fn metered_only_report_keeps_flat_layout() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        write_ledger(dir, "2026-09-10", &[metered("t", "p/m", 1.0, "USD")]);
        let report = render_report(Some(dir), &Scope::Month(2026, 9), &BTreeMap::new());
        assert!(!report.contains("### Metered"), "{report}");
        assert!(report.contains("**Total: 1 calls, $1.0000**"), "{report}");
    }

    /// The cache-util column divides by total input; a zero-input row
    /// (output-only call) must render 0%, not panic.
    #[test]
    fn cache_util_handles_zero_input() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut zero_input = metered("t", "p/m", 1.0, "USD");
        zero_input.input_tokens = 0;
        zero_input.cache_hit_tokens = 0;
        zero_input.output_tokens = 10;
        write_ledger(dir, "2026-09-10", &[zero_input]);
        let report = render_report(Some(dir), &Scope::All, &BTreeMap::new());
        assert!(
            report.contains("| m | t | 1 | 0 | 10 | 0 | 0% |"),
            "{report}"
        );
    }
}
