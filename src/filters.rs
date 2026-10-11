use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use rmcp::model::{JsonObject, Tool, ToolAnnotations};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::catalog::{CatalogError, Ctx, Field, FieldSource, Snapshot, StatusFilter};
use crate::error::ToolError;
use crate::issue::{FieldLookup, IssueRow, issue_row};
use crate::render::{clip, columnar_json, fenced, markdown_table, page_heading};
use crate::sanitize::sanitize;
use crate::tools::{OutputFormat, ProjectId};

const PAGE_DEFAULT: u64 = 25;
const SUBJECT_CLIP: usize = 70;

/// A list row's description cap. `get_issue` uses REDMINE_MAX_DESCRIPTION_CHARS
/// (20,000 by default), which over 25 rows would be half a megabyte.
const LIST_DESCRIPTION_CHARS: usize = 1_000;

/// No issue has this value, so a probe filtered on it returns zero rows when
/// Redmine applies the filter, and rows when it silently drops it.
const PROBE_VALUE: &str = "redmine-dev-mcp-probe-0f3c9a";

/// Arguments shared by list_issues and count_issues. Configured field keys are
/// added per instance.
const FILTER_ARGS: &[&str] = &[
    "project_id",
    "status",
    "tracker",
    "priority",
    "category",
    "assignee",
    "author",
    "subject",
    "created_on",
    "updated_on",
    "query_id",
    "custom_fields",
];
const LIST_ONLY_ARGS: &[&str] = &[
    "sort",
    "include_fields",
    "include_description",
    "limit",
    "offset",
    "format",
];
const FACET_ARGS: &[&str] = &["group_by", "top", "format"];
const META_ARGS: &[&str] = &["project_id", "sections", "format"];

/// With `query_id`, Redmine loads the saved query and ignores every filter in
/// the request. Only these may come with it.
const QUERY_ID_COMPANIONS: &[&str] = &[
    "query_id",
    "project_id",
    "sort",
    "include_fields",
    "include_description",
    "limit",
    "offset",
    "format",
];

const DEFAULT_COLUMNS: &[&str] = &[
    "id", "tracker", "status", "priority", "assignee", "updated", "subject",
];

/// Columns `include_fields` may name, on top of a custom field key.
const EXTRA_COLUMNS: &[&str] = &[
    "project", "author", "category", "created", "closed", "version", "done",
];

const META_SECTIONS: &[&str] = &[
    "statuses",
    "trackers",
    "priorities",
    "custom_fields",
    "queries",
    "categories",
    "members",
];

const DEFAULT_SECTIONS: &[&str] = &[
    "statuses",
    "trackers",
    "priorities",
    "custom_fields",
    "queries",
    "categories",
];

/// `list_issues`, `count_issues`, `facet_issues` and `list_meta`, with a parameter per configured field.
pub fn filter_tools(config: &crate::config::Config) -> Vec<Tool> {
    let read_only = ToolAnnotations::new().read_only(true);
    vec![
        Tool::new(
            "list_issues",
            "List issues as a compact table. Filters take names: status, tracker, priority, category, assignee, author, and any custom field key. Defaults to open issues, newest updated first. The description stays off unless include_description is set. A custom field value no issue in scope has is an error listing the values in use. Read-only.",
            list_schema(config, true),
        )
        .with_title("List issues")
        .with_annotations(read_only.clone()),
        Tool::new(
            "count_issues",
            "How many issues match. Same named filters as list_issues. One number. A custom field value no issue in scope has is an error listing the values in use. Read-only.",
            list_schema(config, false),
        )
        .with_title("Count issues")
        .with_annotations(read_only.clone()),
        Tool::new(
            "facet_issues",
            "Group matching issues and return counts, never the issues. Same named filters as list_issues. Pass one group_by for a histogram or two for a cross-tab. Above REDMINE_MAX_SCAN the call is refused rather than returning a partial total. Read-only.",
            facet_schema(config),
        )
        .with_title("Break issues down by field")
        .with_annotations(read_only.clone()),
        Tool::new(
            "list_meta",
            "Names and ids for statuses, trackers, priorities, custom fields and saved queries. Pass project_id for that project's categories. Add members to sections for its members. Custom field values are not listed. list_issues and count_issues name the values in use when a filter matches nothing. Read-only.",
            meta_schema(),
        )
        .with_title("List field names")
        .with_annotations(read_only),
    ]
}

pub fn filter_tool(config: &crate::config::Config, name: &str) -> Option<Tool> {
    filter_tools(config)
        .into_iter()
        .find(|tool| tool.name == name)
}

pub async fn list_issues(ctx: &Ctx, args: &JsonObject) -> Result<String, ToolError> {
    check_arg_names(args, ctx.client.config(), &[FILTER_ARGS, LIST_ONLY_ARGS])?;
    let args = parse_args(args)?;
    let limit = cap_limit(args.limit, ctx.client.config().max_list);
    let offset = args.offset.unwrap_or(0);
    let resolved = resolve_filters(ctx, &args).await?;
    let snapshot = ctx.catalog.get().await.map_err(catalog_err)?;
    let extra = args.include_fields.clone().unwrap_or_default();
    check_columns(&extra, &snapshot.fields)?;

    let mut query = resolved.params.clone();
    let sort = args
        .sort
        .as_deref()
        .map(str::trim)
        .filter(|sort| !sort.is_empty())
        .unwrap_or("updated_on:desc");
    query.push(("sort".to_string(), sort.to_string()));
    query.push(("limit".to_string(), limit.to_string()));
    query.push(("offset".to_string(), offset.to_string()));

    let body = get_json(&ctx.client, "/issues.json", &query).await?;
    let issues = array_field(&body, "issues")?;
    let total = u64_field(&body, "total_count");
    check_applied(
        ctx,
        &resolved.params,
        &resolved.custom_fields,
        total.unwrap_or(0),
    )
    .await?;
    let assigned = snapshot
        .fields
        .iter()
        .map(|field| (field.id, field.key.clone()))
        .collect::<BTreeMap<_, _>>();
    let lookup = FieldLookup {
        configured: &ctx.client.config().fields,
        assigned: Some(&assigned),
        names_error: None,
    };
    let rows = issues
        .iter()
        .map(|issue| issue_row(issue, lookup))
        .collect::<Vec<_>>();
    let hint = value_hint(ctx, &resolved, total.unwrap_or(0)).await?;
    match args.format {
        OutputFormat::Json => {
            let columns = column_names(&rows, &extra, args.include_description);
            let values = rows
                .iter()
                .map(|row| {
                    columns
                        .iter()
                        .map(|column| json_cell(row, column, ctx))
                        .collect()
                })
                .collect();
            let mut page = page_fields(total, offset, limit);
            if !resolved.echoes.is_empty() {
                page.insert("fields".to_string(), json!(resolved.echoes));
            }
            if !hint.is_empty() {
                page.insert("hint".to_string(), Value::String(hint));
            }
            let headers = columns.iter().map(String::as_str).collect::<Vec<_>>();
            Ok(columnar_json(&headers, values, page))
        }
        OutputFormat::Markdown => Ok(render_list(
            &ListView {
                rows: &rows,
                total,
                offset,
                limit,
                extra: &extra,
                with_description: args.include_description,
                echoes: &resolved.echoes,
                hint: &hint,
            },
            ctx,
        )),
    }
}

pub async fn count_issues(ctx: &Ctx, args: &JsonObject) -> Result<String, ToolError> {
    check_arg_names(args, ctx.client.config(), &[FILTER_ARGS])?;
    let args = parse_args(args)?;
    let resolved = resolve_filters(ctx, &args).await?;
    let mut query = resolved.params.clone();
    query.push(("limit".to_string(), "1".to_string()));
    query.push(("offset".to_string(), "0".to_string()));
    let body = get_json(&ctx.client, "/issues.json", &query).await?;
    let total = u64_field(&body, "total_count").unwrap_or(0);
    check_applied(ctx, &resolved.params, &resolved.custom_fields, total).await?;
    let (noun, verb) = if total == 1 {
        ("issue", "matches")
    } else {
        ("issues", "match")
    };
    let mut text = format!("{total} {noun} {verb} {}.", describe(&resolved.params));
    if !resolved.echoes.is_empty() {
        text.push(' ');
        text.push_str(&resolved.echoes.join("; "));
        text.push('.');
    }
    let hint = value_hint(ctx, &resolved, total).await?;
    if !hint.is_empty() {
        text.push(' ');
        text.push_str(&hint);
    }
    Ok(text)
}

pub async fn facet_issues(ctx: &Ctx, args: &JsonObject) -> Result<String, ToolError> {
    check_arg_names(args, ctx.client.config(), &[FILTER_ARGS, FACET_ARGS])?;
    let args = parse_args(args)?;
    let top = match args.top {
        None => 25,
        Some(0) => return Err(ToolError::refused("top must be at least 1.")),
        Some(value) => usize::try_from(value).unwrap_or(usize::MAX),
    };
    let group_by = args.group_by.clone().unwrap_or_default();
    if group_by.is_empty() || group_by.len() > 2 {
        return Err(ToolError::refused(
            "group_by is required. Pass one dimension for a histogram or two for a cross-tab.",
        ));
    }
    let resolved = resolve_filters(ctx, &args).await?;
    let axes = crate::scan::resolve_axes(ctx, &group_by).await?;
    let loaded = match crate::scan::scan(ctx, &resolved.params).await {
        Ok(loaded) => loaded,
        Err(crate::scan::ScanError::OverCap { total, cap }) => {
            return Err(ToolError::refused(crate::scan::cap_message(total, cap)));
        }
        Err(crate::scan::ScanError::Tool(err)) => return Err(err),
    };
    let open_by_default = !resolved
        .params
        .iter()
        .any(|(key, _)| key == "status_id" || key == "query_id");
    Ok(crate::scan::render(&crate::scan::FacetOutput {
        scan: &loaded,
        axes: &axes,
        top,
        echoes: &resolved.echoes,
        format: args.format,
        open_by_default,
    }))
}

