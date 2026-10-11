//! One write tool.
//!
//! A note, a description edit, and checklist changes go out in one PUT so
//! Redmine files them as one journal entry. Splitting them would let a caller
//! make several requests and detach the explanation from the change.
//!
//! A 204 is not proof. A workflow can drop a field the role cannot edit and
//! still answer success, so the handler refetches and compares.

use rmcp::schemars::{self, JsonSchema};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::checklist::{
    ChecklistError, ChecklistItem, ChecklistOp, ChecklistOpKind, ChecklistPlan, plan_checklist,
    sort_checklist, verify_checklist,
};
use crate::client::Client;
use crate::edit::{
    ChangeSummary, DescriptionEdit, EditResult, Eol, apply_edits, render_preview, summarize, to_eol,
};
use crate::error::ToolError;
use crate::gate::{ProjectRef, issue_gate, project_gate};
use crate::parse::{IdForms, parse_issue_id};
use crate::render::{compact_json, fenced};
use crate::sanitize::sanitize;
use crate::tools::OutputFormat;

/// A number or a string. Models send both, and a string is how a slug or a URL arrives.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum IssueId {
    Number(u64),
    Text(String),
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DescriptionEditArg {
    /// Exact text to find in the current description, copied verbatim. Must be unique unless replace_all is set. Line endings are matched loosely. Everything else is exact.
    pub old_string: String,
    /// Text to put in its place. May be empty, which deletes the matched text.
    pub new_string: String,
    /// Replace every occurrence. Without this, more than one match is an error.
    #[serde(default)]
    pub replace_all: bool,
}

