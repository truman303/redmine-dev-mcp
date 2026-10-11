use rmcp::schemars::{self, JsonSchema};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::catalog::Ctx;
use crate::client::Client;
use crate::error::{RedmineError, ToolError};
use crate::issue::{
    FieldLookup, IssueLimits, IssueView, RenderOpts, cap_issue, description_window,
    render_batch_json, render_batch_markdown, render_issue, shown_description,
};
use crate::parse::{IdForms, parse_issue_id};
use crate::render::{
    bytes_to_human, clip, columnar_json, compact_json, fenced, markdown_table, page_heading,
};
use crate::sanitize::sanitize;

const PAGE_DEFAULT: u64 = 25;
const SNIPPET_CAP: u64 = 300;
const TITLE_CLIP: usize = 90;
const JOURNALS_DEFAULT: u64 = 20;
const MAX_BATCH: usize = 20;

const PROJECT_COLUMNS: &[&str] = &["id", "identifier", "name", "updated_on"];
const SEARCH_COLUMNS: &[&str] = &["id", "type", "title", "datetime"];

#[derive(Debug, Clone, Copy, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum OutputFormat {
    #[default]
    Markdown,
    Json,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListProjectsArgs {
    /// Page size. Default 25, capped by REDMINE_MAX_LIST.
    #[serde(default)]
    pub limit: Option<u64>,
    /// Offset for paging. Default 0.
    #[serde(default)]
    pub offset: Option<u64>,
    /// Output format. Both return id, identifier, name and updated_on.
    #[serde(default)]
    pub format: OutputFormat,
}

/// Redmine's own `scope` values. Anything else falls back to the project, or to
/// every project when there is none, so a wrong value here fails silently.
#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SearchScope {
    /// Every project the key can see. Ignores project_id.
    All,
    /// Projects the key's user is a member of. Ignores project_id.
    MyProjects,
    /// Projects the key's user has bookmarked. Ignores project_id.
    Bookmarks,
    /// project_id and its subprojects. Needs project_id.
    Subprojects,
}

impl SearchScope {
    fn as_query(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::MyProjects => "my_projects",
            Self::Bookmarks => "bookmarks",
            Self::Subprojects => "subprojects",
        }
    }
}

/// A project id as a number or a string. Models send both.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum ProjectId {
    Number(u64),
    Text(String),
}

