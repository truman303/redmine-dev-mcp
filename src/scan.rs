use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use serde_json::{Map, Value, json};
use tokio::sync::Mutex;
use tokio::task::JoinSet;

use crate::catalog::{Ctx, Field, Snapshot, display_key};
use crate::client::Client;
use crate::config::REDMINE_PAGE_MAX;
use crate::error::ToolError;
use crate::filters::{array_field, catalog_err, check_applied, get_json, u64_field};
use crate::render::{columnar_json, markdown_table};
use crate::tools::OutputFormat;

/// Issues per Redmine page. Also the most Redmine will return.
const PAGE_SIZE: u64 = REDMINE_PAGE_MAX;

/// Pages in flight after the first one. The client already retries a 429 on GETs.
const CONCURRENCY: usize = 4;

const PAGING: &[&str] = &["sort", "limit", "offset"];

const BUILTINS: &[&str] = &[
    "status", "tracker", "priority", "assignee", "author", "category", "version", "project",
];

/// How many values a spelling error will list before it says "and N more".
const VALUES_SHOWN: usize = 25;

/// A substring shorter than this never becomes a "did you mean".
const SUGGEST_MIN: usize = 2;

#[derive(Clone, Default)]
pub(crate) struct ScanCache {
    inner: Arc<Mutex<CacheInner>>,
}

#[derive(Default)]
struct CacheInner {
    entries: HashMap<String, (Instant, Arc<Scan>)>,
    /// One scan per key at a time. The mutex is tiny. The scan payload is what expires.
    flights: HashMap<String, Arc<Mutex<()>>>,
}

impl ScanCache {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    async fn get(&self, key: &str, ttl: std::time::Duration) -> Option<Arc<Scan>> {
        let inner = self.inner.lock().await;
        inner.entries.get(key).and_then(|(taken_at, scan)| {
            if taken_at.elapsed() < ttl {
                Some(Arc::clone(scan))
            } else {
                None
            }
        })
    }

    async fn insert(&self, key: String, scan: Arc<Scan>, ttl: std::time::Duration) {
        let taken_at = scan.taken_at;
        let mut inner = self.inner.lock().await;
        inner
            .entries
            .retain(|_, (taken_at, _)| taken_at.elapsed() < ttl);
        inner.entries.insert(key, (taken_at, scan));
    }

    #[cfg(test)]
    pub(crate) async fn flights_in_use(&self) -> usize {
        self.inner.lock().await.flights.len()
    }

    async fn end_flight(&self, key: &str) {
        self.inner.lock().await.flights.remove(key);
    }

    async fn flight(&self, key: &str) -> Arc<Mutex<()>> {
        let mut inner = self.inner.lock().await;
        inner
            .flights
            .entry(key.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }
}

/// Why `scan` stopped. Over the cap is separate so a value check can fall back
/// to the spelling hint instead of failing the call.
pub(crate) enum ScanError {
    OverCap { total: u64, cap: u64 },
    Tool(ToolError),
}

impl From<ToolError> for ScanError {
    fn from(err: ToolError) -> Self {
        Self::Tool(err)
    }
}

pub(crate) fn cap_message(total: u64, cap: u64) -> String {
    let verb = if total == 1 {
        "issue matches"
    } else {
        "issues match"
    };
    format!(
        "{} {verb}, over the scan cap of {}. Narrow the filters (a project, a status, a date range), or raise REDMINE_MAX_SCAN.",
        with_commas(total),
        with_commas(cap)
    )
}

/// One issue, reduced to the fields a facet can group on. No description, no journals.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ScanRow {
    id: u64,
    project: String,
    tracker: String,
    status: String,
    priority: String,
    assignee: String,
    author: String,
    category: String,
    version: String,
    /// Catalog key to the values Redmine sent. A multi-value field has one entry per value.
    fields: BTreeMap<String, Vec<String>>,
}

pub(crate) struct Scan {
    issues: Vec<ScanRow>,
    /// `total_count` from the first page of the pass we kept.
    total: u64,
    /// The `total_count` re-read after the last page, or `total` when the scan was one page.
    reported: u64,
    approximate: bool,
    params: Vec<(String, String)>,
    taken_at: Instant,
}

struct Pass {
    issues: Vec<ScanRow>,
    total: u64,
    reported: u64,
    drifted: bool,
}

/// Page every matching issue into this process.
///
/// The first page's `total_count` is checked against the scan cap before another
/// page is fetched, so a refusal is never a partial total. Pages after the first
/// are fetched a few at a time, in id order, and deduped. A multi-page scan then
/// re-reads `total_count`. Any mismatch rescans once. If the second pass still
/// disagrees, the result is marked approximate.
pub(crate) async fn scan(ctx: &Ctx, params: &[(String, String)]) -> Result<Arc<Scan>, ScanError> {
    let key = cache_key(params);
    let ttl = ctx.client.config().scan_ttl;
    if let Some(hit) = ctx.scans.get(&key, ttl).await {
        return Ok(hit);
    }
    let flight = ctx.scans.flight(&key).await;
    let _guard = flight.lock().await;
    if let Some(hit) = ctx.scans.get(&key, ttl).await {
        return Ok(hit);
    }
    let result = run(ctx, params).await.map(Arc::new);
    if let Ok(loaded) = &result {
        ctx.scans.insert(key.clone(), Arc::clone(loaded), ttl).await;
    }
    // Callers already waiting hold their own clone of the lock and find the
    // cached scan, or load again after a failure. Without this the map would
    // keep a lock for every filter combination ever scanned.
    ctx.scans.end_flight(&key).await;
    result
}