impl DescriptionEditArg {
    fn as_edit(&self) -> DescriptionEdit<'_> {
        DescriptionEdit {
            old_string: &self.old_string,
            new_string: &self.new_string,
            replace_all: self.replace_all,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ChecklistOpName {
    Add,
    Check,
    Uncheck,
    Rename,
    Remove,
}

impl ChecklistOpName {
    fn kind(self) -> ChecklistOpKind {
        match self {
            Self::Add => ChecklistOpKind::Add,
            Self::Check => ChecklistOpKind::Check,
            Self::Uncheck => ChecklistOpKind::Uncheck,
            Self::Rename => ChecklistOpKind::Rename,
            Self::Remove => ChecklistOpKind::Remove,
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChecklistOpArg {
    /// add, check, uncheck, rename, or remove. add appends an unchecked item. The others act on an existing item by id.
    pub op: ChecklistOpName,
    /// Checklist item id from get_issue. Required for everything except add.
    #[serde(default)]
    pub id: Option<u64>,
    /// Item text for add and rename.
    #[serde(default)]
    pub subject: Option<String>,
}

impl ChecklistOpArg {
    fn as_op(&self) -> ChecklistOp<'_> {
        ChecklistOp {
            op: self.op.kind(),
            id: self.id,
            subject: self.subject.as_deref(),
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateIssueArgs {
    /// Issue id, `#123`, an issue URL, or the configured slug. One issue per call.
    pub id: IssueId,
    /// Comment posted with the change. Required when the description changes. It is the text beside the diff.
    #[serde(default)]
    pub notes: Option<String>,
    /// Post the note as private, visible only to roles with that permission.
    #[serde(default)]
    pub private_notes: bool,
    /// Description edits, applied in order. Omit for a note or a checklist change.
    #[serde(default)]
    pub description_edits: Vec<DescriptionEditArg>,
    /// Checklist changes in the same request. Read the issue first. Items are addressed by id.
    #[serde(default)]
    pub checklist: Vec<ChecklistOpArg>,
    /// The issue's updated_on from when you read it. If the issue has changed since, the write is refused.
    #[serde(default)]
    pub expected_updated_on: Option<String>,
    /// Preview the note, the description splice, and the checklist changes without writing.
    #[serde(default)]
    pub dry_run: bool,
    /// Output format.
    #[serde(default)]
    pub format: OutputFormat,
}

pub async fn update_issue(client: &Client, args: UpdateIssueArgs) -> Result<String, ToolError> {
    let id = parse_id(&args.id, id_forms(client))?;
    let notes = args
        .notes
        .as_deref()
        .filter(|notes| !notes.trim().is_empty());
    if !args.description_edits.is_empty() && notes.is_none() {
        return Err(ToolError::refused(
            "notes is required when editing the description. Say what changed and why; it appears next to the diff.",
        ));
    }
    if notes.is_none() && args.description_edits.is_empty() && args.checklist.is_empty() {
        return Err(ToolError::refused(
            "Nothing to do. Pass notes, description_edits, or checklist.",
        ));
    }
    if client.config().read_only && !args.dry_run {
        return Err(ToolError::refused(
            "Redmine MCP is read-only. Set REDMINE_READONLY=false to enable writes, or call again with dry_run=true to preview.",
        ));
    }
    if let Some(message) = issue_gate(id, &client.config().write_issues) {
        return Err(ToolError::refused(message));
    }

    // The issue first: the project allowlist and the per-project checklist
    // cache both need its project.
    let issue = fetch_issue(client, id, None).await?;
    let project_id = refuse_project(id, &issue, &client.config().write_projects)?;
    let checklist = if args.checklist.is_empty() {
        None
    } else if client.checklists_absent(project_id) {
        return Err(checklist_unavailable(id));
    } else {
        Some(read_checklist(client, id, Some(project_id)).await?)
    };
    if let Some(expected) = args.expected_updated_on.as_deref() {
        let found = issue
            .get("updated_on")
            .and_then(Value::as_str)
            .unwrap_or("");
        if found != expected {
            return Err(ToolError::refused(format!(
                "Issue {id} changed since you read it (expected updated_on {expected}, found {found}). Re-read it and rebuild the changes against the current state."
            )));
        }
    }

    // The raw description, not the trimmed text get_issue shows. The splice
    // has to keep the line endings Redmine stored.
    let source = description_text(&issue);
    let edit = if args.description_edits.is_empty() {
        None
    } else {
        let edits = args
            .description_edits
            .iter()
            .map(DescriptionEditArg::as_edit)
            .collect::<Vec<_>>();
        Some(apply_edits(&source, &edits).map_err(|err| ToolError::refused(err.to_string()))?)
    };
    let summary = edit
        .as_ref()
        .map(|edit| summarize(&source, &edit.text, edit));
    let plan = if args.checklist.is_empty() {
        None
    } else {
        let Some(items) = checklist.as_deref() else {
            return Err(checklist_unavailable(id));
        };
        let ops = args
            .checklist
            .iter()
            .map(ChecklistOpArg::as_op)
            .collect::<Vec<_>>();
        Some(plan_checklist(items, &ops).map_err(refused_checklist)?)
    };

    // Redmine splits a private journal that also carries changes: the note stays
    // private, and the changes go into a separate public entry.
    let split = notes.is_some() && args.private_notes && (edit.is_some() || plan.is_some());

    if args.dry_run {
        return Ok(render_dry_run(
            id,
            notes,
            edit.as_ref(),
            summary.as_ref(),
            plan.as_ref(),
            split,
            args.format,
        ));
    }

    let url = issue_url(client, id);
    let body = put_body(notes, args.private_notes, edit.as_ref(), plan.as_ref());
    match client.put_json(&format!("/issues/{id}.json"), &body).await {
        Ok(_) => {}
        // Refused outright: Redmine answered without saving anything.
        Err(err) if matches!(err.status(), Some(401 | 403 | 404 | 422)) => {
            return Err(err.into());
        }
        Err(err) => return Err(maybe_landed(&url, notes.is_some(), &err.to_string())),
    }

    // From here the write has been sent. Any failure has to say so, or a retry
    // posts the note twice.
    let after_fut = fetch_issue(client, id, Some("journals"));
    let checklist_after_fut = async {
        match &plan {
            Some(_) => read_checklist(client, id, None).await.map(Some),
            None => Ok(None),
        }
    };
    let (after, checklist_after) = tokio::join!(after_fut, checklist_after_fut);
    let after = after.map_err(|err| maybe_landed(&url, notes.is_some(), &err.to_string()))?;
    let checklist_after =
        checklist_after.map_err(|err| maybe_landed(&url, notes.is_some(), &err.to_string()))?;

    let journal = note_journal(&after, split);
    let anchor = match journal.and_then(|journal| journal.get("id").and_then(Value::as_u64)) {
        Some(journal_id) => format!("{url}#change-{journal_id}"),
        None => url.clone(),
    };

    let mut failures = Vec::new();
    // Compared with line endings normalised. Current Redmine rewrites every
    // description to CRLF on save, so an LF body comes back CRLF even when the
    // write landed. An ending-only difference is not a dropped edit.
    if let Some(edit) = &edit
        && to_eol(&description_text(&after), Eol::Lf) != to_eol(&edit.text, Eol::Lf)
    {
        let status = after
            .get("status")
            .and_then(|status| status.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("?");
        failures.push(format!(
            "The description did not change. Redmine accepted the request and dropped it, which usually means the workflow marks it read-only for your role at status \"{status}\"."
        ));
    }
    let verified = match (&plan, checklist_after.as_deref()) {
        (Some(plan), Some(items)) => Some(verify_checklist(plan, items)),
        _ => None,
    };
    if let Some(verified) = &verified
        && !verified.problems.is_empty()
    {
        failures.push(format!(
            "The checklist does not match what was sent: {}.",
            verified.problems.join("; ")
        ));
    }
    if !failures.is_empty() {
        let what = if notes.is_some() {
            "The note posted, but not everything it describes happened."
        } else {
            "Redmine accepted the request."
        };
        return Err(ToolError::refused(format!(
            "{what} {} Check {anchor} before retrying.",
            failures.join(" ")
        )));
    }
    if let Some(notes) = notes {
        let posted = journal
            .and_then(|journal| journal.get("notes"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if to_eol(posted.trim(), Eol::Lf) != to_eol(notes.trim(), Eol::Lf) {
            return Err(ToolError::refused(format!(
                "Wrote to #{id} but could not confirm the note in the latest journal entry. Check {url} before retrying, since a retry would post the note twice."
            )));
        }
    }

    let added = verified.map(|verified| verified.added).unwrap_or_default();
    Ok(render_success(
        id,
        Outcome {
            notes,
            summary: summary.as_ref(),
            plan: plan.as_ref(),
            added: &added,
            journal_id: journal.and_then(|journal| journal.get("id").and_then(Value::as_u64)),
            anchor: &anchor,
            split,
        },
        args.format,
    ))
}

/// The write was sent, and what happened next is unknown. Say so plainly.
fn maybe_landed(url: &str, with_note: bool, cause: &str) -> ToolError {
    let retry = if with_note {
        " Do not retry until you have looked: a retry would post the note twice."
    } else {
        " Look before retrying."
    };
    ToolError::refused(format!(
        "The write was sent and may have landed, but it could not be confirmed ({cause}). Check {url}.{retry}"
    ))
}

const SPLIT_NOTE: &str = "The note is private, so Redmine files the description and checklist changes as a separate public journal entry. Anyone who can see the issue sees that diff.";

struct Outcome<'a> {
    notes: Option<&'a str>,
    summary: Option<&'a ChangeSummary>,
    plan: Option<&'a ChecklistPlan>,
    added: &'a [ChecklistItem],
    journal_id: Option<u64>,
    anchor: &'a str,
    split: bool,
}

fn render_dry_run(
    id: u64,
    notes: Option<&str>,
    edit: Option<&EditResult>,
    summary: Option<&ChangeSummary>,
    plan: Option<&ChecklistPlan>,
    split: bool,
    format: OutputFormat,
) -> String {
    match format {
        OutputFormat::Json => {
            let mut out = Map::new();
            out.insert("dry_run".to_string(), json!(true));
            if split {
                out.insert("public_changes".to_string(), json!(SPLIT_NOTE));
            }
            out.insert("issue".to_string(), json!(id));
            if let Some(notes) = notes {
                out.insert("notes".to_string(), json!(notes));
            }
            if let Some(summary) = summary {
                out.insert("summary".to_string(), summary_json(summary));
            }
            if let Some(plan) = plan {
                out.insert("checklist".to_string(), json!(plan.changes));
                out.insert("payload".to_string(), plan.attributes_json());
            }
            compact_json(&Value::Object(out))
        }
        OutputFormat::Markdown => {
            let mut parts = vec![format!("Dry run for #{id}. Nothing written.")];
            parts.push(match notes {
                Some(notes) => format!("Note to post:\n{}", fenced(notes, "")),
                None => "No note.".to_string(),
            });
            if let (Some(edit), Some(summary)) = (edit, summary) {
                parts.push(render_preview(edit, summary));
            }
            if let Some(plan) = plan {
                let lines = plan
                    .changes
                    .iter()
                    .map(|change| format!("- {change}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                parts.push(format!("Checklist changes:\n{lines}"));
            }
            if split {
                parts.push(SPLIT_NOTE.to_string());
            }
            parts.join("\n\n")
        }
    }
}

fn render_success(id: u64, outcome: Outcome<'_>, format: OutputFormat) -> String {
    let line = success_line(outcome.notes, outcome.summary, outcome.plan, outcome.added);
    match format {
        OutputFormat::Markdown if outcome.split => {
            format!("{line} {SPLIT_NOTE}\n{}", outcome.anchor)
        }
        OutputFormat::Markdown => format!("{line}\n{}", outcome.anchor),
        OutputFormat::Json => {
            let mut out = Map::new();
            out.insert("issue".to_string(), json!(id));
            if outcome.split {
                out.insert("public_changes".to_string(), json!(SPLIT_NOTE));
            }
            if let Some(journal_id) = outcome.journal_id {
                out.insert("journal_id".to_string(), json!(journal_id));
            }
            out.insert("url".to_string(), json!(outcome.anchor));
            out.insert("note_posted".to_string(), json!(outcome.notes.is_some()));
            out.insert(
                "description_changed".to_string(),
                json!(outcome.summary.is_some()),
            );
            if let Some(summary) = outcome.summary {
                out.insert("summary".to_string(), summary_json(summary));
            }
            if let Some(plan) = outcome.plan {
                out.insert("checklist".to_string(), json!(plan.changes));
            }
            if outcome.plan.is_some() {
                let ids = outcome.added.iter().map(|item| item.id).collect::<Vec<_>>();
                out.insert("added_item_ids".to_string(), json!(ids));
            }
            compact_json(&Value::Object(out))
        }
    }
}

fn success_line(
    notes: Option<&str>,
    summary: Option<&ChangeSummary>,
    plan: Option<&ChecklistPlan>,
    added: &[ChecklistItem],
) -> String {
    let mut parts = Vec::new();
    if notes.is_some() {
        parts.push("Note posted.".to_string());
    }
    if let Some(summary) = summary {
        parts.push(format!(
            "Description updated ({} → {} chars, ~{} diff tokens touched).",
            summary.chars_before, summary.chars_after, summary.tokens_touched
        ));
    }
    if let Some(plan) = plan {
        parts.push(format!("Checklist: {}.", plan.changes.join("; ")));
        if !added.is_empty() {
            let ids = added
                .iter()
                .map(|item| item.id.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            parts.push(format!("New item ids: {ids}."));
        }
    }
    parts.join(" ")
}

fn summary_json(summary: &ChangeSummary) -> Value {
    json!({
        "chars_before": summary.chars_before,
        "chars_after": summary.chars_after,
        "tokens_after": summary.tokens_after,
        "tokens_touched": summary.tokens_touched,
        "eol": match summary.eol {
            Eol::Crlf => "crlf",
            Eol::Lf => "lf",
        }
    })
}

fn put_body(
    notes: Option<&str>,
    private_notes: bool,
    edit: Option<&EditResult>,
    plan: Option<&ChecklistPlan>,
) -> Value {
    let mut issue = Map::new();
    if let Some(notes) = notes {
        issue.insert("notes".to_string(), json!(notes));
        issue.insert("private_notes".to_string(), json!(private_notes));
    }
    if let Some(edit) = edit {
        issue.insert("description".to_string(), json!(edit.text));
    }
    if let Some(plan) = plan {
        issue.insert("checklists_attributes".to_string(), plan.attributes_json());
    }
    json!({ "issue": issue })
}

fn parse_id(id: &IssueId, forms: IdForms<'_>) -> Result<u64, ToolError> {
    let text = match id {
        IssueId::Number(number) => number.to_string(),
        IssueId::Text(text) => text.clone(),
    };
    parse_issue_id(&text, forms).map_err(ToolError::refused)
}

fn id_forms(client: &Client) -> IdForms<'_> {
    IdForms {
        prefix: client.config().issue_prefix.as_deref(),
        base_url: Some(&client.config().base_url),
    }
}

fn issue_url(client: &Client, id: u64) -> String {
    format!(
        "{}/issues/{id}",
        client.config().base_url.trim_end_matches('/')
    )
}

fn description_text(issue: &Value) -> String {
    issue
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// The journal entry this write created. Journals arrive oldest first, so it is
/// the last one.
///
/// Except after a split: a private note with changes becomes two entries, the
/// note and then a public entry with no notes. Only then is the note one back,
/// and only when the newest entry is that note-less one. Looking further back in
/// any other case would match an older copy of the same note and call a write
/// confirmed that never landed.
fn note_journal(issue: &Value, split: bool) -> Option<&Value> {
    let items = issue.get("journals").and_then(Value::as_array)?;
    let newest = items.last().filter(|journal| journal.is_object())?;
    let newest_has_notes = newest
        .get("notes")
        .and_then(Value::as_str)
        .is_some_and(|notes| !notes.trim().is_empty());
    if split && !newest_has_notes && items.len() >= 2 {
        return items
            .get(items.len() - 2)
            .filter(|journal| journal.is_object());
    }
    Some(newest)
}

fn refuse_project(id: u64, issue: &Value, allowed: &[String]) -> Result<u64, ToolError> {
    let Some(project) = issue.get("project").filter(|project| project.is_object()) else {
        return Err(ToolError::refused(format!(
            "Issue #{id} has no project, so the write allowlist cannot be checked."
        )));
    };
    let Some(project_id) = project.get("id").and_then(Value::as_u64) else {
        return Err(ToolError::refused(format!(
            "Issue #{id} has a project with no id, so the write allowlist cannot be checked."
        )));
    };
    let identifier = project
        .get("identifier")
        .and_then(Value::as_str)
        .unwrap_or("");
    let name = project.get("name").and_then(Value::as_str).unwrap_or("");
    if let Some(message) = project_gate(
        ProjectRef {
            id: project_id,
            identifier,
            name,
        },
        allowed,
    ) {
        return Err(ToolError::refused(message));
    }
    Ok(project_id)
}

fn checklist_unavailable(id: u64) -> ToolError {
    ToolError::refused(format!(
        "Cannot read the checklist on #{id}. The checklist module may be off for this project, or the key lacks permission."
    ))
}

fn refused_checklist(err: ChecklistError) -> ToolError {
    ToolError::refused(err.to_string())
}

async fn fetch_issue(client: &Client, id: u64, include: Option<&str>) -> Result<Value, ToolError> {
    let path = format!("/issues/{id}.json");
    let body = match include {
        Some(include) => {
            client
                .get_json::<Value>(&path, &[("include", include)])
                .await?
        }
        None => client.get_json::<Value>(&path, &[]).await?,
    };
    match sanitize(body).get("issue") {
        Some(issue) if issue.is_object() => Ok(issue.clone()),
        _ => Err(ToolError::refused(format!(
            "Redmine returned no issue for id {id}."
        ))),
    }
}

/// The checklist as it stands. `project_id` is set on the read before a write,
/// so a 403 or 404 marks that project as having no checklist module. The
/// refetch after a write passes `None`: a 403 there must not poison the cache.
async fn read_checklist(
    client: &Client,
    id: u64,
    project_id: Option<u64>,
) -> Result<Vec<ChecklistItem>, ToolError> {
    let path = format!("/issues/{id}/checklists.json");
    let body = match client.get_json::<Value>(&path, &[]).await {
        Ok(body) => sanitize(body),
        Err(err) if matches!(err.status(), Some(403 | 404)) => {
            if let Some(project_id) = project_id {
                client.mark_checklists_absent(project_id);
            }
            return Err(checklist_unavailable(id));
        }
        Err(err) => return Err(err.into()),
    };
    parse_checklist(&body, id)
}

fn parse_checklist(body: &Value, id: u64) -> Result<Vec<ChecklistItem>, ToolError> {
    let Some(items) = body.get("checklists") else {
        return Ok(Vec::new());
    };
    let Some(items) = items.as_array() else {
        return Err(ToolError::refused(format!(
            "Checklist response for #{id} was not a list."
        )));
    };
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let Some(item_id) = item.get("id").and_then(Value::as_u64) else {
            return Err(ToolError::refused(format!(
                "A checklist item on #{id} is missing an id, so it cannot be updated safely."
            )));
        };
        let Some(subject) = item.get("subject").and_then(Value::as_str) else {
            return Err(ToolError::refused(format!(
                "Checklist item {item_id} on #{id} is missing a subject, so it cannot be updated safely."
            )));
        };
        out.push(ChecklistItem {
            id: item_id,
            subject: subject.to_string(),
            is_done: item
                .get("is_done")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            position: item.get("position").and_then(Value::as_u64),
            is_section: item
                .get("is_section")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        });
    }
    Ok(sort_checklist(&out))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use serde_json::json;
    use wiremock::matchers::{method, path, query_param, query_param_is_missing};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::client::Client;
    use crate::config::load_from;
    use crate::issue::description_window;

    const UPDATED: &str = "2026-10-01T00:00:00Z";
    const DESCRIPTION: &str = "The easer egg\r\nkeeps its ending\r\n";
    const SPLICED: &str = "The easter egg\r\nkeeps its ending\r\n";

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

    fn writable(base: &str, extra: &[(&str, &str)]) -> Client {
        let mut pairs = vec![("REDMINE_READONLY", "false")];
        pairs.extend_from_slice(extra);
        client_with(base, &pairs)
    }

    fn args(id: u64) -> UpdateIssueArgs {
        UpdateIssueArgs {
            id: IssueId::Number(id),
            notes: None,
            private_notes: false,
            description_edits: Vec::new(),
            checklist: Vec::new(),
            expected_updated_on: None,
            dry_run: false,
            format: OutputFormat::Markdown,
        }
    }

    fn edit(old_string: &str, new_string: &str) -> DescriptionEditArg {
        DescriptionEditArg {
            old_string: old_string.to_string(),
            new_string: new_string.to_string(),
            replace_all: false,
        }
    }

    fn check(id: u64) -> ChecklistOpArg {
        ChecklistOpArg {
            op: ChecklistOpName::Check,
            id: Some(id),
            subject: None,
        }
    }

    fn issue_json(description: &str, journals: Option<Value>) -> Value {
        let mut issue = json!({
            "id": 7,
            "description": description,
            "updated_on": UPDATED,
            "status": { "id": 1, "name": "New" },
            "project": { "id": 3, "name": "Demo", "identifier": "demo" }
        });
        if let Some(journals) = journals {
            issue["journals"] = journals;
        }
        json!({ "issue": issue })
    }

    fn checklist_json(is_done: bool) -> Value {
        json!({
            "checklists": [{
                "id": 5,
                "subject": "Box",
                "is_done": is_done,
                "position": 1
            }]
        })
    }

    async fn mount_put(server: &MockServer, times: u64) {
        Mock::given(method("PUT"))
            .and(path("/issues/7.json"))
            .respond_with(ResponseTemplate::new(204))
            .expect(times)
            .mount(server)
            .await;
    }

    async fn put_bodies(server: &MockServer) -> Vec<Value> {
        server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|request| request.method.as_str() == "PUT")
            .map(|request| request.body_json().unwrap())
            .collect()
    }

    #[test]
    fn unknown_arguments_are_refused() {
        let err = serde_json::from_value::<UpdateIssueArgs>(json!({"id": 1, "status": "New"}))
            .unwrap_err();
        assert!(err.to_string().contains("unknown field"), "{err}");
    }

    #[tokio::test]
    async fn a_description_edit_needs_notes_and_an_empty_call_does_nothing() {
        let client = writable("https://redmine.example", &[]);
        let mut edit_only = args(7);
        edit_only.description_edits.push(edit("a", "b"));
        let err = update_issue(&client, edit_only).await.unwrap_err();
        assert!(err.to_string().contains("notes is required"), "{err}");

        let mut blank = args(7);
        blank.notes = Some("   ".to_string());
        blank.description_edits.push(edit("a", "b"));
        let err = update_issue(&client, blank).await.unwrap_err();
        assert!(err.to_string().contains("notes is required"), "{err}");

        let err = update_issue(&client, args(7)).await.unwrap_err();
        assert!(err.to_string().contains("Nothing to do"), "{err}");
    }

    #[tokio::test]
    async fn read_only_sends_nothing_and_dry_run_only_gets() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/issues/7.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(issue_json("Hello world", None)))
            .mount(&server)
            .await;
        mount_put(&server, 0).await;

        let client = client_with(&server.uri(), &[]);
        let mut refused = args(7);
        refused.notes = Some("a note".to_string());
        let err = update_issue(&client, refused).await.unwrap_err();
        assert!(err.to_string().contains("dry_run=true"), "{err}");
        assert!(server.received_requests().await.unwrap().is_empty());

        let mut preview = args(7);
        preview.notes = Some("a note".to_string());
        preview.dry_run = true;
        preview.description_edits.push(edit("Hello", "Hi"));
        let text = update_issue(&client, preview).await.unwrap();
        assert!(text.contains("Nothing written"), "{text}");
        assert!(text.contains("CRLF") || text.contains("LF"), "{text}");
        assert!(text.contains("Hello"), "{text}");

        let mut json_preview = args(7);
        json_preview.notes = Some("a note".to_string());
        json_preview.dry_run = true;
        json_preview.format = OutputFormat::Json;
        json_preview.description_edits.push(edit("Hello", "Hi"));
        let json = update_issue(&client, json_preview).await.unwrap();
        assert!(json.contains("\"dry_run\":true"), "{json}");
        assert!(json.contains("\"chars_before\""), "{json}");
        assert!(!json.contains('\n'));
        assert!(put_bodies(&server).await.is_empty());
    }

    #[tokio::test]
    async fn the_issue_allowlist_refuses_before_any_request() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;
        let client = writable(&server.uri(), &[("REDMINE_WRITE_ISSUES", "101")]);
        for dry_run in [false, true] {
            let mut call = args(12);
            call.notes = Some("a note".to_string());
            call.dry_run = dry_run;
            let err = update_issue(&client, call).await.unwrap_err();
            assert!(err.to_string().contains("#12 are not allowed"), "{err}");
        }
    }

    #[tokio::test]
    async fn the_project_allowlist_refuses_after_the_read() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/issues/7.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(issue_json(DESCRIPTION, None)))
            .expect(1)
            .mount(&server)
            .await;
        mount_put(&server, 0).await;
        let client = writable(&server.uri(), &[("REDMINE_WRITE_PROJECTS", "other")]);
        let mut call = args(7);
        call.notes = Some("a note".to_string());
        let err = update_issue(&client, call).await.unwrap_err();
        assert!(err.to_string().contains("REDMINE_WRITE_PROJECTS"), "{err}");
    }

    #[tokio::test]
    async fn expected_updated_on_mismatch_sends_no_put() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/issues/7.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(issue_json(DESCRIPTION, None)))
            .mount(&server)
            .await;
        mount_put(&server, 0).await;
        let client = writable(&server.uri(), &[]);
        let mut call = args(7);
        call.notes = Some("a note".to_string());
        call.expected_updated_on = Some("2020-01-01T00:00:00Z".to_string());
        let err = update_issue(&client, call).await.unwrap_err();
        let message = err.to_string();
        assert!(message.contains("2020-01-01T00:00:00Z"), "{message}");
        assert!(message.contains(UPDATED), "{message}");
    }

    #[tokio::test]
    async fn a_missing_old_string_sends_no_put() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/issues/7.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(issue_json(DESCRIPTION, None)))
            .mount(&server)
            .await;
        mount_put(&server, 0).await;
        let client = writable(&server.uri(), &[]);
        let mut call = args(7);
        call.notes = Some("a note".to_string());
        call.description_edits.push(edit("not in the body", "x"));
        let err = update_issue(&client, call).await.unwrap_err();
        assert!(err.to_string().contains("not found"), "{err}");
    }

    #[tokio::test]
    async fn a_crlf_body_is_spliced_and_the_refetch_must_match() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/issues/7.json"))
            .and(query_param_is_missing("include"))
            .respond_with(ResponseTemplate::new(200).set_body_json(issue_json(DESCRIPTION, None)))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/issues/7.json"))
            .and(query_param("include", "journals"))
            .respond_with(ResponseTemplate::new(200).set_body_json(issue_json(
                SPLICED,
                Some(json!([{ "id": 44, "notes": "fix the typo" }])),
            )))
            .expect(1)
            .mount(&server)
            .await;
        mount_put(&server, 1).await;

        let client = writable(&server.uri(), &[]);
        let mut call = args(7);
        call.notes = Some("fix the typo".to_string());
        call.expected_updated_on = Some(UPDATED.to_string());
        call.description_edits
            .push(edit("The easer egg", "The easter egg"));
        let text = update_issue(&client, call).await.unwrap();
        assert!(text.contains("Note posted."), "{text}");
        assert!(text.contains("Description updated"), "{text}");
        assert!(text.contains("#change-44"), "{text}");

        let bodies = put_bodies(&server).await;
        assert_eq!(bodies.len(), 1);
        assert_eq!(bodies[0]["issue"]["description"], SPLICED);
        assert_eq!(bodies[0]["issue"]["notes"], "fix the typo");
        assert_eq!(bodies[0]["issue"]["private_notes"], false);
        assert!(bodies[0]["issue"].get("checklists_attributes").is_none());
    }