impl ProjectId {
    pub(crate) fn as_query(&self) -> String {
        match self {
            Self::Number(id) => id.to_string(),
            Self::Text(text) => text.trim().to_string(),
        }
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SearchArgs {
    /// Search query.
    pub q: String,
    /// Restrict to one project, by id or identifier.
    #[serde(default)]
    pub project_id: Option<ProjectId>,
    /// Characters of matching context per hit. 0 returns titles only. Capped at 300.
    #[serde(default)]
    pub snippet_chars: u64,
    /// Widen or change the project set. Omit to search project_id alone, or every
    /// project when project_id is absent.
    #[serde(default)]
    pub scope: Option<SearchScope>,
    /// Include issues. Default true.
    #[serde(default = "default_true")]
    pub issues: bool,
    /// Include wiki pages. Default false.
    #[serde(default)]
    pub wiki_pages: bool,
    /// Only open issues. Default false.
    #[serde(default)]
    pub open_issues: bool,
    /// Match titles only. Default false.
    #[serde(default)]
    pub titles_only: bool,
    /// Page size. Default 25, capped by REDMINE_MAX_LIST.
    #[serde(default)]
    pub limit: Option<u64>,
    /// Offset for paging. Default 0.
    #[serde(default)]
    pub offset: Option<u64>,
    /// Output format.
    #[serde(default)]
    pub format: OutputFormat,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetAttachmentArgs {
    /// Attachment id.
    pub id: u64,
    /// Download the file body. Text types only, under REDMINE_MAX_ATTACHMENT_BYTES.
    #[serde(default)]
    pub fetch: bool,
    /// Output format.
    #[serde(default)]
    pub format: OutputFormat,
}

/// A number or a string. Models send both, and a string is how a slug or a URL arrives.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum IssueRef {
    Number(u64),
    Text(String),
}

/// One id, or an array of up to 20. An array is always summary rows, including an array of one.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum IssueIds {
    Many(Vec<IssueRef>),
    One(IssueRef),
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetIssueArgs {
    /// Issue id, or up to 20. A number, `#123`, an issue URL, a configured slug, or an array of those.
    pub id: IssueIds,
    /// summary, description, journals, or full. Default summary, which does not fetch journals.
    #[serde(default)]
    pub view: IssueView,
    /// Character offset into the trimmed description. Only with view=description.
    /// Pass description_next_offset from the previous response. Counts characters
    /// in the trimmed text this tool shows, not raw bytes from the API.
    #[serde(default)]
    pub description_offset: Option<u64>,
    /// Include journal entries that are only field changes, with no note.
    #[serde(default)]
    pub include_field_changes: bool,
    /// Newest N journal entries. Default 20, never above REDMINE_MAX_JOURNALS.
    #[serde(default)]
    pub journals_limit: Option<u64>,
    /// Only journals on or after this date or time. YYYY-MM-DD, or a full UTC timestamp such as 2026-08-01T09:00:00Z.
    #[serde(default)]
    pub journals_since: Option<String>,
    /// Include attachment metadata. Default true.
    #[serde(default = "default_true")]
    pub include_attachments: bool,
    /// Include sub-issues. Default true.
    #[serde(default = "default_true")]
    pub include_children: bool,
    /// Include checklist items with their ids. Default true. One extra request. A 403 or 404 means the plugin is off.
    #[serde(default = "default_true")]
    pub include_checklist: bool,
    /// Include related issues. Default true.
    #[serde(default = "default_true")]
    pub include_relations: bool,
    /// Include watchers. Default false.
    #[serde(default)]
    pub include_watchers: bool,
    /// Output format.
    #[serde(default)]
    pub format: OutputFormat,
}

pub async fn list_projects(client: &Client, args: ListProjectsArgs) -> Result<String, ToolError> {
    let limit = cap_limit(args.limit, client.config().max_list);
    let offset = args.offset.unwrap_or(0);
    let query = [("limit", limit.to_string()), ("offset", offset.to_string())];
    let body = get_object(client, "/projects.json", &query).await?;
    let projects = array_field(&body, "projects")?;
    let total = u64_field(&body, "total_count");
    let rows = projects
        .iter()
        .map(|project| pick(project, PROJECT_COLUMNS))
        .collect::<Vec<_>>();
    match args.format {
        OutputFormat::Json => Ok(columnar_json(
            PROJECT_COLUMNS,
            rows,
            page_fields(total, offset, limit),
        )),
        OutputFormat::Markdown => {
            if rows.is_empty() {
                return Ok(empty_page(
                    "No projects visible to this API key.",
                    offset,
                    total,
                ));
            }
            let table = markdown_table(
                &["id", "identifier", "name", "updated"],
                &rows.iter().map(|row| display_row(row)).collect::<Vec<_>>(),
            );
            Ok(format!(
                "{}\n\n{table}",
                page_heading("Projects", offset, rows.len(), total, limit)
            ))
        }
    }
}

pub async fn search(client: &Client, args: SearchArgs) -> Result<String, ToolError> {
    let q = args.q.trim();
    if q.is_empty() {
        return Err(ToolError::refused("Search query is empty."));
    }
    // With no type named, Redmine searches every type: news, changesets, forums,
    // and so on. That is not what turning both off means.
    if !args.issues && !args.wiki_pages {
        return Err(ToolError::refused(
            "Set issues or wiki_pages to true. With both false, Redmine searches every content type.",
        ));
    }
    if matches!(args.scope, Some(SearchScope::Subprojects)) && args.project_id.is_none() {
        return Err(ToolError::refused(
            "scope=subprojects needs project_id. Without one, Redmine searches every project.",
        ));
    }
    let limit = cap_limit(args.limit, client.config().max_list);
    let offset = args.offset.unwrap_or(0);
    let snippet_chars = args.snippet_chars.min(SNIPPET_CAP);

    // Redmine reads these flags with `present?`, so "0" counts as on. A flag is
    // sent only when it is true.
    let mut query = vec![
        ("q", q.to_string()),
        ("limit", limit.to_string()),
        ("offset", offset.to_string()),
    ];
    for (flag, on) in [
        ("issues", args.issues),
        ("wiki_pages", args.wiki_pages),
        ("open_issues", args.open_issues),
        ("titles_only", args.titles_only),
    ] {
        if on {
            query.push((flag, "1".to_string()));
        }
    }
    // The search controller finds the project from `id`, not `project_id`.
    if let Some(project_id) = &args.project_id {
        query.push(("id", project_id.as_query()));
    }
    if let Some(scope) = args.scope {
        query.push(("scope", scope.as_query().to_string()));
    }

    let body = get_object(client, "/search.json", &query).await?;
    let results = array_field(&body, "results")?;
    let total = u64_field(&body, "total_count");
    let mut columns = SEARCH_COLUMNS.to_vec();
    if snippet_chars > 0 {
        columns.push("snippet");
    }
    let rows = results
        .iter()
        .map(|hit| search_row(hit, snippet_chars))
        .collect::<Vec<_>>();
    match args.format {
        OutputFormat::Json => Ok(columnar_json(
            &columns,
            rows,
            page_fields(total, offset, limit),
        )),
        OutputFormat::Markdown => {
            if rows.is_empty() {
                return Ok(empty_page("No search results.", offset, total));
            }
            let table = markdown_table(
                &columns,
                &rows.iter().map(|row| display_row(row)).collect::<Vec<_>>(),
            );
            Ok(format!(
                "{}\n\n{table}",
                page_heading("Search results", offset, rows.len(), total, limit)
            ))
        }
    }
}

pub async fn get_attachment(client: &Client, args: GetAttachmentArgs) -> Result<String, ToolError> {
    if args.id == 0 {
        return Err(ToolError::refused(
            "Attachment id must be a positive integer.",
        ));
    }
    let path = format!("/attachments/{}.json", args.id);
    let body = get_object(client, &path, &[]).await?;
    let attachment = match body.get("attachment") {
        Some(attachment) if attachment.is_object() => attachment,
        _ => {
            return Err(ToolError::refused(format!(
                "Redmine returned no attachment for id {}.",
                args.id
            )));
        }
    };
    if !args.fetch {
        return Ok(render_attachment(attachment, None, args.format));
    }

    let filename = display_field(attachment, "filename");
    let content_type = attachment
        .get("content_type")
        .and_then(Value::as_str)
        .unwrap_or("");
    if !is_text_attachment(content_type, &filename) {
        let type_label = if content_type.is_empty() {
            "unknown type"
        } else {
            content_type
        };
        return Err(ToolError::refused(format!(
            "Attachment {} ({filename}, {type_label}) is not a text type. Metadata is available with fetch=false.",
            args.id
        )));
    }
    let cap = client.config().max_attachment_bytes;
    let filesize = u64_field(attachment, "filesize").unwrap_or(0);
    if filesize > cap {
        return Err(ToolError::refused(format!(
            "Attachment {} is {filesize} bytes, over the {cap} byte fetch cap. Metadata is available with fetch=false.",
            args.id
        )));
    }
    // Built from the id, not the payload's content_url. Redmine derives that from
    // the request host, which behind a proxy can be an internal name.
    let download = format!("/attachments/download/{}", args.id);
    let bytes = match client.get_bytes_limited(&download, cap).await {
        Ok(bytes) => bytes,
        Err(RedmineError::TooLarge { .. }) => {
            return Err(ToolError::refused(format!(
                "Attachment {} is over the {cap} byte fetch cap. Metadata is available with fetch=false.",
                args.id
            )));
        }
        Err(err) => return Err(err.into()),
    };
    let content = match String::from_utf8(bytes) {
        Ok(text) => Content { text, lossy: false },
        Err(err) => Content {
            text: String::from_utf8_lossy(err.as_bytes()).into_owned(),
            lossy: true,
        },
    };
    Ok(render_attachment(attachment, Some(content), args.format))
}

pub async fn get_issue(ctx: &Ctx, args: GetIssueArgs) -> Result<String, ToolError> {
    let client = &ctx.client;
    let ids = parse_ids(&args.id, id_forms(client.config()))?;
    let batch = matches!(args.id, IssueIds::Many(_));
    if batch && args.view != IssueView::Summary {
        return Err(ToolError::refused(
            "A batch of ids returns summary rows. Pass one id for the description or the journals."
                .to_string(),
        ));
    }
    refuse_description_offset(&args)?;
    if args.journals_limit == Some(0) {
        return Err(ToolError::refused(
            "journals_limit must be a positive integer.".to_string(),
        ));
    }
    let journals_since = args
        .journals_since
        .as_deref()
        .map(str::trim)
        .filter(|since| !since.is_empty())
        .map(check_since)
        .transpose()?;

    if batch {
        let (names, fetched) = tokio::join!(ctx.catalog.issue_names(), fetch_batch(client, &ids));
        let (issues, missing) = fetched?;
        let lookup = field_lookup(client, &names);
        return Ok(match args.format {
            OutputFormat::Markdown => {
                render_batch_markdown(&issues, &missing, &client.config().base_url, lookup)
            }
            OutputFormat::Json => render_batch_json(&issues, &missing, lookup),
        });
    }

    let id = ids[0];
    let include = include_param(&args);
    let limits = IssueLimits::from_config(client.config());

    let (names, issue, checklist) = tokio::join!(
        ctx.catalog.issue_names(),
        fetch_issue(client, id, &include),
        fetch_checklist(client, id, args.include_checklist),
    );
    let mut issue = issue?;
    // Built here, from the text the render shows, so a bad offset is a refusal
    // before anything is rendered rather than a check the render has to repeat.
    let description = if matches!(args.view, IssueView::Description | IssueView::Full) {
        let offset = usize::try_from(args.description_offset.unwrap_or(0)).unwrap_or(usize::MAX);
        Some(
            description_window(
                &shown_description(&issue),
                offset,
                limits.max_description_chars,
            )
            .map_err(ToolError::refused)?,
        )
    } else {
        None
    };
    let opts = RenderOpts {
        view: args.view,
        journals_limit: args
            .journals_limit
            .unwrap_or(JOURNALS_DEFAULT)
            .min(client.config().max_journals),
        include_field_changes: args.include_field_changes,
        journals_since,
        description: description.as_ref(),
        names: field_lookup(client, &names),
    };
    if let Some(object) = issue.as_object_mut() {
        match checklist {
            Checklist::Items(items) => {
                object.insert("checklists".to_string(), Value::Array(items));
            }
            Checklist::Failed(reason) => {
                object.insert("checklist_error".to_string(), Value::String(reason));
            }
            Checklist::None => {}
        }
    }

    let url = format!(
        "{}/issues/{id}",
        client.config().base_url.trim_end_matches('/')
    );
    Ok(match args.format {
        OutputFormat::Json => compact_json(&cap_issue(&issue, &limits, &opts)),
        OutputFormat::Markdown => render_issue(&issue, &limits, &url, &opts),
    })
}

struct Content {
    text: String,
    /// The file was not valid UTF-8. Bad bytes became U+FFFD.
    lossy: bool,
}

fn render_attachment(attachment: &Value, content: Option<Content>, format: OutputFormat) -> String {
    match format {
        OutputFormat::Json => {
            let mut value = attachment.clone();
            if let Some(object) = value.as_object_mut() {
                // The download URL is not useful to the model and is often long.
                object.remove("content_url");
                object.remove("thumbnail_url");
                if let Some(content) = content {
                    object.insert("content".to_string(), Value::String(content.text));
                    if content.lossy {
                        object.insert("content_lossy".to_string(), Value::Bool(true));
                    }
                }
            }
            compact_json(&value)
        }
        OutputFormat::Markdown => {
            let author = named(attachment.get("author"));
            let filesize = u64_field(attachment, "filesize").unwrap_or(0);
            let human = bytes_to_human(filesize);
            let rows = [
                ("id", display_field(attachment, "id")),
                ("filename", display_field(attachment, "filename")),
                ("size", format!("{human} ({filesize} bytes)")),
                ("content_type", display_field(attachment, "content_type")),
                ("author", author),
                ("created", display_field(attachment, "created_on")),
                ("description", display_field(attachment, "description")),
            ]
            .into_iter()
            .filter(|(_, value)| !value.is_empty())
            .map(|(field, value)| vec![field.to_string(), value])
            .collect::<Vec<_>>();
            let mut text = markdown_table(&["field", "value"], &rows);
            if let Some(content) = content {
                text.push_str("\n\nContent");
                if content.lossy {
                    text.push_str(" (not valid UTF-8; bad bytes shown as \u{FFFD})");
                }
                text.push_str(":\n\n");
                text.push_str(&fenced(&content.text, ""));
            }
            text
        }
    }
}

fn search_row(hit: &Value, snippet_chars: u64) -> Vec<Value> {
    let mut row = pick(hit, SEARCH_COLUMNS);
    if let Some(Value::String(title)) = row.get_mut(2) {
        *title = clip(title, TITLE_CLIP);
    }
    if snippet_chars > 0 {
        let raw = hit.get("description").and_then(Value::as_str).unwrap_or("");
        let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
        row.push(Value::String(clip(&collapsed, snippet_chars as usize)));
    }
    row
}

/// The listed fields of a record, in order. A missing field is null.
fn pick(record: &Value, columns: &[&str]) -> Vec<Value> {
    columns
        .iter()
        .map(|key| record.get(*key).cloned().unwrap_or(Value::Null))
        .collect()
}

fn display_row(row: &[Value]) -> Vec<String> {
    row.iter().map(display_value).collect()
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

/// An empty page past the end is not the same as nothing existing.
fn empty_page(none_at_all: &str, offset: u64, total: Option<u64>) -> String {
    match total {
        Some(total) if total > 0 => {
            format!("No rows at offset {offset}. There are {total} in total.")
        }
        _ if offset > 0 => format!("No rows at offset {offset}."),
        _ => none_at_all.to_string(),
    }
}

fn is_text_attachment(content_type: &str, filename: &str) -> bool {
    let content_type = content_type.trim().to_ascii_lowercase();
    let primary = content_type.split(';').next().unwrap_or("").trim();
    let type_ok = primary.starts_with("text/")
        || primary.ends_with("+json")
        || primary.ends_with("+xml")
        || matches!(
            primary.strip_prefix("application/"),
            Some("json" | "xml" | "javascript" | "x-yaml" | "yaml" | "csv" | "x-sh" | "sql")
        );
    if type_ok {
        return true;
    }
    let lower = filename.to_ascii_lowercase();
    [
        ".txt",
        ".md",
        ".markdown",
        ".json",
        ".xml",
        ".csv",
        ".log",
        ".yml",
        ".yaml",
        ".textile",
        ".html",
        ".css",
        ".js",
        ".ts",
        ".sql",
    ]
    .iter()
    .any(|ext| lower.ends_with(ext))
}

async fn get_object(
    client: &Client,
    path: &str,
    query: &[(&str, String)],
) -> Result<Value, ToolError> {
    let refs = query
        .iter()
        .map(|(key, value)| (*key, value.as_str()))
        .collect::<Vec<_>>();
    let body = sanitize(client.get_json::<Value>(path, &refs).await?);
    if !body.is_object() {
        return Err(ToolError::refused(format!(
            "Unexpected response from {path}: expected an object"
        )));
    }
    Ok(body)
}

fn array_field<'a>(body: &'a Value, key: &str) -> Result<&'a [Value], ToolError> {
    match body.get(key) {
        None | Some(Value::Null) => Ok(&[]),
        Some(Value::Array(items)) => Ok(items),
        Some(_) => Err(ToolError::refused(format!(
            "Redmine returned {key} in an unexpected shape"
        ))),
    }
}

fn u64_field(value: &Value, key: &str) -> Option<u64> {
    value.get(key).and_then(Value::as_u64)
}

fn display_field(value: &Value, key: &str) -> String {
    value.get(key).map(display_value).unwrap_or_default()
}

fn display_value(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        Value::Bool(flag) => flag.to_string(),
        _ => String::new(),
    }
}