pub async fn list_meta(ctx: &Ctx, args: &JsonObject) -> Result<String, ToolError> {
    check_meta_args(args)?;
    let args = parse_args(args)?;
    let snapshot = ctx.catalog.get().await.map_err(catalog_err)?;
    let project = args.project_id.as_ref().map(ProjectId::as_query);
    let project = project.as_deref().filter(|id| !id.is_empty());
    let want = wanted_sections(args.sections.as_deref(), project)?;

    let mut out = Map::new();
    if want.contains("statuses") {
        out.insert(
            "statuses".to_string(),
            Value::Array(
                snapshot
                    .statuses
                    .iter()
                    .map(|status| {
                        json!({
                            "id": status.id,
                            "name": status.name,
                            "is_closed": status.is_closed,
                        })
                    })
                    .collect(),
            ),
        );
    }
    if want.contains("trackers") {
        out.insert("trackers".to_string(), named_json(&snapshot.trackers));
    }
    if want.contains("priorities") {
        out.insert("priorities".to_string(), named_json(&snapshot.priorities));
    }
    if want.contains("queries") {
        out.insert(
            "queries".to_string(),
            Value::Array(
                snapshot
                    .queries
                    .iter()
                    .map(|query| {
                        let mut row = json!({ "id": query.id, "name": query.name });
                        if let Some(project_id) = query.project_id {
                            row["project_id"] = json!(project_id);
                        }
                        row
                    })
                    .collect(),
            ),
        );
    }
    if want.contains("categories") {
        let project = project.ok_or_else(|| {
            ToolError::refused("categories needs a project_id. Redmine scopes them per project.")
        })?;
        let categories = ctx.catalog.categories(project).await.map_err(catalog_err)?;
        out.insert("categories".to_string(), named_json(&categories));
    }
    if want.contains("members") {
        let project = project.ok_or_else(|| {
            ToolError::refused(
                "members needs a project_id. A non-admin key only sees members per project.",
            )
        })?;
        let members = ctx.catalog.members(project).await.map_err(catalog_err)?;
        out.insert(
            "members".to_string(),
            Value::Array(
                members
                    .iter()
                    .map(|member| {
                        json!({
                            "id": member.id,
                            "name": member.name,
                            "kind": match member.kind {
                                crate::catalog::MemberKind::User => "user",
                                crate::catalog::MemberKind::Group => "group",
                            },
                        })
                    })
                    .collect(),
            ),
        );
    }
    if want.contains("custom_fields") {
        out.insert(
            "custom_fields".to_string(),
            Value::Array(
                snapshot
                    .fields
                    .iter()
                    .map(|field| {
                        let mut row = json!({
                            "key": field.key,
                            "id": field.id,
                            "label": field.label,
                        });
                        if field.multiple == Some(true) {
                            row["multiple"] = json!(true);
                        }
                        if field.filterable == Some(false) {
                            row["filter"] = json!(false);
                        }
                        row
                    })
                    .collect(),
            ),
        );
    }
    let notes = meta_notes(&snapshot);
    match args.format {
        OutputFormat::Json => {
            out.insert("notes".to_string(), json!(notes));
            Ok(Value::Object(out).to_string())
        }
        OutputFormat::Markdown => Ok(render_meta(&out, &notes)),
    }
}

struct Resolved {
    params: Vec<(String, String)>,
    echoes: Vec<String>,
    custom_keys: Vec<String>,
    /// The custom fields filtered on, for `check_applied`.
    custom_fields: Vec<Field>,
}