    #[tokio::test]
    async fn a_quote_from_the_second_description_window_is_spliced() {
        let head = format!("{}\r\n", "h".repeat(19_000));
        // No break between this newline and the cap, so the first window stops
        // here. The sentence under test sits in the next window.
        let gap = "x".repeat(20_000 - head.chars().count());
        let marked = "before UNIQUE_SENTENCE_IN_THE_TAIL after\r\n";
        let body = format!("{head}{gap}\r\n{marked}{}", "z\r\n".repeat(200));
        assert!(body.chars().count() > 20_000);
        let first = description_window(body.trim(), 0, 20_000).unwrap();
        assert!(first.text.ends_with('\n'));
        assert!(
            !first.text.contains("UNIQUE_SENTENCE_IN_THE_TAIL"),
            "the marker leaked into the first window"
        );
        let second = description_window(body.trim(), first.next_offset.unwrap(), 20_000).unwrap();
        assert!(second.text.contains("UNIQUE_SENTENCE_IN_THE_TAIL"));
        let spliced = body.replacen("UNIQUE_SENTENCE_IN_THE_TAIL", "UNIQUE_SENTENCE_EDITED", 1);

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/issues/7.json"))
            .and(query_param_is_missing("include"))
            .respond_with(ResponseTemplate::new(200).set_body_json(issue_json(&body, None)))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/issues/7.json"))
            .and(query_param("include", "journals"))
            .respond_with(ResponseTemplate::new(200).set_body_json(issue_json(
                &spliced,
                Some(json!([{ "id": 44, "notes": "fix the sentence past the first window" }])),
            )))
            .expect(1)
            .mount(&server)
            .await;
        mount_put(&server, 1).await;

        let client = writable(&server.uri(), &[]);
        let mut call = args(7);
        call.notes = Some("fix the sentence past the first window".to_string());
        call.expected_updated_on = Some(UPDATED.to_string());
        call.description_edits.push(edit(
            "UNIQUE_SENTENCE_IN_THE_TAIL",
            "UNIQUE_SENTENCE_EDITED",
        ));
        update_issue(&client, call).await.unwrap();

        let bodies = put_bodies(&server).await;
        assert_eq!(bodies[0]["issue"]["description"], spliced);
        assert!(spliced.starts_with(&head));
        assert!(spliced.contains("UNIQUE_SENTENCE_EDITED"));
        assert!(!spliced.contains("UNIQUE_SENTENCE_IN_THE_TAIL"));
    }