async fn run(ctx: &Ctx, params: &[(String, String)]) -> Result<Scan, ScanError> {
    let first = load_pass(ctx, params).await?;
    let pass = if first.drifted {
        let second = load_pass(ctx, params).await?;
        let approximate = second.drifted;
        finish(params, second, approximate)
    } else {
        finish(params, first, false)
    };
    tracing::debug!(
        issues = pass.issues.len(),
        first_total = pass.total,
        reported = pass.reported,
        filters = pass.params.len(),
        approximate = pass.approximate,
        "scanned issues"
    );
    Ok(pass)
}

fn finish(params: &[(String, String)], pass: Pass, approximate: bool) -> Scan {
    Scan {
        issues: pass.issues,
        total: pass.total,
        reported: pass.reported,
        approximate,
        params: params
            .iter()
            .filter(|(key, _)| !PAGING.contains(&key.as_str()))
            .cloned()
            .collect(),
        taken_at: Instant::now(),
    }
}

async fn load_pass(ctx: &Ctx, params: &[(String, String)]) -> Result<Pass, ScanError> {
    let snapshot = ctx.catalog.get().await.map_err(catalog_err)?;
    let body = fetch_page(&ctx.client, params, PAGE_SIZE, 0).await?;
    let total = total_count(&body)?;
    let cap = ctx.client.config().max_scan;
    if total > cap {
        return Err(ScanError::OverCap { total, cap });
    }
    let fields = filtered_fields(&snapshot, params);
    check_applied(ctx, params, &fields, total).await?;

    let assigned = snapshot
        .fields
        .iter()
        .map(|field| (field.id, field.key.clone()))
        .collect::<BTreeMap<_, _>>();
    let mut rows = rows_from(&body, &assigned, ctx.client.config())?;
    let mut offset = PAGE_SIZE;
    let mut offsets = Vec::new();
    while offset < total {
        offsets.push(offset);
        offset += PAGE_SIZE;
    }
    if !offsets.is_empty() {
        let pages = fetch_offsets(&ctx.client, params, &offsets).await?;
        for page in &pages {
            rows.extend(rows_from(page, &assigned, ctx.client.config())?);
        }
    }
    let issues = dedupe(rows);
    tracing::debug!(pages = offsets.len() + 1, total, "scan pass");
    let distinct = u64::try_from(issues.len()).unwrap_or(u64::MAX);
    // One response cannot drift against itself. The re-read is for the gap
    // between the first page and the last.
    let reported = if offsets.is_empty() {
        total
    } else {
        let again = fetch_page(&ctx.client, params, 1, 0).await?;
        total_count(&again)?
    };
    let drifted = distinct != total || distinct != reported;
    Ok(Pass {
        issues,
        total,
        reported,
        drifted,
    })
}

fn filtered_fields(snapshot: &Snapshot, params: &[(String, String)]) -> Vec<Field> {
    params
        .iter()
        .filter_map(|(key, _)| {
            let id = key.strip_prefix("cf_")?.parse::<u64>().ok()?;
            Some(
                snapshot
                    .fields
                    .iter()
                    .find(|field| field.id == id)
                    .cloned()
                    .unwrap_or(Field {
                        id,
                        key: format!("cf_{id}"),
                        label: format!("cf_{id}"),
                        multiple: None,
                        filterable: None,
                    }),
            )
        })
        .collect()
}

fn total_count(body: &Value) -> Result<u64, ToolError> {
    u64_field(body, "total_count")
        .ok_or_else(|| ToolError::refused("Redmine omitted total_count for the issue list."))
}

fn dedupe(rows: Vec<ScanRow>) -> Vec<ScanRow> {
    let mut seen = HashSet::new();
    rows.into_iter().filter(|row| seen.insert(row.id)).collect()
}

async fn fetch_offsets(
    client: &Client,
    params: &[(String, String)],
    offsets: &[u64],
) -> Result<Vec<Value>, ToolError> {
    let mut indexed = Vec::with_capacity(offsets.len());
    for chunk in offsets.chunks(CONCURRENCY) {
        let mut set = JoinSet::new();
        for offset in chunk {
            let client = client.clone();
            let params = params.to_vec();
            let offset = *offset;
            set.spawn(async move {
                let body = fetch_page(&client, &params, PAGE_SIZE, offset).await?;
                Ok::<_, ToolError>((offset, body))
            });
        }
        while let Some(joined) = set.join_next().await {
            let pair = joined.map_err(|err| {
                ToolError::refused(format!("A scan page stopped before it finished: {err}"))
            })??;
            indexed.push(pair);
        }
    }
    indexed.sort_by_key(|(offset, _)| *offset);
    Ok(indexed.into_iter().map(|(_, body)| body).collect())
}