#[derive(Deserialize)]
struct CallArgs {
    #[serde(default)]
    project_id: Option<ProjectId>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    tracker: Option<String>,
    #[serde(default)]
    priority: Option<String>,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    assignee: Option<String>,
    #[serde(default)]
    author: Option<String>,
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    created_on: Option<String>,
    #[serde(default)]
    updated_on: Option<String>,
    #[serde(default)]
    query_id: Option<u64>,
    #[serde(default)]
    sort: Option<String>,
    #[serde(default)]
    custom_fields: Option<Map<String, Value>>,
    #[serde(default)]
    include_fields: Option<Vec<String>>,
    #[serde(default)]
    include_description: bool,
    #[serde(default)]
    limit: Option<u64>,
    #[serde(default)]
    offset: Option<u64>,
    #[serde(default)]
    format: OutputFormat,
    #[serde(default)]
    group_by: Option<Vec<String>>,
    #[serde(default)]
    top: Option<u64>,
    #[serde(default)]
    sections: Option<Vec<String>>,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

fn parse_args(args: &JsonObject) -> Result<CallArgs, ToolError> {
    serde_json::from_value(Value::Object(args.clone()))
        .map_err(|err| ToolError::refused(format!("Bad arguments: {err}")))
}

async fn resolve_filters(ctx: &Ctx, args: &CallArgs) -> Result<Resolved, ToolError> {
    if args.query_id.is_some() {
        check_query_id_alone(args)?;
    }
    let snapshot = ctx.catalog.get().await.map_err(catalog_err)?;
    let mut params = Vec::new();
    if let Some(project) = args.project_id.as_ref() {
        let project = project.as_query();
        if project.is_empty() {
            return Err(ToolError::refused("project_id is empty."));
        }
        params.push(("project_id".to_string(), project));
    }
    if let Some(query_id) = args.query_id {
        params.push(("query_id".to_string(), query_id.to_string()));
    }
    if let Some(status) = trimmed(&args.status) {
        let filter = snapshot.resolve_status(&status).map_err(catalog_err)?;
        params.push(("status_id".to_string(), status_param(filter)));
    }
    if let Some(tracker) = trimmed(&args.tracker) {
        let ids = snapshot.resolve_tracker(&tracker).map_err(catalog_err)?;
        params.push(("tracker_id".to_string(), join_ids(&ids)));
    }
    if let Some(priority) = trimmed(&args.priority) {
        let ids = snapshot.resolve_priority(&priority).map_err(catalog_err)?;
        params.push(("priority_id".to_string(), join_ids(&ids)));
    }
    let project = params
        .iter()
        .find(|(key, _)| key == "project_id")
        .map(|(_, value)| value.clone());
    if let Some(category) = trimmed(&args.category) {
        params.push((
            "category_id".to_string(),
            category_param(ctx, project.as_deref(), &category).await?,
        ));
    }
    if let Some(assignee) = trimmed(&args.assignee) {
        params.push((
            "assigned_to_id".to_string(),
            person_param(ctx, project.as_deref(), &assignee, true).await?,
        ));
    }
    if let Some(author) = trimmed(&args.author) {
        params.push((
            "author_id".to_string(),
            person_param(ctx, project.as_deref(), &author, false).await?,
        ));
    }
    if let Some(subject) = trimmed(&args.subject) {
        params.push(("subject".to_string(), subject_param(&subject)));
    }
    if let Some(created) = trimmed(&args.created_on) {
        params.push(("created_on".to_string(), normalize_date(&created)));
    }
    if let Some(updated) = trimmed(&args.updated_on) {
        params.push(("updated_on".to_string(), normalize_date(&updated)));
    }

    let inputs = custom_inputs(args)?;
    let mut echoes = Vec::new();
    let mut custom_keys = Vec::new();
    let mut seen: BTreeMap<u64, String> = BTreeMap::new();
    let mut custom_fields = Vec::new();
    for input in inputs {
        let field = match input.kind {
            // A top-level argument was already checked to be a configured key.
            InputKind::Configured => snapshot
                .fields
                .iter()
                .find(|field| field.key == input.key)
                .cloned()
                .ok_or_else(|| {
                    ToolError::refused(format!(
                        "{} is configured but not in the catalog.",
                        input.key
                    ))
                })?,
            InputKind::Map => ctx
                .catalog
                .resolve_custom_field(&input.key)
                .await
                .map_err(catalog_err)?,
        };
        let key = input.key;
        if let Some(other) = seen.insert(field.id, key.clone()) {
            return Err(ToolError::refused(format!(
                "{other} and {key} are both {} ({}).",
                field.label, field.id
            )));
        }
        if field.filterable == Some(false) {
            return Err(ToolError::refused(format!(
                "{} ({}) is not available as a filter. Its \"Used as a filter\" setting is off, or it is hidden from this key's roles. Redmine would ignore it and return unfiltered results.",
                field.label, field.id
            )));
        }
        echoes.push(format!("{key} -> {} ({})", field.label, field.id));
        custom_keys.push(key);
        params.push((format!("cf_{}", field.id), input.value));
        custom_fields.push(field);
    }
    Ok(Resolved {
        params,
        echoes,
        custom_keys,
        custom_fields,
    })
}

/// Refuse anything a saved query would silently override.
fn check_query_id_alone(args: &CallArgs) -> Result<(), ToolError> {
    let mut ignored = Vec::new();
    for (name, set) in [
        ("status", args.status.is_some()),
        ("tracker", args.tracker.is_some()),
        ("priority", args.priority.is_some()),
        ("category", args.category.is_some()),
        ("assignee", args.assignee.is_some()),
        ("author", args.author.is_some()),
        ("subject", args.subject.is_some()),
        ("created_on", args.created_on.is_some()),
        ("updated_on", args.updated_on.is_some()),
        (
            "custom_fields",
            args.custom_fields
                .as_ref()
                .is_some_and(|map| !map.is_empty()),
        ),
    ] {
        if set {
            ignored.push(name.to_string());
        }
    }
    ignored.extend(args.extra.keys().cloned());
    if ignored.is_empty() {
        return Ok(());
    }
    Err(ToolError::refused(format!(
        "query_id runs the saved query as it is, and Redmine ignores other filters alongside it: {}. Drop query_id to filter by hand, or drop the filters. Allowed with query_id: {}.",
        ignored.join(", "),
        QUERY_ID_COMPANIONS.join(", ")
    )))
}

/// Redmine drops a `cf_N` filter it can't apply: a field that isn't a filter,
/// is hidden from the key's roles, or isn't enabled on the project. The result
/// then looks filtered but isn't.
///
/// Zero rows is right either way, so only a non-empty result is checked. That
/// result proves the scope has issues, so a probe on the same scope with a value
/// no issue has tells the two apart: zero rows means the filter was applied.
/// Answers are cached per field and project for the catalog TTL.
pub(crate) async fn check_applied(
    ctx: &Ctx,
    params: &[(String, String)],
    fields: &[Field],
    total: u64,
) -> Result<(), ToolError> {
    if total == 0 || fields.is_empty() {
        return Ok(());
    }
    let project = params
        .iter()
        .find(|(key, _)| key == "project_id")
        .map_or("*", |(_, value)| value.as_str())
        .to_string();
    for field in fields {
        let applied = match ctx.catalog.filter_applied(field.id, &project).await {
            Some(applied) => applied,
            None => {
                let applied = probe_applied(ctx, params, field.id).await?;
                ctx.catalog
                    .remember_filter_applied(field.id, &project, applied)
                    .await;
                applied
            }
        };
        if !applied {
            let scope = if project == "*" {
                "across the projects this key can see".to_string()
            } else {
                format!("in project {project}")
            };
            return Err(ToolError::refused(format!(
                "Redmine ignored the filter on {} ({}) {scope}, so the result would be unfiltered. The field may not be enabled there, may be hidden from this key, or may not be marked as a filter.",
                field.label, field.id
            )));
        }
    }
    Ok(())
}

async fn probe_applied(
    ctx: &Ctx,
    params: &[(String, String)],
    field_id: u64,
) -> Result<bool, ToolError> {
    let target = format!("cf_{field_id}");
    let mut query = params
        .iter()
        .filter(|(key, _)| key != &target)
        .cloned()
        .collect::<Vec<_>>();
    query.push((target, PROBE_VALUE.to_string()));
    query.push(("limit".to_string(), "1".to_string()));
    let refs = query
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect::<Vec<_>>();
    match ctx.client.get_json::<Value>("/issues.json", &refs).await {
        Ok(body) => Ok(u64_field(&body, "total_count").unwrap_or(0) == 0),
        // Redmine validated the value, so it was reading the filter.
        Err(err) if err.status() == Some(422) => Ok(true),
        Err(err) => Err(err.into()),
    }
}

enum InputKind {
    /// A top-level argument, which must equal a configured key exactly.
    Configured,
    /// An entry in `custom_fields`, resolved by key, `cf_<id>`, label or partial key.
    Map,
}

struct CustomInput {
    key: String,
    value: String,
    kind: InputKind,
}

fn custom_inputs(args: &CallArgs) -> Result<Vec<CustomInput>, ToolError> {
    let mut inputs: Vec<CustomInput> = Vec::new();
    if let Some(fields) = &args.custom_fields {
        for (key, value) in fields {
            if let Some(value) = filter_text(value)? {
                inputs.push(CustomInput {
                    key: key.clone(),
                    value,
                    kind: InputKind::Map,
                });
            }
        }
    }
    for (key, value) in &args.extra {
        if inputs.iter().any(|input| input.key == *key) {
            return Err(ToolError::refused(format!(
                "{key} was passed twice, once as its own parameter and once in custom_fields."
            )));
        }
        if let Some(value) = filter_text(value)? {
            inputs.push(CustomInput {
                key: key.clone(),
                value,
                kind: InputKind::Configured,
            });
        }
    }
    Ok(inputs)
}

/// Every top-level argument must be one this tool advertises. A configured
/// field key counts. Anything else is a typo, or a Redmine parameter name such
/// as `tracker_id`, and fails here instead of being guessed at as a custom field.
fn check_arg_names(
    args: &JsonObject,
    config: &crate::config::Config,
    allowed: &[&[&str]],
) -> Result<(), ToolError> {
    let unknown = args
        .keys()
        .filter(|name| {
            !allowed.iter().any(|set| set.contains(&name.as_str()))
                && !config.fields.contains_key(name.as_str())
        })
        .cloned()
        .collect::<Vec<_>>();
    if unknown.is_empty() {
        return Ok(());
    }
    let mut known = allowed
        .iter()
        .flat_map(|set| set.iter().map(|name| (*name).to_string()))
        .collect::<Vec<_>>();
    known.extend(config.fields.keys().cloned());
    Err(ToolError::refused(format!(
        "Unknown argument{}: {}. Other custom fields go in custom_fields. Arguments: {}.",
        if unknown.len() == 1 { "" } else { "s" },
        unknown.join(", "),
        known.join(", ")
    )))
}

fn check_meta_args(args: &JsonObject) -> Result<(), ToolError> {
    let unknown = args
        .keys()
        .filter(|name| !META_ARGS.contains(&name.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    if unknown.is_empty() {
        return Ok(());
    }
    Err(ToolError::refused(format!(
        "Unknown argument{}: {}. Arguments: {}.",
        if unknown.len() == 1 { "" } else { "s" },
        unknown.join(", "),
        META_ARGS.join(", ")
    )))
}

fn filter_text(value: &Value) -> Result<Option<String>, ToolError> {
    let text = match value {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        Value::Null => return Ok(None),
        _ => {
            return Err(ToolError::refused(
                "Custom field values are strings. Combine several with |.",
            ));
        }
    };
    let text = text.trim().to_string();
    if text.is_empty() {
        Ok(None)
    } else {
        Ok(Some(text))
    }
}

async fn category_param(
    ctx: &Ctx,
    project: Option<&str>,
    input: &str,
) -> Result<String, ToolError> {
    // Ids without a project can't be checked: categories only exist per project.
    if is_id_list(input) && project.is_none() {
        return Ok(input.to_string());
    }
    let Some(project) = project else {
        return Err(ToolError::refused(format!(
            "Category \"{input}\" needs a project_id. Redmine scopes categories per project."
        )));
    };
    let ids = ctx
        .catalog
        .resolve_category(project, input)
        .await
        .map_err(catalog_err)?;
    Ok(join_ids(&ids))
}

async fn person_param(
    ctx: &Ctx,
    project: Option<&str>,
    input: &str,
    allow_none: bool,
) -> Result<String, ToolError> {
    if input.eq_ignore_ascii_case("me") {
        return Ok("me".to_string());
    }
    if input == "*" || input == "!*" {
        return Ok(input.to_string());
    }
    let none = matches!(
        input.to_ascii_lowercase().as_str(),
        "none" | "unassigned" | "nobody"
    );
    if none && allow_none {
        return Ok("!*".to_string());
    }
    if none {
        return Err(ToolError::refused(
            "none is for assignee. An author is always someone.",
        ));
    }
    // Not checked against membership: an issue can stay assigned to someone who
    // has since left the project, and their id is still a valid filter.
    if is_id_list(input) {
        return Ok(input.to_string());
    }
    let Some(project) = project else {
        return Err(ToolError::refused(format!(
            "Cannot resolve \"{input}\" without project_id. Pass project_id, or pass a numeric user id."
        )));
    };
    let ids = ctx
        .catalog
        .resolve_member(project, input)
        .await
        .map_err(catalog_err)?;
    Ok(join_ids(&ids))
}

fn status_param(filter: StatusFilter) -> String {
    match filter {
        StatusFilter::Open => "open".to_string(),
        StatusFilter::Closed => "closed".to_string(),
        StatusFilter::Any => "*".to_string(),
        StatusFilter::Ids(ids) => join_ids(&ids),
    }
}

fn join_ids(ids: &[u64]) -> String {
    ids.iter()
        .map(|id| id.to_string())
        .collect::<Vec<_>>()
        .join("|")
}

fn is_id_list(raw: &str) -> bool {
    !raw.is_empty()
        && raw
            .split('|')
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
}

/// A bare date means `>=` that date. Operators and Redmine's relative tokens pass through.
fn normalize_date(raw: &str) -> String {
    if raw.starts_with("><")
        || raw.starts_with(">=")
        || raw.starts_with("<=")
        || raw.starts_with('>')
        || raw.starts_with('<')
    {
        return raw.to_string();
    }
    if raw.len() >= 10
        && raw.as_bytes()[..4].iter().all(|byte| byte.is_ascii_digit())
        && raw.as_bytes()[4] == b'-'
        && raw.as_bytes()[7] == b'-'
    {
        return format!(">={raw}");
    }
    raw.to_string()
}

fn subject_param(raw: &str) -> String {
    if raw.starts_with('~') {
        raw.to_string()
    } else {
        format!("~{raw}")
    }
}

fn trimmed(value: &Option<String>) -> Option<String> {
    value
        .as_ref()
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
}

fn describe(params: &[(String, String)]) -> String {
    let body = params
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(" ");
    let status_sent = params.iter().any(|(key, _)| key == "status_id");
    let saved_query = params.iter().any(|(key, _)| key == "query_id");
    if body.is_empty() {
        "open issues".to_string()
    } else if status_sent || saved_query {
        // A saved query sets its own status filter.
        body
    } else {
        format!("{body} (open)")
    }
}

async fn value_hint(ctx: &Ctx, resolved: &Resolved, total: u64) -> Result<String, ToolError> {
    if total > 0 || resolved.custom_fields.is_empty() {
        return Ok(String::new());
    }
    match crate::scan::check_values(ctx, &resolved.params, &resolved.custom_fields).await? {
        crate::scan::ValueCheck::Confirmed => Ok(String::new()),
        crate::scan::ValueCheck::Unchecked => {
            let mut hint = zero_hint(0, &resolved.custom_keys);
            if !hint.is_empty() {
                hint.push(' ');
            }
            hint.push_str("Too many issues to check the value; narrow the filters.");
            Ok(hint)
        }
    }
}

fn zero_hint(total: u64, keys: &[String]) -> String {
    if total > 0 || keys.is_empty() {
        return String::new();
    }
    format!(
        "No matches. Redmine returns zero rows for an unrecognised custom field value, so check the spelling of {}.",
        keys.join(", ")
    )
}

fn check_columns(extra: &[String], fields: &[Field]) -> Result<(), ToolError> {
    for name in extra {
        if name == "description" {
            return Err(ToolError::refused(
                "Use include_description to add issue bodies.",
            ));
        }
        if DEFAULT_COLUMNS.contains(&name.as_str()) || EXTRA_COLUMNS.contains(&name.as_str()) {
            continue;
        }
        if fields.iter().any(|field| field.key == *name) {
            continue;
        }
        let mut known = DEFAULT_COLUMNS
            .iter()
            .chain(EXTRA_COLUMNS.iter())
            .map(|name| (*name).to_string())
            .collect::<Vec<_>>();
        known.extend(fields.iter().map(|field| field.key.clone()));
        return Err(ToolError::refused(format!(
            "Unknown column \"{name}\". Columns: {}.",
            known.join(", ")
        )));
    }
    Ok(())
}

fn column_names(rows: &[IssueRow], extra: &[String], with_description: bool) -> Vec<String> {
    let mut columns = DEFAULT_COLUMNS
        .iter()
        .map(|name| (*name).to_string())
        .collect::<Vec<_>>();
    let projects = rows
        .iter()
        .map(|row| row.project.as_str())
        .collect::<BTreeSet<_>>();
    if projects.len() > 1 && !columns.iter().any(|name| name == "project") {
        columns.insert(1, "project".to_string());
    }
    for name in extra {
        if !columns.iter().any(|column| column == name) {
            columns.push(name.clone());
        }
    }
    if with_description && !columns.iter().any(|name| name == "description") {
        columns.push("description".to_string());
    }
    columns
}

fn markdown_cell(row: &IssueRow, column: &str, ctx: &Ctx) -> String {
    match column {
        "subject" => clip(&row.subject, SUBJECT_CLIP),
        "description" => clip_description(&row.description, ctx),
        other => plain_cell(row, other),
    }
}

fn json_cell(row: &IssueRow, column: &str, ctx: &Ctx) -> Value {
    if column == "id" {
        return json!(row.id);
    }
    let text = if column == "description" {
        clip_description(&row.description, ctx)
    } else if column == "subject" {
        row.subject.clone()
    } else {
        plain_cell(row, column)
    };
    if text.is_empty() {
        Value::Null
    } else {
        Value::String(text)
    }
}

fn plain_cell(row: &IssueRow, column: &str) -> String {
    match column {
        "id" => row.id.to_string(),
        "project" => row.project.clone(),
        "tracker" => row.tracker.clone(),
        "status" => row.status.clone(),
        "priority" => row.priority.clone(),
        "assignee" => row.assignee.clone(),
        "author" => row.author.clone(),
        "category" => row.category.clone(),
        "version" => row.version.clone(),
        "created" => row.created.clone(),
        "updated" => row.updated.clone(),
        "closed" => row.closed.clone(),
        "done" => row.done.map(|done| done.to_string()).unwrap_or_default(),
        "subject" => row.subject.clone(),
        "description" => row.description.clone(),
        other => row.fields.get(other).cloned().unwrap_or_default(),
    }
}

/// A list row's description: the smaller of the list cap and the config cap,
/// with a note saying how much was cut.
fn clip_description(text: &str, ctx: &Ctx) -> String {
    let configured =
        usize::try_from(ctx.client.config().max_description_chars).unwrap_or(usize::MAX);
    let max = configured.min(LIST_DESCRIPTION_CHARS);
    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    let kept = text.chars().take(max).collect::<String>();
    format!(
        "{kept}\n[truncated, {} chars omitted; get_issue view=description continues it]",
        count - max
    )
}

struct ListView<'a> {
    rows: &'a [IssueRow],
    total: Option<u64>,
    offset: u64,
    limit: u64,
    extra: &'a [String],
    with_description: bool,
    echoes: &'a [String],
    hint: &'a str,
}

fn render_list(view: &ListView<'_>, ctx: &Ctx) -> String {
    if view.rows.is_empty() {
        let mut out = if view.offset > 0 {
            match view.total {
                Some(total) if total > 0 => {
                    format!(
                        "No issues at offset {}. There are {total} in total.",
                        view.offset
                    )
                }
                _ => format!("No issues at offset {}.", view.offset),
            }
        } else {
            "No issues matched.".to_string()
        };
        push_notes(&mut out, view.echoes, view.hint);
        return out;
    }
    let columns = column_names(view.rows, view.extra, false);
    let headers = columns.iter().map(String::as_str).collect::<Vec<_>>();
    let table_rows = view
        .rows
        .iter()
        .map(|row| {
            columns
                .iter()
                .map(|column| markdown_cell(row, column, ctx))
                .collect()
        })
        .collect::<Vec<_>>();
    let mut out = format!(
        "{}\n\n{}",
        page_heading(
            "Issues",
            view.offset,
            view.rows.len(),
            view.total,
            view.limit
        ),
        markdown_table(&headers, &table_rows)
    );
    if view.with_description {
        for row in view.rows {
            let body = clip_description(&row.description, ctx);
            if body.is_empty() {
                continue;
            }
            out.push_str(&format!("\n\n# {}\n\n{}", row.id, fenced(&body, "")));
        }
    }
    push_notes(&mut out, view.echoes, view.hint);
    out
}

fn push_notes(out: &mut String, echoes: &[String], hint: &str) {
    if !echoes.is_empty() {
        out.push_str("\n\n");
        out.push_str(&echoes.join("\n"));
    }
    if !hint.is_empty() {
        out.push_str("\n\n");
        out.push_str(hint);
    }
}

fn wanted_sections(
    sections: Option<&[String]>,
    project: Option<&str>,
) -> Result<BTreeSet<String>, ToolError> {
    let explicit = sections.is_some();
    let names = match sections {
        Some(names) => names.to_vec(),
        None => DEFAULT_SECTIONS
            .iter()
            .map(|name| (*name).to_string())
            .collect(),
    };
    let mut want = BTreeSet::new();
    for name in names {
        if !META_SECTIONS.contains(&name.as_str()) {
            return Err(ToolError::refused(format!(
                "Unknown section \"{name}\". Sections: {}.",
                META_SECTIONS.join(", ")
            )));
        }
        if matches!(name.as_str(), "categories" | "members") && project.is_none() {
            if explicit {
                return Err(ToolError::refused(format!("{name} needs a project_id.")));
            }
            continue;
        }
        want.insert(name.clone());
    }
    Ok(want)
}

fn named_json(items: &[crate::catalog::Named]) -> Value {
    Value::Array(
        items
            .iter()
            .map(|item| json!({ "id": item.id, "name": item.name }))
            .collect(),
    )
}

fn meta_notes(snapshot: &Snapshot) -> Vec<String> {
    let mut groups = vec!["open".to_string(), "closed".to_string(), "*".to_string()];
    groups.extend(snapshot.groups.keys().cloned());
    let mut notes = vec![
        format!(
            "Status groups: {}. A name with no colon matches every status that starts with that name and a colon. Combine values with |.",
            groups.join(", ")
        ),
        "This list does not include the values in use. list_issues and count_issues check a custom field value when the result is zero, and name the values in use if nothing has it.".to_string(),
    ];
    if matches!(
        snapshot.field_source,
        FieldSource::ProjectInclude | FieldSource::TrackerSample
    ) {
        notes.push(
            "A field can be listed here and still be unusable as a filter: hidden from this key, not enabled on the project, or not marked as a filter. Redmine ignores such a filter, so list_issues and count_issues check and refuse rather than return unfiltered results.".to_string(),
        );
    }
    if snapshot.field_source == FieldSource::TrackerSample
        && !snapshot.unsampled_trackers.is_empty()
    {
        let names = snapshot
            .unsampled_trackers
            .iter()
            .map(|tracker| format!("{} ({})", tracker.name, tracker.id))
            .collect::<Vec<_>>()
            .join(", ");
        notes.push(format!(
            "Trackers with no visible issue, so their fields may be missing: {names}."
        ));
    }
    notes
}

fn render_meta(out: &Map<String, Value>, notes: &[String]) -> String {
    let mut blocks = Vec::new();
    for section in META_SECTIONS {
        if *section == "custom_fields" {
            continue;
        }
        let Some(value) = out.get(*section) else {
            continue;
        };
        let items = value.as_array().map(Vec::as_slice).unwrap_or(&[]);
        let line = if items.is_empty() {
            "_none_".to_string()
        } else {
            items.iter().map(meta_item).collect::<Vec<_>>().join(" · ")
        };
        blocks.push(format!("## {section} ({})\n\n{line}", items.len()));
    }
    if let Some(fields) = out.get("custom_fields").and_then(Value::as_array) {
        let headers = ["key", "id", "label", "multiple", "filter"];
        let rows = fields
            .iter()
            .map(|field| {
                vec![
                    text_at(field, "key"),
                    text_at(field, "id"),
                    text_at(field, "label"),
                    if field.get("multiple").and_then(Value::as_bool) == Some(true) {
                        "yes".to_string()
                    } else {
                        String::new()
                    },
                    if field.get("filter").and_then(Value::as_bool) == Some(false) {
                        "no".to_string()
                    } else {
                        String::new()
                    },
                ]
            })
            .collect::<Vec<_>>();
        blocks.push(format!(
            "## custom fields ({})\n\n{}",
            fields.len(),
            markdown_table(&headers, &rows)
        ));
    }
    for note in notes {
        blocks.push(format!("_{note}_"));
    }
    blocks.join("\n\n")
}

fn meta_item(value: &Value) -> String {
    let name = text_at(value, "name");
    let id = text_at(value, "id");
    let mut extra = Vec::new();
    if value.get("is_closed").and_then(Value::as_bool) == Some(true) {
        extra.push("closed".to_string());
    }
    if let Some(kind) = value.get("kind").and_then(Value::as_str) {
        extra.push(kind.to_string());
    }
    if extra.is_empty() {
        format!("{name} ({id})")
    } else {
        format!("{name} ({id}, {})", extra.join(", "))
    }
}

fn text_at(value: &Value, key: &str) -> String {
    match value.get(key) {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Number(number)) => number.to_string(),
        _ => String::new(),
    }
}

fn page_fields(total: Option<u64>, offset: u64, limit: u64) -> Map<String, Value> {
    let mut page = Map::new();
    if let Some(total) = total {
        page.insert("total_count".to_string(), json!(total));
    }
    page.insert("offset".to_string(), json!(offset));
    page.insert("limit".to_string(), json!(limit));
    page
}

pub(crate) async fn get_json(
    client: &crate::client::Client,
    path: &str,
    query: &[(String, String)],
) -> Result<Value, ToolError> {
    let refs = query
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect::<Vec<_>>();
    let body = sanitize(client.get_json(path, &refs).await?);
    if !body.is_object() {
        return Err(ToolError::refused(format!(
            "Unexpected response from {path}: expected an object"
        )));
    }
    Ok(body)
}

pub(crate) fn array_field<'a>(body: &'a Value, key: &str) -> Result<&'a [Value], ToolError> {
    match body.get(key) {
        None | Some(Value::Null) => Ok(&[]),
        Some(Value::Array(items)) => Ok(items),
        Some(_) => Err(ToolError::refused(format!(
            "Redmine returned {key} in an unexpected shape"
        ))),
    }
}