fn named(value: Option<&Value>) -> String {
    let Some(value) = value else {
        return String::new();
    };
    if let Some(name) = value.get("name").and_then(Value::as_str)
        && !name.is_empty()
    {
        return name.to_string();
    }
    value
        .get("id")
        .and_then(Value::as_u64)
        .map(|id| id.to_string())
        .unwrap_or_default()
}

fn field_lookup<'a>(client: &'a Client, names: &'a crate::catalog::IssueNames) -> FieldLookup<'a> {
    FieldLookup {
        configured: &client.config().fields,
        assigned: names.assigned.as_ref(),
        names_error: names.error.as_deref(),
    }
}

fn id_forms(config: &crate::config::Config) -> IdForms<'_> {
    IdForms {
        prefix: config.issue_prefix.as_deref(),
        base_url: Some(config.base_url.as_str()),
    }
}

fn parse_ids(id: &IssueIds, forms: IdForms<'_>) -> Result<Vec<u64>, ToolError> {
    let refs: Vec<&IssueRef> = match id {
        IssueIds::One(one) => vec![one],
        IssueIds::Many(many) => many.iter().collect(),
    };
    if refs.is_empty() {
        return Err(ToolError::refused("Pass an issue id.".to_string()));
    }
    if refs.len() > MAX_BATCH {
        return Err(ToolError::refused(format!(
            "A batch is {MAX_BATCH} ids at most. This one has {}.",
            refs.len()
        )));
    }
    let mut ids = Vec::with_capacity(refs.len());
    for id in refs {
        let id = parse_one(id, forms)?;
        // `#12` and a URL for 12 are the same issue. Asking twice adds nothing.
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    Ok(ids)
}

fn parse_one(id: &IssueRef, forms: IdForms<'_>) -> Result<u64, ToolError> {
    let text = match id {
        IssueRef::Number(number) => number.to_string(),
        IssueRef::Text(text) => text.clone(),
    };
    parse_issue_id(&text, forms).map_err(ToolError::refused)
}

/// `description_offset` is a page into one description. Any other view would
/// ignore it, and the caller would think it had received that slice.
fn refuse_description_offset(args: &GetIssueArgs) -> Result<(), ToolError> {
    let Some(offset) = args.description_offset else {
        return Ok(());
    };
    if matches!(args.id, IssueIds::Many(_)) {
        return Err(ToolError::refused(
            "description_offset applies to one issue with view=description. A batch returns summary rows."
                .to_string(),
        ));
    }
    match args.view {
        IssueView::Description => Ok(()),
        IssueView::Full if offset == 0 => Ok(()),
        IssueView::Full => Err(ToolError::refused(
            "description_offset is only valid with view=description. view=full always starts at the beginning and also returns journals."
                .to_string(),
        )),
        IssueView::Summary | IssueView::Journals => Err(ToolError::refused(
            "description_offset is only valid with view=description.".to_string(),
        )),
    }
}

/// Summary and description do not ask for journals. On a heavily edited issue
/// those are most of the payload, so leaving them off saves the bytes on the wire.
fn include_param(args: &GetIssueArgs) -> String {
    let mut parts = Vec::new();
    if matches!(args.view, IssueView::Journals | IssueView::Full) {
        parts.push("journals");
    }
    if args.include_attachments {
        parts.push("attachments");
    }
    if args.include_children {
        parts.push("children");
    }
    if args.include_relations {
        parts.push("relations");
    }
    if args.include_watchers {
        parts.push("watchers");
    }
    parts.join(",")
}

async fn fetch_issue(client: &Client, id: u64, include: &str) -> Result<Value, ToolError> {
    let path = format!("/issues/{id}.json");
    let mut query = Vec::new();
    if !include.is_empty() {
        query.push(("include", include.to_string()));
    }
    let body = get_object(client, &path, &query).await?;
    match body.get("issue") {
        Some(issue) if issue.is_object() => Ok(issue.clone()),
        _ => Err(ToolError::refused(format!(
            "Redmine returned no issue for id {id}."
        ))),
    }
}

/// One list request for the whole batch, not one GET per id. Twenty parallel
/// GETs is the burst that trips a rate limiter, and one 404 among them would
/// lose the other nineteen. The list endpoint skips closed issues unless
/// `status_id=*`. Ids it does not return are missing or not visible.
async fn fetch_batch(client: &Client, ids: &[u64]) -> Result<(Vec<Value>, Vec<u64>), ToolError> {
    let joined = ids.iter().map(u64::to_string).collect::<Vec<_>>().join(",");
    let query = [
        ("issue_id", joined),
        ("status_id", "*".to_string()),
        ("limit", ids.len().to_string()),
    ];
    let body = get_object(client, "/issues.json", &query).await?;
    let mut by_id = array_field(&body, "issues")?
        .iter()
        .filter_map(|issue| Some((issue.get("id")?.as_u64()?, issue.clone())))
        .collect::<std::collections::HashMap<_, _>>();
    let mut issues = Vec::with_capacity(ids.len());
    let mut missing = Vec::new();
    for id in ids {
        match by_id.remove(id) {
            Some(issue) => issues.push(issue),
            None => missing.push(*id),
        }
    }
    Ok((issues, missing))
}

enum Checklist {
    /// Not asked for, no items, or the plugin is off.
    None,
    Items(Vec<Value>),
    /// The endpoint failed some other way. The issue still comes back.
    Failed(String),
}

/// A 403 or 404 means the plugin's module is off for this issue's project.
/// The read runs alongside the issue fetch, before the project is known, so it
/// can't skip projects already seen without the module; the 404 is cheap and
/// shows nothing. Any other failure is reported beside the issue rather than
/// failing the whole read.
async fn fetch_checklist(client: &Client, id: u64, include: bool) -> Checklist {
    if !include {
        return Checklist::None;
    }
    let path = format!("/issues/{id}/checklists.json");
    match get_object(client, &path, &[]).await {
        Ok(body) => match array_field(&body, "checklists") {
            Ok([]) => Checklist::None,
            Ok(items) => Checklist::Items(items.to_vec()),
            Err(err) => Checklist::Failed(err.to_string()),
        },
        Err(ToolError::Redmine(err)) if matches!(err.status(), Some(403 | 404)) => Checklist::None,
        Err(err) => Checklist::Failed(err.to_string()),
    }
}

/// Journal times are UTC timestamps such as `2026-08-01T09:00:00Z`, compared as
/// text. That only works for this exact shape, so anything else is refused
/// rather than silently matching too much or nothing.
fn check_since(raw: &str) -> Result<&str, ToolError> {
    let bytes = raw.as_bytes();
    let digits = |range: std::ops::Range<usize>| bytes[range].iter().all(u8::is_ascii_digit);
    let date_ok = bytes.len() >= 10
        && digits(0..4)
        && bytes[4] == b'-'
        && digits(5..7)
        && bytes[7] == b'-'
        && digits(8..10);
    let time_ok = match bytes.len() {
        10 => true,
        20 => {
            bytes[10] == b'T'
                && digits(11..13)
                && bytes[13] == b':'
                && digits(14..16)
                && bytes[16] == b':'
                && digits(17..19)
                && bytes[19] == b'Z'
        }
        _ => false,
    };
    if date_ok && time_ok {
        Ok(raw)
    } else {
        Err(ToolError::refused(format!(
            "journals_since must be YYYY-MM-DD or a UTC timestamp like 2026-08-01T09:00:00Z, got \"{raw}\"."
        )))
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

    use wiremock::matchers::{method, path, path_regex, query_param, query_param_is_missing};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::catalog::Ctx;
    use crate::config::load_from;

    async fn call_issue(client: &Client, args: GetIssueArgs) -> Result<String, ToolError> {
        get_issue(&Ctx::new(client.clone()), args).await
    }

    /// A catalog that loads, so `get_issue` tests exercise the real naming path
    /// rather than the `cf_<id>` fallback. The fields match the ones the issue
    /// fixtures carry.
    async fn mount_catalog(server: &MockServer) {
        for (at, body) in [
            (
                "/issue_statuses.json",
                json!({ "issue_statuses": [
                    { "id": 1, "name": "New", "is_closed": false },
                    { "id": 5, "name": "Closed", "is_closed": true }
                ]}),
            ),
            (
                "/trackers.json",
                json!({ "trackers": [{ "id": 1, "name": "Bug" }] }),
            ),
            (
                "/enumerations/issue_priorities.json",
                json!({ "issue_priorities": [{ "id": 2, "name": "Normal" }] }),
            ),
            ("/queries.json", json!({ "queries": [], "total_count": 0 })),
            (
                "/custom_fields.json",
                json!({ "custom_fields": [
                    { "id": 36, "name": "Severity Rating", "customized_type": "issue" },
                    { "id": 40, "name": "Severity", "customized_type": "issue" }
                ]}),
            ),
        ] {
            Mock::given(method("GET"))
                .and(path(at))
                .respond_with(ResponseTemplate::new(200).set_body_json(body))
                .expect(1)
                .mount(server)
                .await;
        }
    }

    fn assert_named(text: &str) {
        assert!(!text.contains("Field names unavailable"), "{text}");
    }

    fn client_with(base: &str, extra: &[(&str, &str)]) -> Client {
        let mut pairs = vec![
            ("REDMINE_URL".to_string(), base.to_string()),
            ("REDMINE_API_KEY".to_string(), "test-key".to_string()),
        ];
        for (key, value) in extra {
            pairs.push(((*key).to_string(), (*value).to_string()));
        }
        let env: HashMap<String, String> = pairs.into_iter().collect();
        Client::new(load_from(&env, |_| unreachable!()).unwrap()).unwrap()
    }

    fn projects_args(limit: Option<u64>, format: OutputFormat) -> ListProjectsArgs {
        ListProjectsArgs {
            limit,
            offset: None,
            format,
        }
    }

    fn search_args(q: &str) -> SearchArgs {
        SearchArgs {
            q: q.to_string(),
            project_id: None,
            snippet_chars: 0,
            scope: None,
            issues: true,
            wiki_pages: false,
            open_issues: false,
            titles_only: false,
            limit: None,
            offset: None,
            format: OutputFormat::Markdown,
        }
    }

    fn attachment_args(id: u64, fetch: bool, format: OutputFormat) -> GetAttachmentArgs {
        GetAttachmentArgs { id, fetch, format }
    }

    /// A project as Redmine returns it, with the fields list rows drop.
    fn full_project(id: u64) -> Value {
        json!({
            "id": id,
            "name": format!("Project {id}"),
            "identifier": format!("project-{id}"),
            "description": "A paragraph of description that list rows should never carry. ".repeat(8),
            "homepage": "https://example.test/home",
            "parent": { "id": 1, "name": "Parent" },
            "status": 1,
            "is_public": true,
            "inherit_members": false,
            "custom_fields": [{ "id": 3, "name": "Owner", "value": "Someone" }],
            "created_on": "2026-01-01T00:00:00Z",
            "updated_on": "2026-01-02T00:00:00Z"
        })
    }

    fn full_hit(id: u64) -> Value {
        json!({
            "id": id,
            "title": format!("Bug #{id} (New): {}", "a long subject line ".repeat(10)),
            "type": "issue",
            "url": format!("https://redmine.example/issues/{id}"),
            "description": "matching context ".repeat(40),
            "datetime": "2026-02-01T00:00:00Z"
        })
    }

    async fn mount_json(server: &MockServer, at: &str, body: Value) {
        Mock::given(method("GET"))
            .and(path(at))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn list_projects_markdown_drops_the_description_and_caps_the_page() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/projects.json"))
            .and(query_param("limit", "2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "projects": [{
                    "id": 7,
                    "identifier": "ops",
                    "name": "Ops | Nightly",
                    "description": "SECRET DESCRIPTION",
                    "updated_on": "2026-01-02"
                }],
                "total_count": 9
            })))
            .mount(&server)
            .await;

        let client = client_with(&server.uri(), &[("REDMINE_MAX_LIST", "2")]);
        let text = list_projects(&client, projects_args(Some(50), OutputFormat::Markdown))
            .await
            .unwrap();
        assert!(text.contains("Projects 1 to 1 of 9 (limit 2):"));
        assert!(text.contains("Ops \\| Nightly"));
        assert!(!text.contains("SECRET DESCRIPTION"));
    }

    #[tokio::test]
    async fn list_projects_json_is_columnar_and_drops_everything_else() {
        let server = MockServer::start().await;
        let mut project = full_project(1);
        project["api_key"] = json!("secret-key");
        mount_json(
            &server,
            "/projects.json",
            json!({ "projects": [project], "total_count": 1 }),
        )
        .await;

        let client = client_with(&server.uri(), &[]);
        let text = list_projects(&client, projects_args(None, OutputFormat::Json))
            .await
            .unwrap();
        assert_eq!(
            text,
            r#"{"columns":["id","identifier","name","updated_on"],"limit":25,"offset":0,"rows":[[1,"project-1","Project 1","2026-01-02T00:00:00Z"]],"total_count":1}"#
        );
    }

    #[tokio::test]
    async fn empty_pages_say_whether_anything_exists() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/projects.json"))
            .and(query_param("offset", "0"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"projects": []})))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/projects.json"))
            .and(query_param("offset", "50"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"projects": [], "total_count": 9})),
            )
            .mount(&server)
            .await;

        let client = client_with(&server.uri(), &[]);
        let none = list_projects(&client, projects_args(None, OutputFormat::Markdown))
            .await
            .unwrap();
        assert_eq!(none, "No projects visible to this API key.");

        let past_end = list_projects(
            &client,
            ListProjectsArgs {
                limit: None,
                offset: Some(50),
                format: OutputFormat::Markdown,
            },
        )
        .await
        .unwrap();
        assert_eq!(past_end, "No rows at offset 50. There are 9 in total.");
    }

    #[tokio::test]
    async fn search_sends_only_true_flags_and_the_project_as_id() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/search.json"))
            .and(query_param("q", "router"))
            .and(query_param("issues", "1"))
            .and(query_param("open_issues", "1"))
            .and(query_param("id", "ops"))
            .and(query_param_is_missing("wiki_pages"))
            .and(query_param_is_missing("titles_only"))
            .and(query_param_is_missing("scope"))
            .and(query_param_is_missing("project_id"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "results": [full_hit(4)],
                "total_count": 1
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = client_with(&server.uri(), &[]);
        let mut args = search_args(" router ");
        args.project_id = Some(ProjectId::Text("ops".to_string()));
        args.open_issues = true;
        let text = search(&client, args).await.unwrap();
        assert!(text.contains("Search results 1 to 1 of 1"), "{text}");
    }

    #[tokio::test]
    async fn search_passes_redmines_scope_names() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/search.json"))
            .and(query_param("scope", "my_projects"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"results": []})))
            .expect(1)
            .mount(&server)
            .await;
        let client = client_with(&server.uri(), &[]);
        let mut args = search_args("router");
        args.scope = Some(SearchScope::MyProjects);
        assert_eq!(search(&client, args).await.unwrap(), "No search results.");
    }

    #[tokio::test]
    async fn search_refuses_calls_redmine_would_widen() {
        let client = client_with("https://redmine.example", &[]);
        let mut both_off = search_args("router");
        both_off.issues = false;
        let err = search(&client, both_off).await.unwrap_err();
        assert!(err.to_string().contains("every content type"), "{err}");

        let mut subprojects = search_args("router");
        subprojects.scope = Some(SearchScope::Subprojects);
        let err = search(&client, subprojects).await.unwrap_err();
        assert!(err.to_string().contains("needs project_id"), "{err}");

        let err = search(&client, search_args("  ")).await.unwrap_err();
        assert_eq!(err, ToolError::refused("Search query is empty."));
    }

    #[tokio::test]
    async fn search_omits_snippets_unless_asked_and_clips_titles() {
        let server = MockServer::start().await;
        let title = format!("{}é", "a".repeat(100));
        mount_json(
            &server,
            "/search.json",
            json!({
                "results": [{
                    "id": 4,
                    "type": "issue",
                    "title": title,
                    "description": "a long   match\nwith breaks",
                    "datetime": "2026-02-01"
                }],
                "total_count": 1
            }),
        )
        .await;

        let client = client_with(&server.uri(), &[]);
        let markdown = search(&client, search_args("router")).await.unwrap();
        assert!(markdown.contains('…'));
        assert!(!markdown.contains("a long"));

        let mut args = search_args("router");
        args.snippet_chars = 12;
        args.format = OutputFormat::Json;
        let json = search(&client, args).await.unwrap();
        assert!(json.contains(r#""columns":["id","type","title","datetime","snippet"]"#));
        assert!(json.contains("\"a long matc…\""), "{json}");
        assert!(!json.contains('\n'));
    }

    #[tokio::test]
    async fn attachment_downloads_text_by_id_and_fences_it() {
        let server = MockServer::start().await;
        mount_json(
            &server,
            "/attachments/5.json",
            json!({
                "attachment": {
                    "id": 5,
                    "filename": "notes.md",
                    "filesize": 25,
                    "content_type": "text/markdown",
                    // An internal host, as Redmine reports behind a proxy. Never used.
                    "content_url": "http://app-internal:3000/attachments/download/5/notes.md",
                    "author": { "id": 1, "name": "Ada", "api_key": "secret-key" }
                }
            }),
        )
        .await;
        Mock::given(method("GET"))
            .and(path("/attachments/download/5"))
            .respond_with(ResponseTemplate::new(200).set_body_string("## Injected\n```\nhi\n```"))
            .expect(2)
            .mount(&server)
            .await;

        let client = client_with(&server.uri(), &[]);
        let json = get_attachment(&client, attachment_args(5, true, OutputFormat::Json))
            .await
            .unwrap();
        assert!(json.contains("\"content\":\"## Injected"), "{json}");
        assert!(!json.contains("secret-key"));
        assert!(!json.contains("app-internal"));

        let markdown = get_attachment(&client, attachment_args(5, true, OutputFormat::Markdown))
            .await
            .unwrap();
        assert!(
            markdown.ends_with("Content:\n\n````\n## Injected\n```\nhi\n```\n````"),
            "{markdown}"
        );
    }

    #[tokio::test]
    async fn attachment_fetch_rejects_binary_and_marks_invalid_utf8() {
        let server = MockServer::start().await;
        mount_json(
            &server,
            "/attachments/9.json",
            json!({ "attachment": {
                "id": 9, "filename": "diagram.pdf", "filesize": 4, "content_type": "application/pdf"
            }}),
        )
        .await;
        mount_json(
            &server,
            "/attachments/6.json",
            json!({ "attachment": {
                "id": 6, "filename": "old.log", "filesize": 4, "content_type": "text/plain"
            }}),
        )
        .await;
        Mock::given(method("GET"))
            .and(path("/attachments/download/6"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"caf\xe9".to_vec()))
            .mount(&server)
            .await;

        let client = client_with(&server.uri(), &[]);
        let refused = get_attachment(&client, attachment_args(9, true, OutputFormat::Markdown))
            .await
            .unwrap_err();
        assert!(refused.to_string().contains("not a text type"), "{refused}");

        let lossy = get_attachment(&client, attachment_args(6, true, OutputFormat::Json))
            .await
            .unwrap();
        assert!(lossy.contains("caf\u{FFFD}"), "{lossy}");
        assert!(lossy.contains(r#""content_lossy":true"#), "{lossy}");
    }

    #[tokio::test]
    async fn oversized_attachment_is_refused_before_download() {
        let server = MockServer::start().await;
        mount_json(
            &server,
            "/attachments/3.json",
            json!({ "attachment": {
                "id": 3, "filename": "notes.txt", "filesize": 50, "content_type": "text/plain"
            }}),
        )
        .await;
        Mock::given(method("GET"))
            .and(path("/attachments/download/3"))
            .respond_with(ResponseTemplate::new(200).set_body_string("should not be read"))
            .expect(0)
            .mount(&server)
            .await;

        let client = client_with(&server.uri(), &[("REDMINE_MAX_ATTACHMENT_BYTES", "10")]);
        let err = get_attachment(&client, attachment_args(3, true, OutputFormat::Markdown))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("over the 10 byte fetch cap"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn a_filesize_that_lies_still_stops_at_the_cap() {
        let server = MockServer::start().await;
        mount_json(
            &server,
            "/attachments/4.json",
            json!({ "attachment": {
                "id": 4, "filename": "notes.txt", "filesize": 5, "content_type": "text/plain"
            }}),
        )
        .await;
        Mock::given(method("GET"))
            .and(path("/attachments/download/4"))
            .respond_with(ResponseTemplate::new(200).set_body_string("x".repeat(50)))
            .mount(&server)
            .await;

        let client = client_with(&server.uri(), &[("REDMINE_MAX_ATTACHMENT_BYTES", "10")]);
        let err = get_attachment(&client, attachment_args(4, true, OutputFormat::Json))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Refused(_)), "{err:?}");
        assert!(
            err.to_string().contains("over the 10 byte fetch cap"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn redmine_errors_reach_the_boundary_with_their_status() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/attachments/8.json"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let client = client_with(&server.uri(), &[]);
        let err = get_attachment(&client, attachment_args(8, false, OutputFormat::Markdown))
            .await
            .unwrap_err();
        let ToolError::Redmine(inner) = err else {
            panic!("expected a Redmine error, got {err:?}");
        };
        assert_eq!(inner.status(), Some(404));
    }

    fn issue_args(id: IssueIds) -> GetIssueArgs {
        GetIssueArgs {
            id,
            view: IssueView::Summary,
            include_field_changes: false,
            journals_limit: None,
            journals_since: None,
            include_attachments: true,
            include_children: true,
            include_checklist: true,
            include_relations: true,
            include_watchers: false,
            format: OutputFormat::Markdown,
            description_offset: None,
        }
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

    #[test]
    fn get_issue_args_accept_an_id_a_string_or_a_batch() {
        let one: GetIssueArgs = serde_json::from_value(json!({"id": 12})).unwrap();
        assert!(matches!(one.id, IssueIds::One(IssueRef::Number(12))));
        assert_eq!(one.view, IssueView::Summary);
        assert!(one.include_checklist);
        assert!(!one.include_watchers);
        assert!(!one.include_field_changes);

        let text: GetIssueArgs =
            serde_json::from_value(json!({"id": "#12", "view": "journals"})).unwrap();
        assert_eq!(text.view, IssueView::Journals);

        let batch: GetIssueArgs = serde_json::from_value(json!({"id": [12, "ticket-9"]})).unwrap();
        assert!(matches!(batch.id, IssueIds::Many(ref ids) if ids.len() == 2));

        assert!(serde_json::from_value::<GetIssueArgs>(json!({"id": 1, "extra": true})).is_err());
    }

    #[tokio::test]
    async fn get_issue_summary_skips_journals_and_keeps_a_missing_checklist() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/issues/9.json"))
            .and(query_param("include", "attachments,children,relations"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "issue": {
                    "id": 9,
                    "subject": "Router drops packets",
                    "description": "Hello body",
                    "status": { "id": 1, "name": "New" },
                    "project": { "id": 1, "name": "Ops" },
                    "journals": [{
                        "id": 1,
                        "notes": "SECRET JOURNAL",
                        "created_on": "2026-01-01T00:00:00Z",
                        "user": { "name": "Ada" }
                    }],
                    "author": { "name": "Ada", "api_key": "secret-key" }
                }
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/issues/9/checklists.json"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&server)
            .await;

        mount_catalog(&server).await;
        let client = client_with(&server.uri(), &[]);
        let args = issue_args(IssueIds::One(IssueRef::Text(format!(
            "{}/issues/9",
            server.uri()
        ))));
        let text = call_issue(&client, args).await.unwrap();
        assert_named(&text);
        assert!(text.contains("Router drops packets"), "{text}");
        assert!(text.contains("Description omitted (10 chars)"), "{text}");
        assert!(text.contains("view=journals"), "{text}");
        assert!(!text.contains("Hello body"), "{text}");
        assert!(!text.contains("SECRET JOURNAL"), "{text}");
        assert!(!text.contains("secret-key"), "{text}");
        assert!(!text.contains("## Checklist"), "{text}");
    }

    #[tokio::test]
    async fn get_issue_journals_are_newest_first_and_include_checklist_ids() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/issues/4.json"))
            .and(query_param("include", "journals"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "issue": {
                    "id": 4,
                    "subject": "History",
                    "journals": [
                        { "notes": "older note", "created_on": "2026-01-01T00:00:00Z", "user": { "name": "Ada" } },
                        { "notes": "newer note", "created_on": "2026-02-01T00:00:00Z", "user": { "name": "Ada" } },
                        { "notes": "", "created_on": "2026-03-01T00:00:00Z", "details": [{ "property": "attr", "name": "status_id", "old_value": "1", "new_value": "2" }] }
                    ]
                }
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/issues/4/checklists.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "checklists": [{ "id": 3, "subject": "Collect logs", "is_done": false, "position": 1 }]
            })))
            .expect(1)
            .mount(&server)
            .await;

        mount_catalog(&server).await;
        let client = client_with(&server.uri(), &[]);
        let mut args = issue_args(IssueIds::One(IssueRef::Number(4)));
        args.view = IssueView::Journals;
        args.include_attachments = false;
        args.include_children = false;
        args.include_relations = false;
        let text = call_issue(&client, args).await.unwrap();
        assert_named(&text);
        let newer = text.find("newer note").unwrap();
        let older = text.find("older note").unwrap();
        assert!(newer < older, "{text}");
        assert!(
            text.contains("1 field-change-only entries hidden"),
            "{text}"
        );
        assert!(!text.contains("status_id"), "{text}");
        assert!(text.contains("- [ ] 3 Collect logs"), "{text}");
    }

    #[tokio::test]
    async fn get_issue_batch_is_one_list_request_and_reports_missing_ids() {
        let server = MockServer::start().await;
        let issue = |id: u64| {
            json!({
                "id": id,
                "subject": format!("Issue {id}"),
                "description": "Please water the roses",
                "status": { "name": "Closed" },
                "tracker": { "name": "Bug" },
                "custom_fields": [{ "id": 36, "name": "Severity Rating", "value": "Minor" }]
            })
        };
        // Redmine returns list results in its own order, not the order asked.
        Mock::given(method("GET"))
            .and(path("/issues.json"))
            .and(query_param("issue_id", "10,9,11"))
            .and(query_param("status_id", "*"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "issues": [issue(9), issue(10)],
                "total_count": 2
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path_regex(r"^/issues/\d+(\.json|/checklists\.json)$"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;

        mount_catalog(&server).await;
        let client = client_with_file(
            &server.uri(),
            r#"{"issue_prefix":"ticket","fields":{"severity":36}}"#,
        );
        let batch = call_issue(
            &client,
            issue_args(IssueIds::Many(vec![
                IssueRef::Number(10),
                IssueRef::Text("#9".to_string()),
                IssueRef::Text("ticket-11".to_string()),
                IssueRef::Number(10),
            ])),
        )
        .await
        .unwrap();
        assert!(batch.contains("| severity |"), "{batch}");
        let first = batch.find("Issue 10").expect("issue 10");
        let second = batch.find("Issue 9").expect("issue 9");
        assert!(first < second, "rows keep the order asked: {batch}");
        assert!(
            batch.contains("Closed"),
            "closed issues are included: {batch}"
        );
        assert!(!batch.contains("water the roses"), "{batch}");
        assert_named(&batch);
        assert!(
            batch.ends_with("Not found, or not visible to this API key: #11."),
            "{batch}"
        );
    }

    #[tokio::test]
    async fn get_issue_slug_uses_the_configured_prefix_and_field_name() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/issues/15.json"))
            .and(query_param_is_missing("include"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "issue": {
                    "id": 15,
                    "subject": "Slug target",
                    "custom_fields": [
                        { "id": 36, "name": "Severity Rating", "value": "Minor" },
                        { "id": 40, "name": "Severity", "value": "Other" }
                    ]
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        mount_catalog(&server).await;
        let client = client_with_file(
            &server.uri(),
            r#"{"issue_prefix":"ticket","fields":{"severity":36}}"#,
        );
        let mut slug = issue_args(IssueIds::One(IssueRef::Text("Ticket-15".to_string())));
        slug.include_checklist = false;
        slug.include_attachments = false;
        slug.include_children = false;
        slug.include_relations = false;
        let one = call_issue(&client, slug).await.unwrap();
        assert_named(&one);
        assert!(one.contains("| severity | Minor |"), "{one}");
        // The catalog gave "Severity" to the configured key, so field 40 is cf_40.
        assert!(one.contains("| cf_40 | Other |"), "{one}");
        assert!(!one.contains("severity_rating"), "{one}");
    }

    #[tokio::test]
    async fn one_project_without_checklists_does_not_hide_them_elsewhere() {
        let server = MockServer::start().await;
        for (id, project) in [(3, 30), (4, 40)] {
            Mock::given(method("GET"))
                .and(path(format!("/issues/{id}.json")))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "issue": { "id": id, "subject": "s", "project": { "id": project, "name": "p" } }
                })))
                .mount(&server)
                .await;
        }
        Mock::given(method("GET"))
            .and(path("/issues/3/checklists.json"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/issues/4/checklists.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "checklists": [{ "id": 8, "subject": "Water the beds", "is_done": false, "position": 1 }]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = client_with(&server.uri(), &[]);
        let ctx = Ctx::new(client);
        let off = get_issue(&ctx, issue_args(IssueIds::One(IssueRef::Number(3))))
            .await
            .unwrap();
        assert!(!off.contains("Checklist"), "{off}");
        // A 404 in project 30 must not hide project 40's checklist.
        let on = get_issue(&ctx, issue_args(IssueIds::One(IssueRef::Number(4))))
            .await
            .unwrap();
        assert!(on.contains("- [ ] 8 Water the beds"), "{on}");
    }

    #[tokio::test]
    async fn a_checklist_server_error_still_returns_the_issue() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/issues/3.json"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"issue": {"id": 3, "subject": "Still here"}})),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/issues/3/checklists.json"))
            .respond_with(ResponseTemplate::new(500))
            .expect(2)
            .mount(&server)
            .await;

        let client = client_with(&server.uri(), &[]);
        let text = call_issue(&client, issue_args(IssueIds::One(IssueRef::Number(3))))
            .await
            .unwrap();
        assert!(text.contains("Still here"), "{text}");
        assert!(text.contains("Checklist could not be loaded"), "{text}");
        assert!(text.contains("HTTP 500"), "{text}");
        // A 500 is not "plugin absent", so the next call asks again.
        call_issue(&client, issue_args(IssueIds::One(IssueRef::Number(3))))
            .await
            .unwrap();
    }

    #[test]
    fn journals_since_takes_a_date_or_a_utc_timestamp_only() {
        assert_eq!(check_since("2026-08-01"), Ok("2026-08-01"));
        assert_eq!(
            check_since("2026-08-01T09:00:00Z"),
            Ok("2026-08-01T09:00:00Z")
        );
        for bad in [
            "2026-8-1",
            "1 Aug 2026",
            "2026-08-01 09:00",
            "2026-08-01T09:00:00+02:00",
            "yesterday",
            "２０２６-08-01",
        ] {
            let err = check_since(bad).unwrap_err();
            assert!(err.to_string().contains("YYYY-MM-DD"), "{bad}: {err}");
        }
    }

    #[tokio::test]
    async fn get_issue_refuses_a_bad_id_an_oversized_batch_and_a_detailed_batch() {
        let client = client_with("https://redmine.example", &[]);
        let bad = call_issue(
            &client,
            issue_args(IssueIds::One(IssueRef::Text("nope".to_string()))),
        )
        .await
        .unwrap_err();
        assert!(bad.to_string().contains("Could not parse"), "{bad}");

        let too_many = call_issue(
            &client,
            issue_args(IssueIds::Many((1..=21).map(IssueRef::Number).collect())),
        )
        .await
        .unwrap_err();
        assert!(
            too_many.to_string().contains("20 ids at most"),
            "{too_many}"
        );

        let mut detailed = issue_args(IssueIds::Many(vec![
            IssueRef::Number(1),
            IssueRef::Number(2),
        ]));
        detailed.view = IssueView::Journals;
        let err = call_issue(&client, detailed).await.unwrap_err();
        assert!(err.to_string().contains("summary rows"), "{err}");

        let mut zero = issue_args(IssueIds::One(IssueRef::Number(1)));
        zero.journals_limit = Some(0);
        let err = call_issue(&client, zero).await.unwrap_err();
        assert!(err.to_string().contains("positive"), "{err}");

        let mut since = issue_args(IssueIds::One(IssueRef::Number(1)));
        since.journals_since = Some("1 Aug 2026".to_string());
        let err = call_issue(&client, since).await.unwrap_err();
        assert!(err.to_string().contains("YYYY-MM-DD"), "{err}");

        let empty = call_issue(&client, issue_args(IssueIds::Many(vec![])))
            .await
            .unwrap_err();
        assert!(empty.to_string().contains("Pass an issue id"), "{empty}");
    }

    #[tokio::test]
    async fn description_offset_is_refused_except_on_view_description() {
        let client = client_with("https://redmine.example", &[]);
        let mut batch = issue_args(IssueIds::Many(vec![IssueRef::Number(1)]));
        batch.description_offset = Some(0);
        let err = call_issue(&client, batch).await.unwrap_err();
        assert!(err.to_string().contains("view=description"), "{err}");

        for view in [IssueView::Summary, IssueView::Journals] {
            let mut args = issue_args(IssueIds::One(IssueRef::Number(1)));
            args.view = view;
            args.description_offset = Some(0);
            let err = call_issue(&client, args).await.unwrap_err();
            assert!(
                err.to_string().contains("view=description"),
                "{view:?} {err}"
            );
        }

        let mut full = issue_args(IssueIds::One(IssueRef::Number(1)));
        full.view = IssueView::Full;
        full.description_offset = Some(12);
        let err = call_issue(&client, full).await.unwrap_err();
        assert!(err.to_string().contains("view=description"), "{err}");
    }

    #[tokio::test]
    async fn description_offset_pages_one_issue_and_rejects_the_end() {
        let server = MockServer::start().await;
        let body = format!("{}\n{}", "a".repeat(30), "b".repeat(20));
        Mock::given(method("GET"))
            .and(path("/issues/9.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "issue": {
                    "id": 9,
                    "subject": "Paged",
                    "description": body,
                    "updated_on": "2026-09-07T13:50:44Z",
                    "status": { "id": 1, "name": "New" },
                    "project": { "id": 1, "name": "Ops" }
                }
            })))
            .mount(&server)
            .await;
        let client = client_with(&server.uri(), &[("REDMINE_MAX_DESCRIPTION_CHARS", "40")]);

        let mut first_args = issue_args(IssueIds::One(IssueRef::Number(9)));
        first_args.view = IssueView::Description;
        first_args.include_attachments = false;
        first_args.include_children = false;
        first_args.include_checklist = false;
        first_args.include_relations = false;
        first_args.format = OutputFormat::Json;
        let first: Value =
            serde_json::from_str(&call_issue(&client, first_args).await.unwrap()).unwrap();
        let next = first["description_next_offset"].as_u64().unwrap();
        assert!(first["description"].as_str().unwrap().ends_with('\n'));
        assert!(first.get("description_truncated").is_none());

        let mut second_args = issue_args(IssueIds::One(IssueRef::Number(9)));
        second_args.view = IssueView::Description;
        second_args.include_attachments = false;
        second_args.include_children = false;
        second_args.include_checklist = false;
        second_args.include_relations = false;
        second_args.format = OutputFormat::Json;
        second_args.description_offset = Some(next);
        let second: Value =
            serde_json::from_str(&call_issue(&client, second_args).await.unwrap()).unwrap();
        assert!(second.get("description_next_offset").is_none(), "{second}");
        assert_eq!(
            format!(
                "{}{}",
                first["description"].as_str().unwrap(),
                second["description"].as_str().unwrap()
            ),
            body
        );

        let total = body.chars().count() as u64;
        for offset in [total, total + 10] {
            let mut at_end = quiet_description(9);
            at_end.description_offset = Some(offset);
            let err = call_issue(&client, at_end).await.unwrap_err();
            assert!(
                err.to_string().contains(&format!("{total} characters")),
                "{err}"
            );
        }

        Mock::given(method("GET"))
            .and(path("/issues/10.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "issue": {
                    "id": 10,
                    "subject": "Empty",
                    "description": "",
                    "project": { "id": 1, "name": "Ops" }
                }
            })))
            .mount(&server)
            .await;
        let empty = call_issue(&client, quiet_description(10)).await.unwrap();
        assert!(!empty.contains("past the end"), "{empty}");
        assert!(!empty.contains("## Description"), "{empty}");

        let mut full = quiet_description(9);
        full.view = IssueView::Full;
        full.description_offset = Some(0);
        let text = call_issue(&client, full).await.unwrap();
        assert!(text.contains("description_next_offset"), "{text}");
    }

    fn quiet_description(id: u64) -> GetIssueArgs {
        let mut args = issue_args(IssueIds::One(IssueRef::Number(id)));
        args.view = IssueView::Description;
        args.include_attachments = false;
        args.include_children = false;
        args.include_checklist = false;
        args.include_relations = false;
        args.format = OutputFormat::Json;
        args
    }

    #[tokio::test]
    async fn get_issue_keeps_the_status_when_redmine_rejects_the_id() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/issues/8.json"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let client = client_with(&server.uri(), &[]);
        let mut args = issue_args(IssueIds::One(IssueRef::Number(8)));
        args.include_checklist = false;
        let err = call_issue(&client, args).await.unwrap_err();
        let ToolError::Redmine(inner) = err else {
            panic!("expected a Redmine error, got {err:?}");
        };
        assert_eq!(inner.status(), Some(404));
    }

    #[tokio::test]
    async fn get_issue_returns_when_the_catalog_cannot_load() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/issues/7.json"))
            .and(query_param_is_missing("include"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "issue": {
                    "id": 7,
                    "subject": "Hose",
                    "custom_fields": [
                        { "id": 36, "name": "Severity Rating", "value": "Minor" },
                        { "id": 40, "name": "Severity", "value": "Wrong" }
                    ]
                }
            })))
            .expect(2)
            .mount(&server)
            .await;
        // Asked once. The second issue read within the backoff skips the catalog.
        Mock::given(method("GET"))
            .and(path("/trackers.json"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&server)
            .await;

        let client = client_with_file(&server.uri(), r#"{"fields":{"severity":36}}"#);
        let ctx = Ctx::new(client);
        let args = || {
            let mut args = issue_args(IssueIds::One(IssueRef::Number(7)));
            args.include_checklist = false;
            args.include_attachments = false;
            args.include_children = false;
            args.include_relations = false;
            args
        };
        for _ in 0..2 {
            let text = get_issue(&ctx, args()).await.unwrap();
            assert!(text.contains("| severity | Minor |"), "{text}");
            assert!(text.contains("| cf_40 | Wrong |"), "{text}");
            assert!(text.contains("_Field names unavailable:"), "{text}");
            assert!(!text.contains("severity_rating"), "{text}");
        }
    }

    #[tokio::test]
    async fn a_config_mismatch_is_one_short_line_on_an_issue() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/issues/7.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "issue": { "id": 7, "subject": "Hose" }
            })))
            .mount(&server)
            .await;
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
        ] {
            Mock::given(method("GET"))
                .and(path(at))
                .respond_with(ResponseTemplate::new(200).set_body_json(body))
                .mount(&server)
                .await;
        }
        // A big instance: the bind error lists every one of these.
        let many = (100..400)
            .map(|id| json!({ "id": id, "name": format!("Field number {id}"), "customized_type": "issue" }))
            .collect::<Vec<_>>();
        Mock::given(method("GET"))
            .and(path("/custom_fields.json"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({ "custom_fields": many })),
            )
            .mount(&server)
            .await;

        let client = client_with_file(&server.uri(), r#"{"fields":{"widget":99}}"#);
        let mut args = issue_args(IssueIds::One(IssueRef::Number(7)));
        args.include_checklist = false;
        args.format = OutputFormat::Json;
        let json = call_issue(&client, args).await.unwrap();
        assert!(json.contains("REDMINE_CONFIG does not match"), "{json}");
        assert!(!json.contains("Field number"), "{json}");
        assert!(json.len() < 300, "{} bytes: {json}", json.len());
    }

    #[test]
    fn text_types_include_structured_suffixes() {
        assert!(is_text_attachment("application/vnd.api+json", "x.bin"));
        assert!(is_text_attachment("image/svg+xml", "x.bin"));
        assert!(is_text_attachment("text/plain; charset=utf-8", "x.bin"));
        assert!(is_text_attachment("application/octet-stream", "build.log"));
        assert!(!is_text_attachment("application/pdf", "x.pdf"));
    }

    // Byte budgets. Each response shape has a ceiling, measured on a full page of
    // realistic records. The ceilings sit about 5% above today's output, so a new
    // column trips them. Raising one should be a decision, not a side effect.

    fn assert_budget(label: &str, text: &str, ceiling: usize) {
        assert!(
            text.len() <= ceiling,
            "{label} is {} bytes, over its {ceiling} byte budget",
            text.len()
        );
    }

    async fn full_page_server() -> MockServer {
        let server = MockServer::start().await;
        let projects = (1..=25).map(full_project).collect::<Vec<_>>();
        mount_json(
            &server,
            "/projects.json",
            json!({ "projects": projects, "total_count": 300 }),
        )
        .await;
        let hits = (1..=25).map(full_hit).collect::<Vec<_>>();
        mount_json(
            &server,
            "/search.json",
            json!({ "results": hits, "total_count": 300 }),
        )
        .await;
        mount_json(
            &server,
            "/attachments/2.json",
            json!({ "attachment": {
                "id": 2,
                "filename": "release-notes-for-the-quarter.txt",
                "filesize": 48213,
                "content_type": "text/plain",
                "description": "Notes for the quarter",
                "content_url": "https://redmine.example/attachments/download/2/release-notes-for-the-quarter.txt",
                "thumbnail_url": "https://redmine.example/attachments/thumbnail/2",
                "author": { "id": 1, "name": "Ada Lovelace" },
                "created_on": "2026-03-04T05:06:07Z"
            }}),
        )
        .await;
        server
    }

    #[tokio::test]
    async fn list_projects_page_budget() {
        let server = full_page_server().await;
        let client = client_with(&server.uri(), &[]);
        let markdown = list_projects(&client, projects_args(None, OutputFormat::Markdown))
            .await
            .unwrap();
        assert_budget("list_projects markdown, 25 rows", &markdown, 1_550);
        let json = list_projects(&client, projects_args(None, OutputFormat::Json))
            .await
            .unwrap();
        assert_budget("list_projects json, 25 rows", &json, 1_500);
    }

    #[tokio::test]
    async fn search_page_budget() {
        let server = full_page_server().await;
        let client = client_with(&server.uri(), &[]);
        let markdown = search(&client, search_args("bug")).await.unwrap();
        assert_budget("search markdown, 25 rows", &markdown, 3_600);
        let mut args = search_args("bug");
        args.format = OutputFormat::Json;
        let json = search(&client, args).await.unwrap();
        assert_budget("search json, 25 rows", &json, 3_550);
    }

    #[tokio::test]
    async fn attachment_metadata_budget() {
        let server = full_page_server().await;
        let client = client_with(&server.uri(), &[]);
        let markdown = get_attachment(&client, attachment_args(2, false, OutputFormat::Markdown))
            .await
            .unwrap();
        assert_budget("get_attachment markdown", &markdown, 270);
        let json = get_attachment(&client, attachment_args(2, false, OutputFormat::Json))
            .await
            .unwrap();
        assert_budget("get_attachment json", &json, 230);
    }
}