    #[tokio::test]
    async fn a_dropped_description_is_an_error_even_after_204() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/issues/7.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(issue_json(
                DESCRIPTION,
                Some(json!([{ "id": 44, "notes": "fix the typo" }])),
            )))
            .mount(&server)
            .await;
        mount_put(&server, 1).await;
        let client = writable(&server.uri(), &[]);
        let mut call = args(7);
        call.notes = Some("fix the typo".to_string());
        call.description_edits
            .push(edit("The easer egg", "The easter egg"));
        let err = update_issue(&client, call).await.unwrap_err();
        let message = err.to_string();
        assert!(message.contains("The note posted"), "{message}");
        assert!(message.contains("did not change"), "{message}");
        assert!(message.contains("status \"New\""), "{message}");
        assert!(message.contains("#change-44"), "{message}");
        assert_eq!(
            put_bodies(&server).await[0]["issue"]["description"],
            SPLICED
        );
    }

    #[tokio::test]
    async fn a_dropped_checklist_tick_is_an_error_even_after_204() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/issues/7.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(issue_json(
                DESCRIPTION,
                Some(json!([{ "id": 9, "notes": "" }])),
            )))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/issues/7/checklists.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(checklist_json(false)))
            .mount(&server)
            .await;
        mount_put(&server, 1).await;
        let client = writable(&server.uri(), &[]);
        let mut call = args(7);
        call.checklist.push(check(5));
        let err = update_issue(&client, call).await.unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("Redmine accepted the request"),
            "{message}"
        );
        assert!(message.contains("does not match"), "{message}");
        assert!(
            message.contains("5 is unchecked, expected checked"),
            "{message}"
        );
        let body = &put_bodies(&server).await[0];
        assert_eq!(
            body["issue"]["checklists_attributes"],
            json!({ "0": { "id": 5, "subject": "Box", "is_done": true } })
        );
        assert!(body["issue"].get("notes").is_none());
    }

    #[tokio::test]
    async fn a_checklist_tick_that_lands_is_confirmed() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/issues/7.json"))
            .and(query_param_is_missing("include"))
            .respond_with(ResponseTemplate::new(200).set_body_json(issue_json(DESCRIPTION, None)))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/issues/7.json"))
            .and(query_param("include", "journals"))
            .respond_with(ResponseTemplate::new(200).set_body_json(issue_json(
                DESCRIPTION,
                Some(json!([{ "id": 9, "notes": "" }])),
            )))
            .expect(1)
            .mount(&server)
            .await;
        // The first mounted mock wins, so the unchecked list is the read and
        // the checked list is the refetch.
        Mock::given(method("GET"))
            .and(path("/issues/7/checklists.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(checklist_json(false)))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/issues/7/checklists.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(checklist_json(true)))
            .expect(1)
            .mount(&server)
            .await;
        mount_put(&server, 1).await;

        let client = writable(&server.uri(), &[]);
        let mut call = args(7);
        call.checklist.push(check(5));
        call.format = OutputFormat::Json;
        let text = update_issue(&client, call).await.unwrap();
        assert!(text.contains("\"journal_id\":9"), "{text}");
        assert!(text.contains("checked 5"), "{text}");
        assert!(text.contains("#change-9"), "{text}");
        assert!(!text.contains('\n'));
    }

    #[tokio::test]
    async fn a_note_missing_from_the_newest_journal_warns_about_a_retry() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/issues/7.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(issue_json(
                DESCRIPTION,
                Some(json!([
                    { "id": 1, "notes": "fix the typo" },
                    { "id": 2, "notes": "a different note" }
                ])),
            )))
            .mount(&server)
            .await;
        mount_put(&server, 1).await;
        let client = writable(&server.uri(), &[]);
        let mut call = args(7);
        call.notes = Some("fix the typo".to_string());
        call.private_notes = true;
        let err = update_issue(&client, call).await.unwrap_err();
        let message = err.to_string();
        assert!(message.contains("post the note twice"), "{message}");
        assert!(message.contains("/issues/7"), "{message}");
        assert!(!message.contains("#change-"), "{message}");
        let body = &put_bodies(&server).await[0];
        assert_eq!(body["issue"]["notes"], "fix the typo");
        assert_eq!(body["issue"]["private_notes"], true);
    }

    #[tokio::test]
    async fn a_missing_checklist_plugin_sends_no_put_and_is_remembered_for_the_project() {
        let server = MockServer::start().await;
        // Read each call: the project is what the cache is keyed on.
        Mock::given(method("GET"))
            .and(path("/issues/7.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(issue_json(DESCRIPTION, None)))
            .expect(2)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/issues/7/checklists.json"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&server)
            .await;
        mount_put(&server, 0).await;
        let client = writable(&server.uri(), &[]);
        let mut call = args(7);
        call.checklist.push(check(5));
        let err = update_issue(&client, call).await.unwrap_err();
        assert!(
            err.to_string().contains("Cannot read the checklist"),
            "{err}"
        );

        let mut again = args(7);
        again.checklist.push(check(5));
        let err = update_issue(&client, again).await.unwrap_err();
        assert!(
            err.to_string().contains("Cannot read the checklist"),
            "{err}"
        );
        let checklist_gets = server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|request| request.url.path().ends_with("/checklists.json"))
            .count();
        assert_eq!(checklist_gets, 1);
    }

    #[tokio::test]
    async fn another_project_still_gets_its_checklist_written() {
        let server = MockServer::start().await;
        let in_project = |project: u64, journals: Option<Value>| {
            let mut body = issue_json(DESCRIPTION, journals);
            body["issue"]["project"] = json!({ "id": project, "name": "p", "identifier": "p" });
            body
        };
        Mock::given(method("GET"))
            .and(path("/issues/7.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(in_project(3, None)))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/issues/7/checklists.json"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/issues/8.json"))
            .and(query_param_is_missing("include"))
            .respond_with(ResponseTemplate::new(200).set_body_json(in_project(4, None)))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/issues/8.json"))
            .and(query_param("include", "journals"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(in_project(4, Some(json!([{ "id": 9, "notes": "" }])))),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/issues/8/checklists.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(checklist_json(false)))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/issues/8/checklists.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(checklist_json(true)))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/issues/8.json"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let client = writable(&server.uri(), &[]);
        let mut off = args(7);
        off.checklist.push(check(5));
        assert!(update_issue(&client, off).await.is_err());
        // Project 3 has no checklist module. Project 4 does.
        let mut on = args(8);
        on.checklist.push(check(5));
        let text = update_issue(&client, on).await.unwrap();
        assert!(text.contains("checked 5"), "{text}");
    }

    #[tokio::test]
    async fn a_failure_after_the_put_says_the_write_may_have_landed() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/issues/7.json"))
            .and(query_param_is_missing("include"))
            .respond_with(ResponseTemplate::new(200).set_body_json(issue_json(DESCRIPTION, None)))
            .mount(&server)
            .await;
        // The refetch fails. The PUT went out, so the note may already be there.
        Mock::given(method("GET"))
            .and(path("/issues/7.json"))
            .and(query_param("include", "journals"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        mount_put(&server, 1).await;
        let client = writable(&server.uri(), &[]);
        let mut call = args(7);
        call.notes = Some("a note".to_string());
        let err = update_issue(&client, call).await.unwrap_err().to_string();
        assert!(err.contains("may have landed"), "{err}");
        assert!(err.contains("post the note twice"), "{err}");
        assert!(err.contains("/issues/7"), "{err}");
    }

    #[tokio::test]
    async fn a_put_that_times_out_may_have_landed_and_a_422_did_not() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/issues/7.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(issue_json(DESCRIPTION, None)))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/issues/7.json"))
            .respond_with(
                ResponseTemplate::new(204).set_delay(std::time::Duration::from_millis(400)),
            )
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/issues/7.json"))
            .respond_with(
                ResponseTemplate::new(422)
                    .set_body_json(json!({ "errors": ["Subject cannot be blank"] })),
            )
            .with_priority(2)
            .mount(&server)
            .await;
        let client = writable(&server.uri(), &[("REDMINE_TIMEOUT_MS", "100")]);
        let mut call = args(7);
        call.notes = Some("a note".to_string());
        let err = update_issue(&client, call).await.unwrap_err().to_string();
        assert!(err.contains("may have landed"), "{err}");

        // A validation error is Redmine saying no. Nothing was saved, so no warning.
        let mut call = args(7);
        call.notes = Some("a note".to_string());
        let err = update_issue(&client, call).await.unwrap_err().to_string();
        assert!(err.contains("Subject cannot be blank"), "{err}");
        assert!(!err.contains("may have landed"), "{err}");
    }

    #[tokio::test]
    async fn an_lf_body_that_comes_back_crlf_counts_as_written() {
        let server = MockServer::start().await;
        let lf = "The easer egg\nkeeps its ending\n";
        Mock::given(method("GET"))
            .and(path("/issues/7.json"))
            .and(query_param_is_missing("include"))
            .respond_with(ResponseTemplate::new(200).set_body_json(issue_json(lf, None)))
            .mount(&server)
            .await;
        // Redmine normalises to CRLF on save.
        Mock::given(method("GET"))
            .and(path("/issues/7.json"))
            .and(query_param("include", "journals"))
            .respond_with(ResponseTemplate::new(200).set_body_json(issue_json(
                SPLICED,
                Some(json!([{ "id": 44, "notes": "fix the typo" }])),
            )))
            .mount(&server)
            .await;
        mount_put(&server, 1).await;
        let client = writable(&server.uri(), &[]);
        let mut call = args(7);
        call.notes = Some("fix the typo".to_string());
        call.description_edits
            .push(edit("The easer egg", "The easter egg"));
        let text = update_issue(&client, call).await.unwrap();
        assert!(text.contains("Description updated"), "{text}");
        // The splice still sent LF: the body's own endings.
        assert_eq!(
            put_bodies(&server).await[0]["issue"]["description"],
            "The easter egg\nkeeps its ending\n"
        );
    }

    #[tokio::test]
    async fn a_private_note_with_changes_says_the_diff_is_public() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/issues/7.json"))
            .and(query_param_is_missing("include"))
            .respond_with(ResponseTemplate::new(200).set_body_json(issue_json(DESCRIPTION, None)))
            .mount(&server)
            .await;
        // Redmine split the private journal: the public changes entry is newest.
        Mock::given(method("GET"))
            .and(path("/issues/7.json"))
            .and(query_param("include", "journals"))
            .respond_with(ResponseTemplate::new(200).set_body_json(issue_json(
                SPLICED,
                Some(json!([
                    { "id": 50, "notes": "internal reason", "private_notes": true },
                    { "id": 51, "notes": null, "private_notes": false }
                ])),
            )))
            .mount(&server)
            .await;
        mount_put(&server, 1).await;
        let client = writable(&server.uri(), &[]);

        let mut preview = args(7);
        preview.notes = Some("internal reason".to_string());
        preview.private_notes = true;
        preview.dry_run = true;
        preview
            .description_edits
            .push(edit("The easer egg", "The easter egg"));
        let text = update_issue(&client, preview).await.unwrap();
        assert!(text.contains("separate public journal entry"), "{text}");

        let mut call = args(7);
        call.notes = Some("internal reason".to_string());
        call.private_notes = true;
        call.description_edits
            .push(edit("The easer egg", "The easter egg"));
        let text = update_issue(&client, call).await.unwrap();
        assert!(text.contains("separate public journal entry"), "{text}");
        // The link goes to the note's entry, not the public one.
        assert!(text.contains("#change-50"), "{text}");
    }
}