pub(crate) fn u64_field(value: &Value, key: &str) -> Option<u64> {
    value.get(key).and_then(Value::as_u64)
}

pub(crate) fn catalog_err(err: CatalogError) -> ToolError {
    match err {
        CatalogError::Redmine(inner) => ToolError::Redmine(inner),
        other => ToolError::Refused(other.to_string()),
    }
}

fn list_schema(config: &crate::config::Config, with_page: bool) -> Arc<JsonObject> {
    let mut properties = filter_properties(config, true);
    if with_page {
        properties.insert(
            "include_fields".to_string(),
            json!({
                "type": "array",
                "items": { "type": "string" },
                "description": "Extra columns. Built-ins beyond the default set: project, author, category, created, closed, version, done. A custom field key adds that field. Default columns are id, tracker, status, priority, assignee, updated, subject."
            }),
        );
        properties.insert(
            "include_description".to_string(),
            json!({
                "type": "boolean",
                "description": "Include each description, still capped. Leave this off unless you are reading bodies."
            }),
        );
        properties.insert(
            "limit".to_string(),
            json!({
                "type": "integer",
                "description": "Page size. Default 25, capped by REDMINE_MAX_LIST."
            }),
        );
        properties.insert(
            "offset".to_string(),
            json!({
                "type": "integer",
                "description": "Offset for paging. Default 0."
            }),
        );
        properties.insert(
            "format".to_string(),
            json!({
                "type": "string",
                "enum": ["markdown", "json"],
                "description": "Output format. Default markdown."
            }),
        );
    }
    Arc::new(json_object(json!({
        "type": "object",
        "properties": properties
    })))
}