async fn fetch_page(
    client: &Client,
    params: &[(String, String)],
    limit: u64,
    offset: u64,
) -> Result<Value, ToolError> {
    let mut query = params
        .iter()
        .filter(|(key, _)| !PAGING.contains(&key.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    query.push(("sort".to_string(), "id:asc".to_string()));
    query.push(("limit".to_string(), limit.to_string()));
    query.push(("offset".to_string(), offset.to_string()));
    get_json(client, "/issues.json", &query).await
}

fn rows_from(
    body: &Value,
    assigned: &BTreeMap<u64, String>,
    config: &crate::config::Config,
) -> Result<Vec<ScanRow>, ToolError> {
    array_field(body, "issues")?
        .iter()
        .map(|issue| Ok(scan_row(issue, assigned, config)))
        .collect()
}

fn scan_row(
    issue: &Value,
    assigned: &BTreeMap<u64, String>,
    config: &crate::config::Config,
) -> ScanRow {
    let mut fields = BTreeMap::new();
    if let Ok(custom) = array_field(issue, "custom_fields") {
        for field in custom {
            let Some(id) = field.get("id").and_then(Value::as_u64) else {
                continue;
            };
            let values = field.get("value").map(custom_values).unwrap_or_default();
            if values.is_empty() {
                continue;
            }
            let label = field.get("name").and_then(Value::as_str).unwrap_or("");
            let key = display_key(id, label, &config.fields, Some(assigned));
            fields.entry(key).or_insert_with(Vec::new).extend(values);
        }
    }
    ScanRow {
        id: issue.get("id").and_then(Value::as_u64).unwrap_or(0),
        project: named(issue, "project"),
        tracker: named(issue, "tracker"),
        status: named(issue, "status"),
        priority: named(issue, "priority"),
        assignee: named(issue, "assigned_to"),
        author: named(issue, "author"),
        category: named(issue, "category"),
        version: named(issue, "fixed_version"),
        fields,
    }
}

/// The stored values, as a filter would match them. Only a blank is "not set".
///
/// `"0"` stays: it is an unchecked boolean, or an integer field set to zero, and
/// `cf_N=0` matches it either way. Dropping it, as an issue render does, would
/// put "false" and "never set" in the same `(none)` group.
fn custom_values(value: &Value) -> Vec<String> {
    match value {
        Value::Array(items) => items
            .iter()
            .map(json_scalar)
            .map(|item| item.trim().to_string())
            .filter(|item| !item.is_empty())
            .collect(),
        other => {
            let text = json_scalar(other).trim().to_string();
            if text.is_empty() {
                Vec::new()
            } else {
                vec![text]
            }
        }
    }
}

fn json_scalar(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        Value::Bool(flag) => flag.to_string(),
        _ => String::new(),
    }
}

fn named(issue: &Value, key: &str) -> String {
    issue
        .get(key)
        .and_then(|value| value.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

fn cache_key(params: &[(String, String)]) -> String {
    let mut pairs = params
        .iter()
        .filter(|(key, _)| !PAGING.contains(&key.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    pairs.sort();
    pairs
        .into_iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// What a zero result with a custom field filter turned out to be.
pub(crate) enum ValueCheck {
    /// Every requested value is in use in that scope, or there was nothing to spell-check.
    Confirmed,
    /// The scope is over the scan cap, so the old spelling hint still applies.
    Unchecked,
}

/// When a result is zero and plain custom field values were filtered on, check
/// those values against the scope the other filters define.
///
/// Every plain value filter is removed from that scope, not just the one being
/// checked. Otherwise a typo in one field empties the scope for the next, and
/// the error lands on the field that was spelled right. Operator filters (`~`,
/// `!` and the rest) stay, because they aren't spelling questions.
///
/// An empty scope means the other filters already match nothing, so the zero
/// is real and nothing is reported. Membership is exact. A case difference or a
/// unique substring is named, and the call still fails, so a corrected answer is
/// never substituted quietly. A `|` list fails if any part is missing.
pub(crate) async fn check_values(
    ctx: &Ctx,
    params: &[(String, String)],
    fields: &[Field],
) -> Result<ValueCheck, ToolError> {
    let checked = fields
        .iter()
        .filter_map(|field| {
            let key = format!("cf_{}", field.id);
            let raw = params.iter().find(|(name, _)| *name == key)?.1.as_str();
            Some((field, key, plain_parts(raw)?))
        })
        .collect::<Vec<_>>();
    if checked.is_empty() {
        return Ok(ValueCheck::Confirmed);
    }
    let scope = params
        .iter()
        .filter(|(name, _)| !checked.iter().any(|(_, key, _)| key == name))
        .cloned()
        .collect::<Vec<_>>();
    let loaded = match scan(ctx, &scope).await {
        Ok(loaded) => loaded,
        Err(ScanError::OverCap { .. }) => return Ok(ValueCheck::Unchecked),
        Err(ScanError::Tool(err)) => return Err(err),
    };
    if loaded.issues.is_empty() {
        return Ok(ValueCheck::Confirmed);
    }
    let errors = checked
        .iter()
        .filter_map(|(field, _, parts)| {
            unknown_value_error(&field.label, parts, &tally(&loaded.issues, field))
        })
        .collect::<Vec<_>>();
    if errors.is_empty() {
        Ok(ValueCheck::Confirmed)
    } else {
        Err(ToolError::refused(errors.join(" ")))
    }
}

/// `None` when the value uses an operator. Those zeros are not spelling.
fn plain_parts(value: &str) -> Option<Vec<String>> {
    let mut parts = Vec::new();
    for part in value.split('|') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if part.starts_with(['~', '!', '>', '<', '*']) {
            return None;
        }
        parts.push(part.to_string());
    }
    if parts.is_empty() { None } else { Some(parts) }
}

fn tally(rows: &[ScanRow], field: &Field) -> Vec<(String, u64)> {
    let axis = Axis::Custom(field.clone());
    let mut counts: BTreeMap<String, u64> = BTreeMap::new();
    for row in rows {
        for value in values_of(row, &axis) {
            *counts.entry(value).or_default() += 1;
        }
    }
    let mut list = counts.into_iter().collect::<Vec<_>>();
    list.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    list
}

fn unknown_value_error(label: &str, parts: &[String], counts: &[(String, u64)]) -> Option<String> {
    let missing = parts
        .iter()
        .filter(|part| !counts.iter().any(|(value, _)| value == *part))
        .cloned()
        .collect::<Vec<_>>();
    if missing.is_empty() {
        return None;
    }
    let quoted = missing
        .iter()
        .map(|value| format!("\"{value}\""))
        .collect::<Vec<_>>();
    let listed = if quoted.len() == 1 {
        quoted[0].clone()
    } else {
        let last = quoted.last().map(String::as_str).unwrap_or("");
        format!("{} or {last}", quoted[..quoted.len() - 1].join(", "))
    };
    let mut message = format!(
        "No issue in this scope has {label} = {listed}. Values in use: {}.",
        format_counts(counts)
    );
    let suggestions = missing
        .iter()
        .filter_map(|part| {
            suggest(part, counts).map(|hit| {
                if missing.len() == 1 {
                    format!("Did you mean \"{hit}\"?")
                } else {
                    format!("Did you mean \"{hit}\" for \"{part}\"?")
                }
            })
        })
        .collect::<Vec<_>>();
    if !suggestions.is_empty() {
        message.push(' ');
        message.push_str(&suggestions.join(" "));
    }
    Some(message)
}

fn format_counts(counts: &[(String, u64)]) -> String {
    if counts.is_empty() {
        return "none".to_string();
    }
    let mut parts = counts
        .iter()
        .take(VALUES_SHOWN)
        .map(|(value, count)| format!("{value} ({count})"))
        .collect::<Vec<_>>();
    if counts.len() > VALUES_SHOWN {
        parts.push(format!("and {} more", counts.len() - VALUES_SHOWN));
    }
    parts.join(", ")
}

fn suggest(needle: &str, counts: &[(String, u64)]) -> Option<String> {
    let needle_folded = needle.to_lowercase();
    let mut case_hit = None;
    let mut case_hits = 0usize;
    for (value, _) in counts {
        if value.to_lowercase() == needle_folded {
            case_hits += 1;
            case_hit = Some(value.clone());
        }
    }
    if case_hits == 1 && case_hit.as_deref() != Some(needle) {
        return case_hit;
    }
    if case_hits > 0 || needle_folded.chars().count() < SUGGEST_MIN {
        return None;
    }
    let mut substring = None;
    let mut substring_hits = 0usize;
    for (value, _) in counts {
        let folded = value.to_lowercase();
        if folded != needle_folded && folded.contains(&needle_folded) {
            substring_hits += 1;
            substring = Some(value.clone());
        }
    }
    if substring_hits == 1 { substring } else { None }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Builtin {
    Status,
    Tracker,
    Priority,
    Assignee,
    Author,
    Category,
    Version,
    Project,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Axis {
    Builtin(Builtin),
    Custom(Field),
}

impl Axis {
    fn name(&self) -> &str {
        match self {
            Self::Builtin(builtin) => builtin.name(),
            Self::Custom(field) => &field.key,
        }
    }

    fn label(&self) -> &str {
        match self {
            Self::Builtin(builtin) => builtin.name(),
            Self::Custom(field) => {
                if field.label.is_empty() {
                    &field.key
                } else {
                    &field.label
                }
            }
        }
    }

    fn echo(&self) -> Option<String> {
        match self {
            Self::Custom(field) => Some(format!("{} -> {} ({})", field.key, field.label, field.id)),
            Self::Builtin(_) => None,
        }
    }
}

impl Builtin {
    fn name(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Tracker => "tracker",
            Self::Priority => "priority",
            Self::Assignee => "assignee",
            Self::Author => "author",
            Self::Category => "category",
            Self::Version => "version",
            Self::Project => "project",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "status" => Some(Self::Status),
            "tracker" => Some(Self::Tracker),
            "priority" => Some(Self::Priority),
            "assignee" => Some(Self::Assignee),
            "author" => Some(Self::Author),
            "category" => Some(Self::Category),
            "version" => Some(Self::Version),
            "project" => Some(Self::Project),
            _ => None,
        }
    }
}

pub(crate) fn builtin_names() -> &'static [&'static str] {
    BUILTINS
}

pub(crate) async fn resolve_axes(ctx: &Ctx, names: &[String]) -> Result<Vec<Axis>, ToolError> {
    if names.is_empty() || names.len() > 2 {
        return Err(ToolError::refused(
            "group_by takes one dimension for a histogram or two for a cross-tab.",
        ));
    }
    let mut axes = Vec::with_capacity(names.len());
    for name in names {
        let axis = resolve_axis(ctx, name).await?;
        if axes.iter().any(|earlier: &Axis| same_axis(earlier, &axis)) {
            return Err(ToolError::refused(format!(
                "group_by repeats {}.",
                axis.label()
            )));
        }
        axes.push(axis);
    }
    Ok(axes)
}

async fn resolve_axis(ctx: &Ctx, name: &str) -> Result<Axis, ToolError> {
    let name = name.trim();
    if let Some(builtin) = Builtin::parse(name) {
        return Ok(Axis::Builtin(builtin));
    }
    let field = ctx
        .catalog
        .resolve_custom_field(name)
        .await
        .map_err(catalog_err)?;
    Ok(Axis::Custom(field))
}

fn same_axis(left: &Axis, right: &Axis) -> bool {
    match (left, right) {
        (Axis::Builtin(left), Axis::Builtin(right)) => left == right,
        (Axis::Custom(left), Axis::Custom(right)) => left.id == right.id,
        _ => false,
    }
}

pub(crate) struct FacetOutput<'a> {
    pub scan: &'a Scan,
    pub axes: &'a [Axis],
    pub top: usize,
    pub echoes: &'a [String],
    pub format: OutputFormat,
    /// No status and no saved query was given, so Redmine counted open issues only.
    pub open_by_default: bool,
}

pub(crate) fn render(view: &FacetOutput<'_>) -> String {
    let top = view.top.max(1);
    match view.axes {
        [axis] => render_histogram(view, axis, top),
        [row_axis, col_axis] => render_cross(view, row_axis, col_axis, top),
        _ => String::new(),
    }
}

fn render_histogram(view: &FacetOutput<'_>, axis: &Axis, top: usize) -> String {
    let grouped = group_histogram(&view.scan.issues, axis, top);
    let notes = facet_notes(view, std::slice::from_ref(axis), &grouped_note(&grouped));
    match view.format {
        OutputFormat::Json => {
            let rows = grouped
                .rows
                .iter()
                .map(|(value, count)| vec![json!(value), json!(count)])
                .collect();
            columnar_json(&[axis.name(), "count"], rows, facet_page(view, &notes))
        }
        OutputFormat::Markdown => {
            let table_rows = grouped
                .rows
                .iter()
                .map(|(value, count)| vec![value.clone(), count.to_string()])
                .collect::<Vec<_>>();
            let table = if grouped.rows.is_empty() {
                String::new()
            } else {
                markdown_table(&[axis.name(), "count"], &table_rows)
            };
            markdown_facet(view, &table, &notes)
        }
    }
}

fn render_cross(view: &FacetOutput<'_>, row_axis: &Axis, col_axis: &Axis, top: usize) -> String {
    let grouped = group_cross(&view.scan.issues, row_axis, col_axis, top);
    let notes = facet_notes(view, view.axes, &cross_note(&grouped));
    match view.format {
        OutputFormat::Json => {
            let mut out = Map::new();
            out.insert(
                "rows".to_string(),
                json!(
                    grouped
                        .rows
                        .iter()
                        .map(|bucket| &bucket.label)
                        .collect::<Vec<_>>()
                ),
            );
            out.insert(
                "columns".to_string(),
                json!(
                    grouped
                        .cols
                        .iter()
                        .map(|bucket| &bucket.label)
                        .collect::<Vec<_>>()
                ),
            );
            out.insert("cells".to_string(), json!(grouped.cells));
            out.insert("row_totals".to_string(), json!(grouped.row_totals));
            out.insert("col_totals".to_string(), json!(grouped.col_totals));
            out.extend(facet_page(view, &notes));
            Value::Object(out).to_string()
        }
        OutputFormat::Markdown => {
            let mut headers = vec![row_axis.name(), "total"];
            headers.extend(grouped.cols.iter().map(|bucket| bucket.label.as_str()));
            let mut table_rows = Vec::new();
            for (index, bucket) in grouped.rows.iter().enumerate() {
                let mut line = vec![bucket.label.clone(), grouped.row_totals[index].to_string()];
                line.extend(grouped.cells[index].iter().map(|count| count.to_string()));
                table_rows.push(line);
            }
            if !grouped.rows.is_empty() {
                let grand: u64 = grouped.row_totals.iter().sum();
                let mut total = vec!["(total)".to_string(), grand.to_string()];
                total.extend(grouped.col_totals.iter().map(|count| count.to_string()));
                table_rows.push(total);
            }
            let table = if table_rows.is_empty() {
                String::new()
            } else {
                markdown_table(&headers, &table_rows)
            };
            markdown_facet(view, &table, &notes)
        }
    }
}

fn markdown_facet(view: &FacetOutput<'_>, table: &str, notes: &[String]) -> String {
    let mut out = heading(view);
    if !table.is_empty() {
        out.push_str("\n\n");
        out.push_str(table);
    }
    let mut echoes = view.echoes.to_vec();
    for axis in view.axes {
        if let Some(echo) = axis.echo()
            && !echoes.contains(&echo)
        {
            echoes.push(echo);
        }
    }
    if !echoes.is_empty() {
        out.push_str("\n\n");
        out.push_str(&echoes.join("\n"));
    }
    for note in notes {
        out.push_str("\n\n");
        out.push_str(note);
    }
    out
}

fn heading(view: &FacetOutput<'_>) -> String {
    let count = view.scan.issues.len();
    let noun = match (count == 1, view.open_by_default) {
        (true, true) => "open issue",
        (false, true) => "open issues",
        (true, false) => "issue",
        (false, false) => "issues",
    };
    let by = view
        .axes
        .iter()
        .map(Axis::name)
        .collect::<Vec<_>>()
        .join(" x ");
    format!("{count} {noun}, grouped by {by}:")
}

fn facet_page(view: &FacetOutput<'_>, notes: &[String]) -> Map<String, Value> {
    let mut page = Map::new();
    page.insert("total".to_string(), json!(view.scan.issues.len()));
    page.insert(
        "group_by".to_string(),
        json!(view.axes.iter().map(Axis::name).collect::<Vec<_>>()),
    );
    let mut echoes = view.echoes.to_vec();
    for axis in view.axes {
        if let Some(echo) = axis.echo()
            && !echoes.contains(&echo)
        {
            echoes.push(echo);
        }
    }
    if !echoes.is_empty() {
        page.insert("fields".to_string(), json!(echoes));
    }
    if view.scan.approximate {
        page.insert(
            "approximate".to_string(),
            Value::String(approximate_line(view.scan)),
        );
    }
    if !notes.is_empty() {
        page.insert("note".to_string(), Value::String(notes.join(" ")));
    }
    page
}

fn facet_notes(view: &FacetOutput<'_>, axes: &[Axis], rollup: &Option<String>) -> Vec<String> {
    let scan = view.scan;
    let mut notes = Vec::new();
    if view.open_by_default {
        notes.push(
            "No status was given, so Redmine counted open issues only. Pass status \"*\" for every status."
                .to_string(),
        );
    }
    if let Some(note) = multi_note(scan, axes) {
        notes.push(note);
    }
    if let Some(note) = rollup {
        notes.push(note.clone());
    }
    if scan.approximate {
        notes.push(format!("approximate: {}", approximate_line(scan)));
    }
    notes
}

fn approximate_line(scan: &Scan) -> String {
    let scanned = scan.issues.len();
    format!(
        "issues changed during the scan (scanned {scanned}, Redmine reported {})",
        scan.reported
    )
}

fn multi_note(scan: &Scan, axes: &[Axis]) -> Option<String> {
    let labels = axes
        .iter()
        .filter(|axis| axis_is_multi(axis, &scan.issues))
        .map(Axis::label)
        .collect::<Vec<_>>();
    if labels.is_empty() {
        return None;
    }
    let expanded = expanded_count(&scan.issues, axes);
    let issues = scan.issues.len();
    let verb = if labels.len() == 1 { "is" } else { "are" };
    let who = join_and(&labels);
    if expanded > u64::try_from(issues).unwrap_or(u64::MAX) {
        Some(format!(
            "{who} {verb} multi-value; an issue counts once per value, so groups sum to more than {issues}."
        ))
    } else {
        Some(format!(
            "{who} {verb} multi-value; an issue counts once per value."
        ))
    }
}

fn axis_is_multi(axis: &Axis, rows: &[ScanRow]) -> bool {
    let Axis::Custom(field) = axis else {
        return false;
    };
    field.multiple == Some(true)
        || rows.iter().any(|row| {
            row.fields
                .get(&field.key)
                .is_some_and(|values| values.len() > 1)
        })
}

fn expanded_count(rows: &[ScanRow], axes: &[Axis]) -> u64 {
    let mut total = 0u64;
    for row in rows {
        let mut expanded = 1u64;
        for axis in axes {
            expanded *= u64::try_from(values_of(row, axis).len()).unwrap_or(1);
        }
        total += expanded;
    }
    total
}

fn join_and(labels: &[&str]) -> String {
    match labels {
        [] => String::new(),
        [one] => (*one).to_string(),
        [first, second] => format!("{first} and {second}"),
        _ => {
            let last = labels[labels.len() - 1];
            format!("{} and {last}", labels[..labels.len() - 1].join(", "))
        }
    }
}

struct Histogram {
    rows: Vec<(String, u64)>,
    groups: usize,
}

struct Bucket {
    label: String,
    keys: Vec<String>,
}

struct CrossTab {
    rows: Vec<Bucket>,
    cols: Vec<Bucket>,
    cells: Vec<Vec<u64>>,
    row_totals: Vec<u64>,
    col_totals: Vec<u64>,
    row_groups: usize,
    col_groups: usize,
}

fn grouped_note(grouped: &Histogram) -> Option<String> {
    let shown = grouped
        .rows
        .iter()
        .filter(|(label, _)| label != "(other)")
        .count();
    if grouped.groups > shown {
        Some(format!("Showing top {shown} of {} groups.", grouped.groups))
    } else {
        None
    }
}

fn cross_note(grouped: &CrossTab) -> Option<String> {
    let row_shown = grouped
        .rows
        .iter()
        .filter(|bucket| bucket.label != "(other)")
        .count();
    let col_shown = grouped
        .cols
        .iter()
        .filter(|bucket| bucket.label != "(other)")
        .count();
    let rows_cut = grouped.row_groups > row_shown;
    let cols_cut = grouped.col_groups > col_shown;
    match (rows_cut, cols_cut) {
        (true, true) => Some(format!(
            "Showing top {row_shown} of {} row groups and top {col_shown} of {} column groups.",
            grouped.row_groups, grouped.col_groups
        )),
        (true, false) => Some(format!(
            "Showing top {row_shown} of {} row groups.",
            grouped.row_groups
        )),
        (false, true) => Some(format!(
            "Showing top {col_shown} of {} column groups.",
            grouped.col_groups
        )),
        (false, false) => None,
    }
}

fn group_histogram(rows: &[ScanRow], axis: &Axis, top: usize) -> Histogram {
    let mut counts = BTreeMap::new();
    for row in rows {
        for value in values_of(row, axis) {
            *counts.entry(value).or_default() += 1u64;
        }
    }
    let (buckets, groups) = buckets(&counts, top);
    let rows = buckets
        .into_iter()
        .map(|bucket| {
            let count = bucket.keys.iter().map(|key| counts[key]).sum();
            (bucket.label, count)
        })
        .collect();
    Histogram { rows, groups }
}

fn group_cross(rows: &[ScanRow], row_axis: &Axis, col_axis: &Axis, top: usize) -> CrossTab {
    let mut grid: BTreeMap<(String, String), u64> = BTreeMap::new();
    let mut row_counts: BTreeMap<String, u64> = BTreeMap::new();
    let mut col_counts: BTreeMap<String, u64> = BTreeMap::new();
    for row in rows {
        for row_value in values_of(row, row_axis) {
            for col_value in values_of(row, col_axis) {
                *grid
                    .entry((row_value.clone(), col_value.clone()))
                    .or_default() += 1;
                *row_counts.entry(row_value.clone()).or_default() += 1;
                *col_counts.entry(col_value).or_default() += 1;
            }
        }
    }
    let (row_buckets, row_groups) = buckets(&row_counts, top);
    let (col_buckets, col_groups) = buckets(&col_counts, top);
    let cells = row_buckets
        .iter()
        .map(|row_bucket| {
            col_buckets
                .iter()
                .map(|col_bucket| sum_grid(&grid, &row_bucket.keys, &col_bucket.keys))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let row_totals = cells.iter().map(|line| line.iter().sum()).collect();
    let col_totals = (0..col_buckets.len())
        .map(|index| cells.iter().map(|line| line[index]).sum())
        .collect();
    CrossTab {
        rows: row_buckets,
        cols: col_buckets,
        cells,
        row_totals,
        col_totals,
        row_groups,
        col_groups,
    }
}

fn buckets(counts: &BTreeMap<String, u64>, top: usize) -> (Vec<Bucket>, usize) {
    let mut keys = counts.keys().cloned().collect::<Vec<_>>();
    keys.sort_by(|left, right| {
        counts[right]
            .cmp(&counts[left])
            .then_with(|| left.cmp(right))
    });
    let groups = keys.len();
    if groups == 0 {
        return (Vec::new(), 0);
    }
    let keep = top.max(1).min(groups);
    if groups <= keep {
        let buckets = keys
            .into_iter()
            .map(|key| Bucket {
                label: key.clone(),
                keys: vec![key],
            })
            .collect();
        return (buckets, groups);
    }
    let dropped = keys.split_off(keep);
    let mut buckets = keys
        .into_iter()
        .map(|key| Bucket {
            label: key.clone(),
            keys: vec![key],
        })
        .collect::<Vec<_>>();
    buckets.push(Bucket {
        label: "(other)".to_string(),
        keys: dropped,
    });
    (buckets, groups)
}

fn sum_grid(grid: &BTreeMap<(String, String), u64>, rows: &[String], cols: &[String]) -> u64 {
    let mut total = 0u64;
    for row in rows {
        for col in cols {
            total += grid.get(&(row.clone(), col.clone())).copied().unwrap_or(0);
        }
    }
    total
}

fn values_of(row: &ScanRow, axis: &Axis) -> Vec<String> {
    let values = match axis {
        Axis::Builtin(builtin) => {
            let raw = match builtin {
                Builtin::Status => &row.status,
                Builtin::Tracker => &row.tracker,
                Builtin::Priority => &row.priority,
                Builtin::Assignee => &row.assignee,
                Builtin::Author => &row.author,
                Builtin::Category => &row.category,
                Builtin::Version => &row.version,
                Builtin::Project => &row.project,
            };
            if raw.is_empty() {
                Vec::new()
            } else {
                vec![raw.clone()]
            }
        }
        Axis::Custom(field) => row.fields.get(&field.key).cloned().unwrap_or_default(),
    };
    if values.is_empty() {
        vec!["(none)".to_string()]
    } else {
        values
    }
}

fn with_commas(value: u64) -> String {
    let raw = value.to_string();
    let mut out = String::new();
    for (index, ch) in raw.chars().rev().enumerate() {
        if index > 0 && index % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out.chars().rev().collect()
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};

    use super::*;

    fn row(status: &str, assignee: &str, severity: &[&str]) -> ScanRow {
        let mut fields = BTreeMap::new();
        if !severity.is_empty() {
            fields.insert(
                "severity".to_string(),
                severity.iter().map(|value| (*value).to_string()).collect(),
            );
        }
        ScanRow {
            id: 0,
            project: "Garden".to_string(),
            tracker: "Bug".to_string(),
            status: status.to_string(),
            priority: "Normal".to_string(),
            assignee: assignee.to_string(),
            author: "Ada".to_string(),
            category: String::new(),
            version: String::new(),
            fields,
        }
    }

    fn severity_axis(multiple: bool) -> Axis {
        Axis::Custom(Field {
            id: 36,
            key: "severity".to_string(),
            label: "Severity".to_string(),
            multiple: Some(multiple),
            filterable: Some(true),
        })
    }

    #[test]
    fn cache_key_ignores_parameter_order_and_paging() {
        let left = [
            ("status_id".to_string(), "open".to_string()),
            ("project_id".to_string(), "12".to_string()),
        ];
        let right = [
            ("project_id".to_string(), "12".to_string()),
            ("limit".to_string(), "100".to_string()),
            ("status_id".to_string(), "open".to_string()),
        ];
        assert_eq!(cache_key(&left), cache_key(&right));
        assert_ne!(
            cache_key(&left),
            cache_key(&[("project_id".to_string(), "9".to_string())])
        );
    }

    #[test]
    fn an_empty_value_is_its_own_group_and_top_rolls_the_rest_into_other() {
        let rows = vec![
            row("", "Ada", &[]),
            row("Review", "Ada", &[]),
            row("New", "Ada", &[]),
            row("Closed", "Ada", &[]),
        ];
        let grouped = group_histogram(&rows, &Axis::Builtin(Builtin::Status), 2);
        assert_eq!(grouped.groups, 4);
        assert_eq!(grouped.rows.len(), 3);
        assert_eq!(grouped.rows[0].1, 1);
        assert_eq!(grouped.rows[2].0, "(other)");
        assert_eq!(grouped.rows[2].1, 2);
        assert!(grouped.rows.iter().any(|(label, _)| label == "(none)"));
        let sum: u64 = grouped.rows.iter().map(|(_, count)| count).sum();
        assert_eq!(sum, 4);
    }

    #[test]
    fn a_multi_value_field_counts_an_issue_once_per_value() {
        let rows = vec![
            row("New", "Ada", &["Minor", "Major"]),
            row("New", "Ada", &["Minor"]),
        ];
        let grouped = group_histogram(&rows, &severity_axis(true), 25);
        let minor = grouped
            .rows
            .iter()
            .find(|(label, _)| label == "Minor")
            .unwrap();
        let major = grouped
            .rows
            .iter()
            .find(|(label, _)| label == "Major")
            .unwrap();
        assert_eq!(minor.1, 2);
        assert_eq!(major.1, 1);
        let sum: u64 = grouped.rows.iter().map(|(_, count)| count).sum();
        assert_eq!(sum, 3);
    }

    #[test]
    fn cross_tab_totals_include_the_rolled_up_cells() {
        let rows = vec![
            row("A", "X", &[]),
            row("A", "X", &[]),
            row("A", "Y", &[]),
            row("B", "X", &[]),
            row("C", "Z", &[]),
        ];
        let grouped = group_cross(
            &rows,
            &Axis::Builtin(Builtin::Status),
            &Axis::Builtin(Builtin::Assignee),
            1,
        );
        assert_eq!(grouped.row_groups, 3);
        assert_eq!(grouped.col_groups, 3);
        assert_eq!(grouped.rows[0].label, "A");
        assert_eq!(grouped.rows[1].label, "(other)");
        assert_eq!(grouped.cols[0].label, "X");
        assert_eq!(grouped.cols[1].label, "(other)");
        assert_eq!(grouped.cells, vec![vec![2, 1], vec![1, 1]]);
        assert_eq!(grouped.row_totals, vec![3, 2]);
        assert_eq!(grouped.col_totals, vec![3, 2]);
        let grand: u64 = grouped.row_totals.iter().sum();
        assert_eq!(grand, 5);
        let columns: u64 = grouped.col_totals.iter().sum();
        assert_eq!(grand, columns);
    }

    #[test]
    fn a_case_difference_and_a_unique_substring_are_suggestions_and_a_near_miss_is_not() {
        let counts = vec![
            ("Minor".to_string(), 210),
            ("Major".to_string(), 90),
            ("Critical".to_string(), 12),
        ];
        let case = unknown_value_error("Severity", &["minor".to_string()], &counts).unwrap();
        assert!(case.contains("Did you mean \"Minor\"?"), "{case}");
        assert!(case.contains("Minor (210)"), "{case}");
        let miss = unknown_value_error("Severity", &["Minr".to_string()], &counts).unwrap();
        assert!(!miss.contains("Did you mean"), "{miss}");
        assert!(miss.contains("Critical (12)"), "{miss}");
        let both = unknown_value_error(
            "Severity",
            &["Minor".to_string(), "Minr".to_string()],
            &counts,
        )
        .unwrap();
        assert!(both.contains("\"Minr\""), "{both}");
        assert!(!both.contains("Did you mean"), "{both}");
        assert!(unknown_value_error("Severity", &["Minor".to_string()], &counts).is_none());
    }

    #[test]
    fn an_operator_is_not_a_spelling_question_and_commas_group_thousands() {
        assert!(plain_parts("~minor").is_none());
        assert!(plain_parts("!*").is_none());
        assert_eq!(
            plain_parts("Minor| Major").unwrap(),
            vec!["Minor".to_string(), "Major".to_string()]
        );
        assert_eq!(with_commas(12400), "12,400");
        assert_eq!(with_commas(5000), "5,000");
        assert_eq!(
            cap_message(12400, 5000),
            "12,400 issues match, over the scan cap of 5,000. Narrow the filters (a project, a status, a date range), or raise REDMINE_MAX_SCAN."
        );
    }

    #[test]
    fn a_multi_value_array_stays_split_and_a_zero_is_a_value() {
        let issue = json!({
            "id": 7,
            "status": { "name": "New" },
            "custom_fields": [
                { "id": 36, "name": "Severity Rating", "value": ["Minor", "Major"] },
                { "id": 40, "name": "Severity", "value": "0" },
                { "id": 41, "name": "Notes", "value": "" }
            ]
        });
        let mut assigned = BTreeMap::new();
        assigned.insert(36, "severity".to_string());
        assigned.insert(40, "severity_plain".to_string());
        assigned.insert(41, "notes".to_string());
        let config = crate::config::load_from(
            &HashMap::from([
                ("REDMINE_URL".to_string(), "http://example.test".to_string()),
                ("REDMINE_API_KEY".to_string(), "test-key".to_string()),
            ]),
            |_| Err("no file".to_string()),
        )
        .unwrap();
        let row = scan_row(&issue, &assigned, &config);
        assert_eq!(
            row.fields.get("severity").unwrap(),
            &vec!["Minor".to_string(), "Major".to_string()]
        );
        // An unchecked boolean is its own group, not "(none)".
        assert_eq!(
            row.fields.get("severity_plain").unwrap(),
            &vec!["0".to_string()]
        );
        assert!(!row.fields.contains_key("notes"));
        assert!(row.status == "New");
    }

    #[test]
    fn facet_budget_for_a_thirty_by_thirty_crosstab() {
        let issues = (0..5_000u64)
            .map(|index| {
                let mut row = row(
                    &format!("S{:02}", index % 30),
                    &format!("A{:02}", (index / 30) % 30),
                    &[],
                );
                row.id = index;
                row
            })
            .collect::<Vec<_>>();
        let scan = Scan {
            issues,
            total: 5_000,
            reported: 5_000,
            approximate: false,
            params: Vec::new(),
            taken_at: Instant::now(),
        };
        let axes = [
            Axis::Builtin(Builtin::Status),
            Axis::Builtin(Builtin::Assignee),
        ];
        let markdown = render(&FacetOutput {
            scan: &scan,
            axes: &axes,
            top: 25,
            echoes: &[],
            format: OutputFormat::Markdown,
            open_by_default: false,
        });
        let json_text = render(&FacetOutput {
            scan: &scan,
            axes: &axes,
            top: 25,
            echoes: &[],
            format: OutputFormat::Json,
            open_by_default: false,
        });
        let parsed: Value = serde_json::from_str(&json_text).unwrap();
        // Markdown has row and column totals, so the JSON carries them too.
        assert_eq!(parsed["row_totals"].as_array().unwrap().len(), 26);
        assert_eq!(parsed["col_totals"].as_array().unwrap().len(), 26);
        let cells = parsed["cells"].as_array().unwrap();
        assert_eq!(cells.len(), 26, "25 kept rows plus (other)");
        let sum: u64 = cells
            .iter()
            .map(|line| {
                line.as_array()
                    .unwrap()
                    .iter()
                    .map(|value| value.as_u64().unwrap())
                    .sum::<u64>()
            })
            .sum();
        assert_eq!(sum, 5_000);
        assert_eq!(parsed["total"], 5_000);
        assert!(parsed.get("approximate").is_none());
        assert!(markdown.contains("(other)"), "{markdown}");
        assert!(markdown.contains("(total)"), "{markdown}");
        assert!(markdown.contains("Showing top 25 of 30"), "{markdown}");
        assert_budget("facet markdown, 30 by 30", &markdown, 3_950);
        assert_budget("facet json, 30 by 30", &json_text, 2_270);
    }

    fn assert_budget(label: &str, text: &str, ceiling: usize) {
        assert!(
            text.len() <= ceiling,
            "{label} is {} bytes, over its {ceiling} byte budget",
            text.len()
        );
    }
}