fn filter_properties(config: &crate::config::Config, with_sort: bool) -> Map<String, Value> {
    let groups = config
        .status_groups
        .keys()
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    let group_line = if groups.is_empty() {
        String::new()
    } else {
        format!(" Configured groups: {groups}.")
    };
    let mut properties = Map::new();
    properties.insert(
        "project_id".to_string(),
        json!({
            "description": "Project id or identifier. Required when filtering by a category name or a person's name.",
            "type": ["string", "integer"]
        }),
    );
    properties.insert(
        "status".to_string(),
        json!({
            "type": "string",
            "description": format!(
                "Status name, id, open, closed, or *. A name with no colon matches every status that starts with that name and a colon, such as qa for QA: Testing.{group_line} Combine with |. Omit for Redmine's default, which is open."
            )
        }),
    );
    properties.insert(
        "tracker".to_string(),
        json!({"type": "string", "description": "Tracker name or id. Combine with |."}),
    );
    properties.insert(
        "priority".to_string(),
        json!({"type": "string", "description": "Priority name or id. Combine with |."}),
    );
    properties.insert(
        "category".to_string(),
        json!({"type": "string", "description": "Category name or id. A name needs project_id."}),
    );
    properties.insert(
        "assignee".to_string(),
        json!({"type": "string", "description": "Assignee name, id, me, or none. A name needs project_id."}),
    );
    properties.insert(
        "author".to_string(),
        json!({"type": "string", "description": "Author name, id, or me. A name needs project_id."}),
    );
    properties.insert(
        "subject".to_string(),
        json!({"type": "string", "description": "Substring of the subject. A leading ~ is sent as Redmine's contains operator."}),
    );
    properties.insert(
        "created_on".to_string(),
        json!({"type": "string", "description": "Date filter. A bare YYYY-MM-DD means >= that date. >=, <=, >t- and similar operators pass through."}),
    );
    properties.insert(
        "updated_on".to_string(),
        json!({"type": "string", "description": "Date filter, same forms as created_on."}),
    );
    properties.insert(
        "query_id".to_string(),
        json!({"type": "integer", "description": "Saved query id from list_meta."}),
    );
    if with_sort {
        properties.insert(
            "sort".to_string(),
            json!({"type": "string", "description": "Redmine sort. Default updated_on:desc. Ignored by count_issues."}),
        );
    }
    properties.insert(
        "custom_fields".to_string(),
        json!({
            "type": "object",
            "additionalProperties": { "type": "string" },
            "description": "Other custom fields, keyed by the key from list_meta. A value no issue in scope has is an error listing the values in use."
        }),
    );
    for key in config.fields.keys() {
        properties.insert(
            key.clone(),
            json!({
                "type": "string",
                "description": format!("Filter on the configured custom field {key}. A value no issue in scope has is an error listing the values in use.")
            }),
        );
    }
    properties
}

fn facet_schema(config: &crate::config::Config) -> Arc<JsonObject> {
    let mut properties = filter_properties(config, false);
    let mut dimensions = crate::scan::builtin_names()
        .iter()
        .map(|name| (*name).to_string())
        .collect::<Vec<_>>();
    for key in config.fields.keys() {
        if !dimensions.iter().any(|name| name == key) {
            dimensions.push(key.clone());
        }
    }
    properties.insert(
        "group_by".to_string(),
        json!({
            "type": "array",
            "minItems": 1,
            "maxItems": 2,
            "items": { "type": "string", "enum": dimensions },
            "description": "One dimension for a histogram, two for a cross-tab. The enum lists built-ins and configured field keys. Any other custom field key is accepted and checked against the catalog."
        }),
    );
    properties.insert(
        "top".to_string(),
        json!({
            "type": "integer",
            "description": "Keep the N largest groups on each axis. Default 25. The rest roll into (other)."
        }),
    );
    properties.insert(
        "format".to_string(),
        json!({
            "type": "string",
            "enum": ["markdown", "json"],
            "description": "Output format. Default markdown."
        }),
    );
    Arc::new(json_object(json!({
        "type": "object",
        "properties": properties,
        "required": ["group_by"]
    })))
}

fn meta_schema() -> Arc<JsonObject> {
    Arc::new(json_object(json!({
        "type": "object",
        "properties": {
            "project_id": {
                "description": "Include this project's categories. Members too, when sections asks for them.",
                "type": ["string", "integer"]
            },
            "sections": {
                "type": "array",
                "items": {
                    "type": "string",
                    "enum": META_SECTIONS
                },
                "description": "Limit the output. Default is everything except members."
            },
            "format": {
                "type": "string",
                "enum": ["markdown", "json"],
                "description": "Output format. Default markdown."
            }
        }
    })))
}

fn json_object(value: Value) -> JsonObject {
    match value {
        Value::Object(map) => map,
        _ => JsonObject::new(),
    }
}

fn cap_limit(requested: Option<u64>, hard_max: u64) -> u64 {
    match requested {
        Some(value) if value > 0 => value.min(hard_max),
        _ => PAGE_DEFAULT.min(hard_max),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use serde_json::json;
    use wiremock::matchers::{method, path, query_param, query_param_is_missing};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::catalog::Ctx;
    use crate::client::Client;
    use crate::config::{Config, load_from};

    fn object(value: Value) -> JsonObject {
        match value {
            Value::Object(map) => map,
            _ => panic!("expected an object"),
        }
    }

    fn config_with(file: &str) -> Config {
        let env = HashMap::from([
            ("REDMINE_URL".to_string(), "http://example.test".to_string()),
            ("REDMINE_API_KEY".to_string(), "test-key".to_string()),
            (
                "REDMINE_CONFIG".to_string(),
                "/etc/redmine.json".to_string(),
            ),
        ]);
        let file = file.to_string();
        load_from(&env, move |_| Ok(file.clone())).unwrap()
    }

    fn client_with_file(base: &str, file: &str) -> Client {
        let env = HashMap::from([
            ("REDMINE_URL".to_string(), base.to_string()),
            ("REDMINE_API_KEY".to_string(), "test-key".to_string()),
            (
                "REDMINE_CONFIG".to_string(),
                "/etc/redmine.json".to_string(),
            ),
        ]);
        let file = file.to_string();
        Client::new(load_from(&env, move |_| Ok(file.clone())).unwrap()).unwrap()
    }

    fn client_tuned(base: &str, file: &str, max_scan: &str) -> Client {
        let env = HashMap::from([
            ("REDMINE_URL".to_string(), base.to_string()),
            ("REDMINE_API_KEY".to_string(), "test-key".to_string()),
            (
                "REDMINE_CONFIG".to_string(),
                "/etc/redmine.json".to_string(),
            ),
            ("REDMINE_MAX_SCAN".to_string(), max_scan.to_string()),
        ]);
        let file = file.to_string();
        Client::new(load_from(&env, move |_| Ok(file.clone())).unwrap()).unwrap()
    }

    async fn mount_json(server: &MockServer, at: &str, body: Value) {
        Mock::given(method("GET"))
            .and(path(at))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .expect(1)
            .mount(server)
            .await;
    }

    async fn mount_catalog(server: &MockServer) {
        mount_json(
            server,
            "/issue_statuses.json",
            json!({ "issue_statuses": [
                { "id": 1, "name": "New", "is_closed": false },
                { "id": 3, "name": "Review", "is_closed": false },
                { "id": 7, "name": "Closed", "is_closed": true }
            ]}),
        )
        .await;
        mount_json(
            server,
            "/trackers.json",
            json!({ "trackers": [{ "id": 1, "name": "Bug" }] }),
        )
        .await;
        mount_json(
            server,
            "/enumerations/issue_priorities.json",
            json!({ "issue_priorities": [{ "id": 2, "name": "Normal" }] }),
        )
        .await;
        mount_json(
            server,
            "/queries.json",
            json!({ "queries": [], "total_count": 0 }),
        )
        .await;
        mount_json(
            server,
            "/custom_fields.json",
            json!({ "custom_fields": [
                { "id": 36, "name": "Severity Rating", "customized_type": "issue", "multiple": true },
                { "id": 40, "name": "Severity", "customized_type": "issue" }
            ]}),
        )
        .await;
    }

    #[test]
    fn a_bare_date_means_on_or_after_and_a_subject_is_a_contains_match() {
        assert_eq!(normalize_date("2026-08-01"), ">=2026-08-01");
        assert_eq!(normalize_date(">=2026-08-01"), ">=2026-08-01");
        assert_eq!(
            normalize_date("><2026-01-01|2026-06-30"),
            "><2026-01-01|2026-06-30"
        );
        assert_eq!(normalize_date("t"), "t");
        assert_eq!(subject_param("hose"), "~hose");
        assert_eq!(subject_param("~hose"), "~hose");
    }

    #[test]
    fn configured_field_keys_are_parameters() {
        let config =
            config_with(r#"{"fields":{"severity":36},"status_groups":{"blocked":["Closed"]}}"#);
        let list = filter_tool(&config, "list_issues").unwrap();
        let schema = list.schema_as_json_value();
        assert!(schema["properties"]["severity"].is_object(), "{schema}");
        assert!(schema["properties"].get("product").is_none(), "{schema}");
        assert!(
            schema["properties"]["include_fields"].is_object(),
            "{schema}"
        );
        let status = schema["properties"]["status"]["description"]
            .as_str()
            .unwrap();
        assert!(status.contains("blocked"), "{status}");
        let count = filter_tool(&config, "count_issues").unwrap();
        let schema = count.schema_as_json_value();
        assert!(
            schema["properties"].get("include_fields").is_none(),
            "{schema}"
        );
        assert!(schema["properties"]["severity"].is_object(), "{schema}");
    }

    #[test]
    fn facet_group_by_enum_is_builtins_plus_configured_keys() {
        let config =
            config_with(r#"{"fields":{"severity":36},"status_groups":{"blocked":["Closed"]}}"#);
        let facet = filter_tool(&config, "facet_issues").unwrap();
        let schema = facet.schema_as_json_value();
        let names = schema["properties"]["group_by"]["items"]["enum"]
            .as_array()
            .unwrap();
        for name in [
            "status", "tracker", "priority", "assignee", "author", "category", "version",
            "project", "severity",
        ] {
            assert!(
                names.iter().any(|value| value.as_str() == Some(name)),
                "{schema}"
            );
        }
        assert!(schema["properties"].get("sort").is_none(), "{schema}");
        assert!(schema["properties"].get("limit").is_none(), "{schema}");
        assert!(
            schema["required"]
                .as_array()
                .unwrap()
                .contains(&json!("group_by"))
        );

        let bare = filter_tool(&config_with("{}"), "facet_issues").unwrap();
        let schema = bare.schema_as_json_value();
        let names = schema["properties"]["group_by"]["items"]["enum"]
            .as_array()
            .unwrap();
        assert!(
            names.iter().any(|value| value.as_str() == Some("version")),
            "{schema}"
        );
        assert!(
            !names
                .iter()
                .any(|value| value.as_str() == Some("severity_rating")),
            "{schema}"
        );
    }

    #[tokio::test]
    async fn list_issues_uses_the_configured_key_when_the_label_collides() {
        let server = MockServer::start().await;
        mount_catalog(&server).await;
        Mock::given(method("GET"))
            .and(path("/issues.json"))
            .and(query_param("status_id", "3"))
            .and(query_param("cf_36", "Minor"))
            .and(query_param("created_on", ">=2026-08-01"))
            .and(query_param("subject", "~hose"))
            .and(query_param("sort", "updated_on:desc"))
            .and(query_param("limit", "25"))
            .and(query_param("offset", "0"))
            .and(query_param_is_missing("cf_40"))
            .and(query_param_is_missing("project_id"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "issues": [{
                    "id": 9,
                    "project": { "id": 1, "name": "Garden" },
                    "tracker": { "id": 1, "name": "Bug" },
                    "status": { "id": 3, "name": "Review" },
                    "priority": { "id": 2, "name": "Normal" },
                    "assigned_to": { "id": 4, "name": "Ada" },
                    "subject": "Hose split",
                    "updated_on": "2026-03-04T05:06:07Z",
                    "custom_fields": [
                        { "id": 36, "name": "Severity Rating", "value": "Minor" },
                        { "id": 40, "name": "Severity", "value": "Other" }
                    ]
                }],
                "total_count": 1
            })))
            .expect(1)
            .mount(&server)
            .await;

        // The row proves the scope has issues, so one probe checks that Redmine
        // applied the filter.
        mount_probe(&server, 36, 0, 1).await;
        let ctx = Ctx::new(client_with_file(
            &server.uri(),
            r#"{"fields":{"severity":36}}"#,
        ));
        let args = object(json!({
            "status": "Review",
            "custom_fields": { "Severity": "Minor" },
            "created_on": "2026-08-01",
            "subject": "hose",
            "include_fields": ["severity"]
        }));
        let text = list_issues(&ctx, &args).await.unwrap();
        assert!(text.contains("| severity |"), "{text}");
        assert!(text.contains("| Minor |"), "{text}");
        assert!(text.contains("Severity -> Severity Rating (36)"), "{text}");
        assert!(!text.contains("Other"), "{text}");
        assert!(!text.contains("Field names unavailable"), "{text}");
    }

    /// The probe: same filters, the field set to the probe value, limit 1.
    async fn mount_probe(server: &MockServer, field: u64, total: u64, times: u64) {
        Mock::given(method("GET"))
            .and(path("/issues.json"))
            .and(query_param(format!("cf_{field}"), PROBE_VALUE))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "issues": [],
                "total_count": total
            })))
            .expect(times)
            .with_priority(1)
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn an_unknown_status_fails_before_the_issue_request() {
        let server = MockServer::start().await;
        mount_catalog(&server).await;
        Mock::given(method("GET"))
            .and(path("/issues.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "issues": [],
                "total_count": 0
            })))
            .expect(0)
            .mount(&server)
            .await;
        let ctx = Ctx::new(client_with_file(
            &server.uri(),
            r#"{"fields":{"severity":36}}"#,
        ));
        let err = list_issues(&ctx, &object(json!({ "status": "Nope" })))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("Known statuses"), "{err}");
        assert!(err.contains("Review"), "{err}");
    }

    #[tokio::test]
    async fn count_issues_is_one_request_and_defaults_to_open() {
        let server = MockServer::start().await;
        mount_catalog(&server).await;
        Mock::given(method("GET"))
            .and(path("/issues.json"))
            .and(query_param("limit", "1"))
            .and(query_param_is_missing("status_id"))
            .and(query_param_is_missing("sort"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "issues": [{ "id": 1 }],
                "total_count": 4
            })))
            .expect(1)
            .mount(&server)
            .await;
        let ctx = Ctx::new(client_with_file(&server.uri(), "{}"));
        let text = count_issues(&ctx, &object(json!({}))).await.unwrap();
        assert_eq!(text, "4 issues match open issues.");
    }

    #[tokio::test]
    async fn open_and_closed_counts_every_status() {
        let server = MockServer::start().await;
        mount_catalog(&server).await;
        Mock::given(method("GET"))
            .and(path("/issues.json"))
            .and(query_param("status_id", "*"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "issues": [],
                "total_count": 9
            })))
            .expect(1)
            .mount(&server)
            .await;
        let ctx = Ctx::new(client_with_file(&server.uri(), "{}"));
        let text = count_issues(&ctx, &object(json!({ "status": "open|closed" })))
            .await
            .unwrap();
        assert!(text.starts_with("9 issues match status_id=*"), "{text}");
    }

    #[tokio::test]
    async fn assignee_names_need_a_project_and_me_does_not() {
        let server = MockServer::start().await;
        mount_catalog(&server).await;
        Mock::given(method("GET"))
            .and(path("/issues.json"))
            .and(query_param("assigned_to_id", "me"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "issues": [],
                "total_count": 2
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/projects/12/memberships.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "memberships": [{ "user": { "id": 4, "name": "Robin Example" } }],
                "total_count": 1
            })))
            .expect(0)
            .mount(&server)
            .await;
        let ctx = Ctx::new(client_with_file(&server.uri(), "{}"));
        let text = count_issues(&ctx, &object(json!({ "assignee": "me" })))
            .await
            .unwrap();
        assert!(text.contains("assigned_to_id=me"), "{text}");
        let err = count_issues(&ctx, &object(json!({ "assignee": "Robin" })))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("project_id"), "{err}");
    }

    #[tokio::test]
    async fn an_assignee_name_resolves_inside_the_project() {
        let server = MockServer::start().await;
        mount_catalog(&server).await;
        Mock::given(method("GET"))
            .and(path("/projects/12/memberships.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "memberships": [{ "user": { "id": 4, "name": "Robin Example" } }],
                "total_count": 1
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/issues.json"))
            .and(query_param("project_id", "12"))
            .and(query_param("assigned_to_id", "4"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "issues": [],
                "total_count": 1
            })))
            .expect(1)
            .mount(&server)
            .await;
        let ctx = Ctx::new(client_with_file(&server.uri(), "{}"));
        let text = count_issues(
            &ctx,
            &object(json!({ "project_id": 12, "assignee": "Robin" })),
        )
        .await
        .unwrap();
        assert!(text.contains("assigned_to_id=4"), "{text}");
    }

    /// A count answer for any request that isn't the probe.
    async fn mount_count(server: &MockServer, total: u64, times: u64) {
        Mock::given(method("GET"))
            .and(path("/issues.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "issues": [],
                "total_count": total
            })))
            .expect(times)
            .with_priority(5)
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn an_ignored_custom_field_filter_is_refused_not_returned_unfiltered() {
        let server = MockServer::start().await;
        mount_catalog(&server).await;
        mount_count(&server, 40, 1).await;
        // The probe value matches nothing, yet rows come back: Redmine dropped the filter.
        mount_probe(&server, 40, 40, 1).await;
        let ctx = Ctx::new(client_with_file(&server.uri(), "{}"));
        let err = count_issues(
            &ctx,
            &object(json!({ "project_id": "garden", "custom_fields": { "cf_40": "High" } })),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("Redmine ignored the filter on Severity (40)"),
            "{err}"
        );
        assert!(err.contains("in project garden"), "{err}");
    }

    #[tokio::test]
    async fn the_applied_check_is_cached_and_zero_rows_need_no_probe() {
        let server = MockServer::start().await;
        mount_catalog(&server).await;
        mount_count(&server, 3, 2).await;
        mount_probe(&server, 40, 0, 1).await;
        let ctx = Ctx::new(client_with_file(&server.uri(), "{}"));
        let args = object(json!({ "custom_fields": { "cf_40": "High" } }));
        for _ in 0..2 {
            let text = count_issues(&ctx, &args).await.unwrap();
            assert!(text.starts_with("3 issues match"), "{text}");
        }

        // Zero rows skip the probe. The value check scans the scope the other
        // filters define, finds it empty, and leaves the zero alone: nothing in
        // that scope could have matched any value.
        let quiet = MockServer::start().await;
        mount_catalog(&quiet).await;
        mount_count(&quiet, 0, 2).await;
        mount_probe(&quiet, 40, 0, 0).await;
        let ctx = Ctx::new(client_with_file(&quiet.uri(), "{}"));
        let text = count_issues(&ctx, &args).await.unwrap();
        assert!(text.starts_with("0 issues match"), "{text}");
        assert!(!text.contains("Values in use"), "{text}");
        assert!(!text.contains("check the spelling"), "{text}");
    }

    #[tokio::test]
    async fn a_typo_in_one_field_is_blamed_on_that_field_only() {
        let server = MockServer::start().await;
        mount_catalog(&server).await;
        let issue = |id: u64, severity: &str, rating: &str| {
            json!({ "id": id, "custom_fields": [
                { "id": 40, "name": "Severity", "value": severity },
                { "id": 36, "name": "Severity Rating", "value": rating }
            ]})
        };
        // The filtered count: both fields set, so zero.
        Mock::given(method("GET"))
            .and(path("/issues.json"))
            .and(query_param("cf_40", "High"))
            .and(query_param("cf_36", "Minr"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "issues": [],
                "total_count": 0
            })))
            .expect(1)
            .mount(&server)
            .await;
        // The value check: one scan with neither field filter.
        Mock::given(method("GET"))
            .and(path("/issues.json"))
            .and(query_param_is_missing("cf_40"))
            .and(query_param_is_missing("cf_36"))
            .and(query_param("sort", "id:asc"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "issues": [issue(1, "High", "Minor"), issue(2, "Low", "Major")],
                "total_count": 2
            })))
            .expect(1)
            .mount(&server)
            .await;
        let ctx = Ctx::new(client_with_file(&server.uri(), "{}"));
        let err = count_issues(
            &ctx,
            &object(json!({ "custom_fields": { "cf_40": "High", "cf_36": "Minr" } })),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("Severity Rating = \"Minr\""), "{err}");
        assert!(err.contains("Values in use: Major (1), Minor (1)"), "{err}");
        assert!(!err.contains("Severity = \"High\""), "{err}");

        // Both misspelled: both named, in one error.
        let server = MockServer::start().await;
        mount_catalog(&server).await;
        Mock::given(method("GET"))
            .and(path("/issues.json"))
            .and(query_param("cf_40", "Hgh"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "issues": [],
                "total_count": 0
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/issues.json"))
            .and(query_param_is_missing("cf_40"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "issues": [issue(1, "High", "Minor")],
                "total_count": 1
            })))
            .mount(&server)
            .await;
        let ctx = Ctx::new(client_with_file(&server.uri(), "{}"));
        let err = count_issues(
            &ctx,
            &object(json!({ "custom_fields": { "cf_40": "Hgh", "cf_36": "Minr" } })),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("Severity = \"Hgh\""), "{err}");
        assert!(err.contains("Severity Rating = \"Minr\""), "{err}");
    }

    #[tokio::test]
    async fn a_field_the_admin_endpoint_marks_unfilterable_is_refused_before_any_request() {
        let server = MockServer::start().await;
        for (at, body) in [
            (
                "/issue_statuses.json",
                json!({ "issue_statuses": [{ "id": 1, "name": "New", "is_closed": false }] }),
            ),
            ("/trackers.json", json!({ "trackers": [] })),
            (
                "/enumerations/issue_priorities.json",
                json!({ "issue_priorities": [] }),
            ),
            ("/queries.json", json!({ "queries": [], "total_count": 0 })),
            (
                "/custom_fields.json",
                json!({ "custom_fields": [
                    { "id": 5, "name": "Internal notes", "customized_type": "issue", "is_filter": false, "visible": true },
                    { "id": 6, "name": "Cost centre", "customized_type": "issue", "is_filter": true, "visible": false }
                ]}),
            ),
        ] {
            mount_json(&server, at, body).await;
        }
        mount_count(&server, 1, 0).await;
        let ctx = Ctx::new(client_with_file(&server.uri(), "{}"));
        for key in ["internal_notes", "cost_centre"] {
            let err = count_issues(&ctx, &object(json!({ "custom_fields": { key: "x" } })))
                .await
                .unwrap_err()
                .to_string();
            assert!(err.contains("not available as a filter"), "{key}: {err}");
        }
        let meta = list_meta(&ctx, &object(json!({ "sections": ["custom_fields"] })))
            .await
            .unwrap();
        assert!(
            meta.contains("| internal_notes | 5 | Internal notes |  | no |"),
            "{meta}"
        );
    }

    #[tokio::test]
    async fn query_id_refuses_filters_it_would_ignore() {
        let server = MockServer::start().await;
        mount_catalog(&server).await;
        mount_count(&server, 7, 1).await;
        let ctx = Ctx::new(client_with_file(
            &server.uri(),
            r#"{"fields":{"severity":36}}"#,
        ));
        let err = count_issues(
            &ctx,
            &object(json!({ "query_id": 5, "status": "New", "severity": "Minor" })),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("Redmine ignores other filters"), "{err}");
        assert!(err.contains("status, severity"), "{err}");

        let text = count_issues(
            &ctx,
            &object(json!({ "query_id": 5, "project_id": "garden" })),
        )
        .await
        .unwrap();
        assert!(text.contains("query_id=5"), "{text}");
        assert!(!text.contains("(open)"), "{text}");
    }

    #[tokio::test]
    async fn unknown_arguments_fail_without_a_catalog_reload() {
        let server = MockServer::start().await;
        // No catalog mocks: an unknown argument must fail before the catalog loads.
        mount_count(&server, 1, 0).await;
        let ctx = Ctx::new(client_with_file(
            &server.uri(),
            r#"{"fields":{"severity":36}}"#,
        ));
        let err = count_issues(&ctx, &object(json!({ "tracker_id": 3 })))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("Unknown argument: tracker_id"), "{err}");
        assert!(err.contains("custom_fields"), "{err}");
        assert!(err.contains("severity"), "{err}");

        // count_issues has no page or format arguments.
        let err = count_issues(&ctx, &object(json!({ "format": "json", "limit": 5 })))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("Unknown arguments: format, limit"), "{err}");

        let err = list_issues(&ctx, &object(json!({ "sections": ["statuses"] })))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("Unknown argument: sections"), "{err}");

        let err = list_meta(&ctx, &object(json!({ "status": "New" })))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("Unknown argument: status"), "{err}");
    }

    #[tokio::test]
    async fn a_category_id_is_checked_when_the_project_is_known() {
        let server = MockServer::start().await;
        mount_catalog(&server).await;
        Mock::given(method("GET"))
            .and(path("/projects/garden/issue_categories.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "issue_categories": [{ "id": 38, "name": "Irrigation" }]
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/issues.json"))
            .and(query_param("category_id", "38"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "issues": [],
                "total_count": 2
            })))
            .expect(1)
            .mount(&server)
            .await;
        let ctx = Ctx::new(client_with_file(&server.uri(), "{}"));
        let text = count_issues(
            &ctx,
            &object(json!({ "project_id": "garden", "category": "38" })),
        )
        .await
        .unwrap();
        assert!(text.starts_with("2 issues"), "{text}");
        let err = count_issues(
            &ctx,
            &object(json!({ "project_id": "garden", "category": "99" })),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("Unknown category \"99\""), "{err}");
        assert!(err.contains("Irrigation"), "{err}");
    }

    #[tokio::test]
    async fn list_descriptions_have_a_list_sized_cap() {
        let server = MockServer::start().await;
        mount_catalog(&server).await;
        let issues = (1..=25)
            .map(|id| {
                let mut issue = full_issue(id);
                issue["description"] = json!("y".repeat(20_000));
                issue
            })
            .collect::<Vec<_>>();
        Mock::given(method("GET"))
            .and(path("/issues.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "issues": issues,
                "total_count": 25
            })))
            .expect(1)
            .mount(&server)
            .await;
        let ctx = Ctx::new(client_with_file(&server.uri(), "{}"));
        let text = list_issues(&ctx, &object(json!({ "include_description": true })))
            .await
            .unwrap();
        assert!(
            text.contains(
                "[truncated, 19000 chars omitted; get_issue view=description continues it]"
            ),
            "{}",
            &text[..400]
        );
        assert_budget(
            "list_issues markdown, 25 rows with descriptions",
            &text,
            31_900,
        );
    }

    #[tokio::test]
    async fn list_meta_names_fields_and_points_value_checks_at_list_and_count() {
        let server = MockServer::start().await;
        mount_catalog(&server).await;
        let ctx = Ctx::new(client_with_file(&server.uri(), "{}"));
        let text = list_meta(&ctx, &object(json!({}))).await.unwrap();
        assert!(text.contains("Severity Rating"), "{text}");
        assert!(text.contains("| severity_rating | 36 |"), "{text}");
        assert!(text.contains("| yes |"), "{text}");
        assert!(text.contains("values in use"), "{text}");
        assert!(!text.contains("hidden from this key"), "{text}");
        assert!(text.contains("Review (3)"), "{text}");
        assert!(text.contains("Closed (7, closed)"), "{text}");
    }

    #[tokio::test]
    async fn list_meta_marks_fields_the_key_may_not_see() {
        let server = MockServer::start().await;
        mount_json(
            &server,
            "/issue_statuses.json",
            json!({ "issue_statuses": [{ "id": 1, "name": "New", "is_closed": false }] }),
        )
        .await;
        mount_json(
            &server,
            "/trackers.json",
            json!({ "trackers": [{ "id": 1, "name": "Bug" }] }),
        )
        .await;
        mount_json(
            &server,
            "/enumerations/issue_priorities.json",
            json!({ "issue_priorities": [] }),
        )
        .await;
        mount_json(
            &server,
            "/queries.json",
            json!({ "queries": [], "total_count": 0 }),
        )
        .await;
        Mock::given(method("GET"))
            .and(path("/custom_fields.json"))
            .respond_with(ResponseTemplate::new(403))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/projects.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "projects": [{
                    "id": 1,
                    "name": "Garden",
                    "issue_custom_fields": [{ "id": 36, "name": "Severity Rating" }]
                }],
                "total_count": 1
            })))
            .expect(1)
            .mount(&server)
            .await;
        let ctx = Ctx::new(client_with_file(&server.uri(), "{}"));
        let text = list_meta(&ctx, &object(json!({}))).await.unwrap();
        assert!(text.contains("hidden from this key"), "{text}");
        assert!(text.contains("severity_rating"), "{text}");
    }

    #[tokio::test]
    async fn list_issues_page_budget() {
        let server = MockServer::start().await;
        mount_catalog(&server).await;
        let issues = (1..=25).map(full_issue).collect::<Vec<_>>();
        Mock::given(method("GET"))
            .and(path("/issues.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "issues": issues,
                "total_count": 300
            })))
            .expect(2)
            .mount(&server)
            .await;
        let ctx = Ctx::new(client_with_file(&server.uri(), "{}"));
        let markdown = list_issues(&ctx, &object(json!({}))).await.unwrap();
        let json = list_issues(&ctx, &object(json!({ "format": "json" })))
            .await
            .unwrap();
        assert_budget("list_issues markdown, 25 rows", &markdown, 3_320);
        assert_budget("list_issues json, 25 rows", &json, 3_250);
    }

    fn scan_issue(id: u64, status: &str) -> Value {
        json!({
            "id": id,
            "project": { "id": 1, "name": "Garden" },
            "tracker": { "id": 1, "name": "Bug" },
            "status": { "id": 1, "name": status },
            "priority": { "id": 2, "name": "Normal" }
        })
    }

    fn issue_span(from: u64, to: u64, status: &str) -> Vec<Value> {
        (from..=to).map(|id| scan_issue(id, status)).collect()
    }

    #[tokio::test]
    async fn facet_issues_rejects_arguments_outside_its_schema_before_any_request() {
        let ctx = Ctx::new(client_with_file("http://127.0.0.1:9", "{}"));
        let err = facet_issues(
            &ctx,
            &object(json!({ "group_by": ["status"], "sections": ["queries"] })),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("Unknown argument: sections"), "{err}");
        let err = facet_issues(&ctx, &object(json!({})))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("group_by"), "{err}");
    }

    #[tokio::test]
    async fn a_scan_over_the_cap_is_refused_after_one_page() {
        let server = MockServer::start().await;
        mount_catalog(&server).await;
        Mock::given(method("GET"))
            .and(path("/issues.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "issues": [scan_issue(1, "New")],
                "total_count": 11
            })))
            .expect(1)
            .mount(&server)
            .await;
        let ctx = Ctx::new(client_tuned(&server.uri(), "{}", "10"));
        let err = facet_issues(&ctx, &object(json!({ "group_by": ["status"] })))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("over the scan cap of 10"), "{err}");
        assert!(err.contains("11 issues match"), "{err}");
        let requests = server.received_requests().await.unwrap();
        let pages = requests
            .iter()
            .filter(|request| request.url.path() == "/issues.json")
            .collect::<Vec<_>>();
        assert_eq!(pages.len(), 1);
        let query = pages[0].url.query_pairs().collect::<Vec<_>>();
        assert!(
            query
                .iter()
                .any(|(key, value)| key == "limit" && value == "100")
        );
        assert!(
            query
                .iter()
                .any(|(key, value)| key == "offset" && value == "0")
        );
        assert!(
            query
                .iter()
                .any(|(key, value)| key == "sort" && value == "id:asc")
        );
    }

    #[tokio::test]
    async fn three_pages_are_deduped_and_a_second_facet_reuses_the_scan() {
        let server = MockServer::start().await;
        mount_catalog(&server).await;
        Mock::given(method("GET"))
            .and(path("/issues.json"))
            .respond_with(|request: &wiremock::Request| {
                let query = request.url.query_pairs().collect::<Vec<_>>();
                let limit = query
                    .iter()
                    .find(|(key, _)| key == "limit")
                    .map(|(_, value)| value.to_string())
                    .unwrap_or_default();
                let offset = query
                    .iter()
                    .find(|(key, _)| key == "offset")
                    .map(|(_, value)| value.to_string())
                    .unwrap_or_else(|| "0".to_string());
                let (issues, total) = if limit == "1" {
                    (Vec::new(), 250)
                } else if offset == "0" {
                    (issue_span(1, 100, "Review"), 250)
                } else if offset == "100" {
                    (issue_span(100, 199, "Review"), 250)
                } else {
                    let mut page = issue_span(200, 249, "Review");
                    page.push(scan_issue(250, "Closed"));
                    (page, 250)
                };
                ResponseTemplate::new(200).set_body_json(json!({
                    "issues": issues,
                    "total_count": total
                }))
            })
            .expect(4)
            .mount(&server)
            .await;
        let ctx = Ctx::new(client_with_file(&server.uri(), "{}"));
        let args = object(json!({ "group_by": ["status"] }));
        let text = facet_issues(&ctx, &args).await.unwrap();
        // No status given, so Redmine's open default applied, and the output says so.
        assert!(
            text.contains("250 open issues, grouped by status:"),
            "{text}"
        );
        assert!(
            text.contains("Pass status \"*\" for every status"),
            "{text}"
        );
        assert!(text.contains("| Review | 249 |"), "{text}");
        assert!(text.contains("| Closed | 1 |"), "{text}");
        assert!(!text.contains("approximate"), "{text}");
        assert_eq!(
            ctx.scans.flights_in_use().await,
            0,
            "the lock is dropped after a scan"
        );
        let json_text = facet_issues(
            &ctx,
            &object(json!({ "group_by": ["status"], "format": "json" })),
        )
        .await
        .unwrap();
        let parsed: Value = serde_json::from_str(&json_text).unwrap();
        assert_eq!(parsed["total"], 250);
        assert!(parsed.get("approximate").is_none());
    }

    #[tokio::test]
    async fn drift_rescans_once_and_then_marks_the_result_approximate() {
        let server = MockServer::start().await;
        mount_catalog(&server).await;
        Mock::given(method("GET"))
            .and(path("/issues.json"))
            .respond_with(|request: &wiremock::Request| {
                let query = request.url.query_pairs().collect::<Vec<_>>();
                let limit = query
                    .iter()
                    .find(|(key, _)| key == "limit")
                    .map(|(_, value)| value.to_string())
                    .unwrap_or_default();
                let offset = query
                    .iter()
                    .find(|(key, _)| key == "offset")
                    .map(|(_, value)| value.to_string())
                    .unwrap_or_else(|| "0".to_string());
                let (issues, total) = if limit == "1" {
                    (Vec::new(), 151)
                } else if offset == "0" {
                    (issue_span(1, 100, "Review"), 150)
                } else {
                    (issue_span(100, 149, "Review"), 150)
                };
                ResponseTemplate::new(200).set_body_json(json!({
                    "issues": issues,
                    "total_count": total
                }))
            })
            .expect(6)
            .mount(&server)
            .await;
        let ctx = Ctx::new(client_with_file(&server.uri(), "{}"));
        let text = facet_issues(&ctx, &object(json!({ "group_by": ["status"] })))
            .await
            .unwrap();
        assert!(text.contains("approximate:"), "{text}");
        assert!(text.contains("scanned 149"), "{text}");
        assert!(text.contains("Redmine reported 151"), "{text}");
        assert!(text.contains("| Review | 149 |"), "{text}");
    }

    #[tokio::test]
    async fn two_facets_on_the_same_filters_share_one_scan() {
        let server = MockServer::start().await;
        mount_catalog(&server).await;
        Mock::given(method("GET"))
            .and(path("/issues.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "issues": [
                    scan_issue(1, "New"),
                    scan_issue(2, "Review"),
                    scan_issue(3, "Review"),
                    scan_issue(4, "Closed")
                ],
                "total_count": 4
            })))
            .expect(1)
            .mount(&server)
            .await;
        let ctx = Ctx::new(client_with_file(&server.uri(), "{}"));
        let by_status = facet_issues(
            &ctx,
            &object(json!({ "project_id": 12, "group_by": ["status"] })),
        )
        .await
        .unwrap();
        let by_assignee = facet_issues(
            &ctx,
            &object(json!({ "project_id": 12, "group_by": ["assignee"] })),
        )
        .await
        .unwrap();
        assert!(by_status.contains("| Review | 2 |"), "{by_status}");
        assert!(by_assignee.contains("(none)"), "{by_assignee}");
    }

    #[tokio::test]
    async fn an_unconfigured_custom_field_is_still_a_dimension() {
        let server = MockServer::start().await;
        mount_catalog(&server).await;
        Mock::given(method("GET"))
            .and(path("/issues.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "issues": [{
                    "id": 1,
                    "status": { "id": 1, "name": "New" },
                    "custom_fields": [{ "id": 36, "name": "Severity Rating", "multiple": true, "value": ["Minor", "Major"] }]
                }],
                "total_count": 1
            })))
            .expect(1)
            .mount(&server)
            .await;
        let ctx = Ctx::new(client_with_file(&server.uri(), "{}"));
        let text = facet_issues(&ctx, &object(json!({ "group_by": ["severity_rating"] })))
            .await
            .unwrap();
        assert!(
            text.contains("severity_rating -> Severity Rating (36)"),
            "{text}"
        );
        assert!(text.contains("| Minor | 1 |"), "{text}");
        assert!(text.contains("| Major | 1 |"), "{text}");
        assert!(text.contains("multi-value"), "{text}");
        assert!(text.contains("more than 1"), "{text}");
    }

    #[tokio::test]
    async fn a_misspelled_custom_field_value_lists_the_values_in_use() {
        let server = MockServer::start().await;
        mount_catalog(&server).await;
        Mock::given(method("GET"))
            .and(path("/issues.json"))
            .and(query_param("cf_40", "minor"))
            .and(query_param("limit", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "issues": [],
                "total_count": 0
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/issues.json"))
            .and(query_param_is_missing("cf_40"))
            .and(query_param("limit", "100"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "issues": [
                    { "id": 1, "custom_fields": [{ "id": 40, "name": "Severity", "value": "Minor" }] },
                    { "id": 2, "custom_fields": [{ "id": 40, "name": "Severity", "value": "Minor" }] },
                    { "id": 3, "custom_fields": [{ "id": 40, "name": "Severity", "value": "Major" }] }
                ],
                "total_count": 3
            })))
            .expect(1)
            .mount(&server)
            .await;
        let ctx = Ctx::new(client_with_file(&server.uri(), "{}"));
        let err = count_issues(
            &ctx,
            &object(json!({ "custom_fields": { "cf_40": "minor" } })),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("Severity = \"minor\""), "{err}");
        assert!(err.contains("Minor (2)"), "{err}");
        assert!(err.contains("Major (1)"), "{err}");
        assert!(err.contains("Did you mean \"Minor\"?"), "{err}");
    }

    #[tokio::test]
    async fn a_real_zero_keeps_the_count_and_drops_the_spelling_hint() {
        let server = MockServer::start().await;
        mount_catalog(&server).await;
        Mock::given(method("GET"))
            .and(path("/issues.json"))
            .and(query_param("cf_40", "Minor"))
            .and(query_param("limit", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "issues": [],
                "total_count": 0
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/issues.json"))
            .and(query_param_is_missing("cf_40"))
            .and(query_param("limit", "100"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "issues": [{
                    "id": 1,
                    "custom_fields": [{ "id": 40, "name": "Severity", "value": "Minor" }]
                }],
                "total_count": 1
            })))
            .expect(1)
            .mount(&server)
            .await;
        let ctx = Ctx::new(client_with_file(&server.uri(), "{}"));
        let text = count_issues(
            &ctx,
            &object(json!({ "custom_fields": { "cf_40": "Minor" } })),
        )
        .await
        .unwrap();
        assert!(text.starts_with("0 issues match"), "{text}");
        assert!(!text.contains("check the spelling"), "{text}");
        assert!(!text.contains("Values in use"), "{text}");
    }

    #[tokio::test]
    async fn a_value_check_over_the_scan_cap_keeps_the_hint() {
        let server = MockServer::start().await;
        mount_catalog(&server).await;
        Mock::given(method("GET"))
            .and(path("/issues.json"))
            .and(query_param("cf_40", "Minor"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "issues": [],
                "total_count": 0
            })))
            .expect(1)
            .with_priority(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/issues.json"))
            .and(query_param("limit", "100"))
            .and(query_param("offset", "0"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "issues": [scan_issue(1, "New")],
                "total_count": 9
            })))
            .expect(1)
            .mount(&server)
            .await;
        let ctx = Ctx::new(client_tuned(&server.uri(), "{}", "5"));
        let text = count_issues(
            &ctx,
            &object(json!({ "custom_fields": { "cf_40": "Minor" } })),
        )
        .await
        .unwrap();
        assert!(text.starts_with("0 issues match"), "{text}");
        assert!(text.contains("check the spelling"), "{text}");
        assert!(
            text.contains("Too many issues to check the value"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn an_ignored_custom_field_filter_stops_the_scan_before_the_next_page() {
        let server = MockServer::start().await;
        mount_catalog(&server).await;
        mount_probe(&server, 40, 250, 1).await;
        Mock::given(method("GET"))
            .and(path("/issues.json"))
            .and(query_param("cf_40", "High"))
            .and(query_param("limit", "100"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "issues": [scan_issue(1, "New")],
                "total_count": 250
            })))
            .expect(1)
            .mount(&server)
            .await;
        let ctx = Ctx::new(client_with_file(&server.uri(), "{}"));
        let err = facet_issues(
            &ctx,
            &object(json!({ "group_by": ["status"], "custom_fields": { "cf_40": "High" } })),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("Redmine ignored the filter on Severity (40)"),
            "{err}"
        );
    }

    fn full_issue(id: u64) -> Value {
        json!({
            "id": id,
            "project": { "id": 1, "name": "Garden" },
            "tracker": { "id": 1, "name": "Bug" },
            "status": { "id": 1, "name": "In progress" },
            "priority": { "id": 2, "name": "Normal" },
            "assigned_to": { "id": 4, "name": "Ada Lovelace" },
            "subject": format!("Hose split on the north bed near the gate {id}"),
            "updated_on": "2026-03-04T05:06:07Z",
            "description": "x".repeat(400)
        })
    }

    fn assert_budget(label: &str, text: &str, ceiling: usize) {
        assert!(
            text.len() <= ceiling,
            "{label} is {} bytes, over its {ceiling} byte budget",
            text.len()
        );
    }
}
