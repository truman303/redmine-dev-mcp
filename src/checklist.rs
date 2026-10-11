//! Checklist changes for one issue update.
//!
//! The checklist plugin can update items one at a time. Each of those calls is
//! its own journal entry, and a failure halfway leaves the list half-changed.
//! Sending `checklists_attributes` on the issue PUT keeps every item change in
//! the same entry as the note.
//!
//! Items are addressed by id. Two items often share a prefix, so matching on
//! the subject text would guess.

use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value, json};

/// One row from `/issues/:id/checklists.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChecklistItem {
    pub id: u64,
    pub subject: String,
    pub is_done: bool,
    /// Absent when the payload omits it. Sorting treats that as 0.
    pub position: Option<u64>,
    pub is_section: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChecklistOpKind {
    Add,
    Check,
    Uncheck,
    Rename,
    Remove,
}

impl ChecklistOpKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Check => "check",
            Self::Uncheck => "uncheck",
            Self::Rename => "rename",
            Self::Remove => "remove",
        }
    }
}

/// One change. `id` is required except for [`ChecklistOpKind::Add`].
#[derive(Debug, Clone, Copy)]
pub struct ChecklistOp<'a> {
    pub op: ChecklistOpKind,
    pub id: Option<u64>,
    pub subject: Option<&'a str>,
}

/// One entry inside `checklists_attributes`. Absent fields are left out of the
/// JSON. A check with only `id` and `is_done` comes back as 204 and changes
/// nothing, so a check carries the current subject too.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChecklistAttribute {
    pub id: Option<u64>,
    pub subject: Option<String>,
    pub is_done: Option<bool>,
    pub position: Option<u64>,
    pub destroy: Option<bool>,
}

/// What the refetch should look like. `kept` is every item that stays, with
/// this call's edits applied, not only the items the ops named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpectedChecklist {
    pub before_ids: Vec<u64>,
    pub kept: Vec<KeptItem>,
    pub removed: Vec<u64>,
    pub added: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeptItem {
    pub id: u64,
    pub subject: String,
    pub is_done: bool,
}

/// The body to send, the lines for a preview, and the state to check afterwards.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChecklistPlan {
    pub attributes: Vec<ChecklistAttribute>,
    pub changes: Vec<String>,
    pub expected: ExpectedChecklist,
}

impl ChecklistPlan {
    /// `{"0": {...}, "1": {...}}`. The array form gets HTTP 500 and writes nothing.
    pub fn attributes_json(&self) -> Value {
        let mut map = Map::new();
        for (index, attr) in self.attributes.iter().enumerate() {
            map.insert(index.to_string(), attr.to_json());
        }
        Value::Object(map)
    }
}

impl ChecklistAttribute {
    fn to_json(&self) -> Value {
        let mut map = Map::new();
        if let Some(id) = self.id {
            map.insert("id".to_string(), json!(id));
        }
        if let Some(subject) = &self.subject {
            map.insert("subject".to_string(), json!(subject));
        }
        if let Some(is_done) = self.is_done {
            map.insert("is_done".to_string(), json!(is_done));
        }
        if let Some(position) = self.position {
            map.insert("position".to_string(), json!(position));
        }
        if self.destroy == Some(true) {
            map.insert("_destroy".to_string(), json!(true));
        }
        Value::Object(map)
    }
}

/// An unknown id, a no-op, a blank subject, or an item both changed and removed.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("{0}")]
pub struct ChecklistError(pub String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChecklistVerification {
    pub problems: Vec<String>,
    /// New rows from the refetch, in the order the adds were requested.
    pub added: Vec<ChecklistItem>,
}

/// Position, then id. The plugin allows two items at the same position.
pub fn sort_checklist(items: &[ChecklistItem]) -> Vec<ChecklistItem> {
    let mut out = items.to_vec();
    out.sort_by(|a, b| {
        a.position
            .unwrap_or(0)
            .cmp(&b.position.unwrap_or(0))
            .then(a.id.cmp(&b.id))
    });
    out
}

/// Check `ops` against the list as it is now and build the payload.
///
/// An unknown id, a no-op, or an item both changed and removed is an error.
/// A refused call costs a re-read. A wrong write leaves a journal entry that
/// then has to be explained.
pub fn plan_checklist(
    current: &[ChecklistItem],
    ops: &[ChecklistOp<'_>],
) -> Result<ChecklistPlan, ChecklistError> {
    if ops.is_empty() {
        return Err(ChecklistError(
            "No checklist operations supplied.".to_string(),
        ));
    }

    let by_id = current
        .iter()
        .map(|item| (item.id, item))
        .collect::<HashMap<_, _>>();
    let mut updates: Vec<ChecklistAttribute> = Vec::new();
    let mut removed = Vec::new();
    let mut added: Vec<(String, u64)> = Vec::new();
    let mut changes = Vec::new();
    // A new item with no position lands at position 1, beside the first item.
    let mut next_position = current
        .iter()
        .map(|item| item.position.unwrap_or(0))
        .max()
        .unwrap_or(0)
        + 1;

    for (index, op) in ops.iter().enumerate() {
        let label = format!("Checklist op {} ({})", index + 1, op.op.as_str());
        if op.op == ChecklistOpKind::Add {
            let subject = require_subject(op.subject, &label)?;
            added.push((subject.clone(), next_position));
            next_position += 1;
            changes.push(format!("added \"{}\"", clip(&subject, 60)));
            continue;
        }

        let Some(id) = op.id else {
            return Err(ChecklistError(format!(
                "{label}: id is required. Read the issue with get_issue to see checklist item ids."
            )));
        };
        let Some(item) = by_id.get(&id).copied() else {
            return Err(ChecklistError(format!(
                "{label}: item {id} is not on this issue's checklist. {}",
                known_ids(current)
            )));
        };
        if removed.contains(&id) {
            return Err(ChecklistError(format!(
                "{label}: item {id} is already being removed in this call."
            )));
        }

        if op.op == ChecklistOpKind::Remove {
            if updates.iter().any(|attr| attr.id == Some(id)) {
                return Err(ChecklistError(format!(
                    "{label}: item {id} is also changed in this call. Change it or remove it, not both."
                )));
            }
            removed.push(id);
            changes.push(format!("removed {id} \"{}\"", clip(&item.subject, 60)));
            continue;
        }

        let subject = shown_subject(&updates, item);
        let done = shown_done(&updates, item);
        if op.op == ChecklistOpKind::Check || op.op == ChecklistOpKind::Uncheck {
            if item.is_section {
                return Err(ChecklistError(format!(
                    "{label}: item {id} is a section heading and cannot be checked."
                )));
            }
            let want = op.op == ChecklistOpKind::Check;
            if done == want {
                let state = if want { "checked" } else { "unchecked" };
                return Err(ChecklistError(format!(
                    "{label}: item {id} is already {state}, so this changes nothing."
                )));
            }
            let slot = ensure_update(&mut updates, item);
            updates[slot].is_done = Some(want);
            let verb = if want { "checked" } else { "unchecked" };
            changes.push(format!("{verb} {id} \"{}\"", clip(&subject, 60)));
            continue;
        }

        let next = require_subject(op.subject, &label)?;
        if next == subject {
            return Err(ChecklistError(format!(
                "{label}: item {id} already reads \"{}\".",
                clip(&next, 60)
            )));
        }
        let slot = ensure_update(&mut updates, item);
        updates[slot].subject = Some(next.clone());
        changes.push(format!(
            "renamed {id} \"{}\" to \"{}\"",
            clip(&subject, 60),
            clip(&next, 60)
        ));
    }

    let mut attributes = updates;
    for id in &removed {
        attributes.push(ChecklistAttribute {
            id: Some(*id),
            subject: None,
            is_done: None,
            position: None,
            destroy: Some(true),
        });
    }
    for (subject, position) in &added {
        attributes.push(ChecklistAttribute {
            id: None,
            subject: Some(subject.clone()),
            is_done: Some(false),
            position: Some(*position),
            destroy: None,
        });
    }

    let kept = current
        .iter()
        .filter(|item| !removed.contains(&item.id))
        .map(|item| KeptItem {
            id: item.id,
            subject: shown_subject(&attributes, item),
            is_done: shown_done(&attributes, item),
        })
        .collect();

    Ok(ChecklistPlan {
        attributes,
        changes,
        expected: ExpectedChecklist {
            before_ids: current.iter().map(|item| item.id).collect(),
            kept,
            removed,
            added: added.into_iter().map(|(subject, _)| subject).collect(),
        },
    })
}

/// Compare the refetched list with the plan. A 204 is not proof the write landed.
pub fn verify_checklist(plan: &ChecklistPlan, after: &[ChecklistItem]) -> ChecklistVerification {
    let mut problems = Vec::new();
    let after_by_id = after
        .iter()
        .map(|item| (item.id, item))
        .collect::<HashMap<_, _>>();

    for kept in &plan.expected.kept {
        match after_by_id.get(&kept.id) {
            None => problems.push(format!(
                "item {} \"{}\" disappeared",
                kept.id,
                clip(&kept.subject, 60)
            )),
            Some(item) => {
                if item.subject != kept.subject {
                    problems.push(format!(
                        "item {} reads \"{}\", expected \"{}\"",
                        kept.id,
                        clip(&item.subject, 60),
                        clip(&kept.subject, 60)
                    ));
                }
                if item.is_done != kept.is_done {
                    let actual = if item.is_done { "checked" } else { "unchecked" };
                    let want = if kept.is_done { "checked" } else { "unchecked" };
                    problems.push(format!("item {} is {actual}, expected {want}", kept.id));
                }
            }
        }
    }
    for id in &plan.expected.removed {
        if after_by_id.contains_key(id) {
            problems.push(format!("item {id} was not removed"));
        }
    }

    let before = plan
        .expected
        .before_ids
        .iter()
        .copied()
        .collect::<HashSet<_>>();
    let mut fresh = after
        .iter()
        .filter(|item| !before.contains(&item.id))
        .collect::<Vec<_>>();
    let mut added = Vec::new();
    for subject in &plan.expected.added {
        match fresh.iter().position(|item| item.subject == *subject) {
            None => problems.push(format!("new item \"{}\" did not appear", clip(subject, 60))),
            Some(at) => {
                added.push(fresh[at].clone());
                fresh.remove(at);
            }
        }
    }
    ChecklistVerification { problems, added }
}

fn known_ids(current: &[ChecklistItem]) -> String {
    if current.is_empty() {
        return "This issue has no checklist items.".to_string();
    }
    let mut listed = current
        .iter()
        .take(40)
        .map(|item| item.id.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    if current.len() > 40 {
        listed.push_str(", ...");
    }
    format!("Items on this issue: {listed}.")
}

fn require_subject(subject: Option<&str>, label: &str) -> Result<String, ChecklistError> {
    match subject.map(str::trim).filter(|text| !text.is_empty()) {
        Some(text) => Ok(text.to_string()),
        None => Err(ChecklistError(format!(
            "{label}: subject is required and cannot be blank."
        ))),
    }
}

fn shown_subject(updates: &[ChecklistAttribute], item: &ChecklistItem) -> String {
    updates
        .iter()
        .find(|attr| attr.id == Some(item.id))
        .and_then(|attr| attr.subject.clone())
        .unwrap_or_else(|| item.subject.clone())
}

fn shown_done(updates: &[ChecklistAttribute], item: &ChecklistItem) -> bool {
    updates
        .iter()
        .find(|attr| attr.id == Some(item.id))
        .and_then(|attr| attr.is_done)
        .unwrap_or(item.is_done)
}

fn ensure_update(updates: &mut Vec<ChecklistAttribute>, item: &ChecklistItem) -> usize {
    if let Some(index) = updates.iter().position(|attr| attr.id == Some(item.id)) {
        return index;
    }
    updates.push(ChecklistAttribute {
        id: Some(item.id),
        subject: Some(item.subject.clone()),
        is_done: None,
        position: None,
        destroy: None,
    });
    updates.len() - 1
}

fn clip(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let clipped = flat.chars().take(max).collect::<String>();
    format!("{clipped}...")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Longer than the 60-character clip, so a check has to copy the real subject.
    const READER: &str = "@Reader@: Updated and all new classes added and documented in full";
    const VERSION: &str = "@VersionTask@: Updated";
    const IMAGE: &str = "@Image@: Remove @version()@ and add @parts()@";

    fn item(id: u64, subject: &str, is_done: bool, position: u64) -> ChecklistItem {
        ChecklistItem {
            id,
            subject: subject.to_string(),
            is_done,
            position: Some(position),
            is_section: false,
        }
    }

    fn current() -> Vec<ChecklistItem> {
        vec![
            item(101, READER, true, 1),
            item(102, VERSION, false, 2),
            item(103, IMAGE, false, 3),
        ]
    }

    fn op(kind: ChecklistOpKind, id: Option<u64>, subject: Option<&str>) -> ChecklistOp<'_> {
        ChecklistOp {
            op: kind,
            id,
            subject,
        }
    }

    fn updated(id: u64, subject: &str, is_done: bool) -> ChecklistAttribute {
        ChecklistAttribute {
            id: Some(id),
            subject: Some(subject.to_string()),
            is_done: Some(is_done),
            position: None,
            destroy: None,
        }
    }

    fn appended(subject: &str, position: u64) -> ChecklistAttribute {
        ChecklistAttribute {
            id: None,
            subject: Some(subject.to_string()),
            is_done: Some(false),
            position: Some(position),
            destroy: None,
        }
    }

    #[test]
    fn the_payload_is_an_index_keyed_object() {
        let plan = plan_checklist(
            &current(),
            &[
                op(ChecklistOpKind::Check, Some(102), None),
                op(ChecklistOpKind::Add, None, Some("Migrate callers")),
            ],
        )
        .unwrap();
        let body = plan.attributes_json();
        assert!(matches!(body, Value::Object(_)), "{body}");
        let keys = body
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(keys, ["0", "1"]);
        assert_eq!(
            body,
            json!({
                "0": { "id": 102, "subject": VERSION, "is_done": true },
                "1": { "subject": "Migrate callers", "is_done": false, "position": 4 }
            })
        );
    }

    #[test]
    fn new_items_append_after_the_highest_position() {
        let plan = plan_checklist(
            &current(),
            &[
                op(ChecklistOpKind::Add, None, Some("First new")),
                op(ChecklistOpKind::Add, None, Some("  Second new  ")),
            ],
        )
        .unwrap();
        assert_eq!(
            plan.attributes,
            vec![appended("First new", 4), appended("Second new", 5)]
        );
    }

    #[test]
    fn check_and_uncheck_carry_the_current_subject() {
        let plan = plan_checklist(
            &current(),
            &[
                op(ChecklistOpKind::Check, Some(102), None),
                op(ChecklistOpKind::Uncheck, Some(101), None),
            ],
        )
        .unwrap();
        assert_eq!(
            plan.attributes,
            vec![updated(102, VERSION, true), updated(101, READER, false)]
        );
    }

    #[test]
    fn check_and_rename_on_one_item_merge() {
        let plan = plan_checklist(
            &current(),
            &[
                op(ChecklistOpKind::Check, Some(102), None),
                op(
                    ChecklistOpKind::Rename,
                    Some(102),
                    Some("@VersionTask@: Updated and tested"),
                ),
            ],
        )
        .unwrap();
        assert_eq!(
            plan.attributes,
            vec![updated(102, "@VersionTask@: Updated and tested", true)]
        );
        assert_eq!(plan.changes.len(), 2);
    }

    #[test]
    fn remove_sends_destroy_and_nothing_else() {
        let plan =
            plan_checklist(&current(), &[op(ChecklistOpKind::Remove, Some(103), None)]).unwrap();
        assert_eq!(
            plan.attributes_json(),
            json!({ "0": { "id": 103, "_destroy": true } })
        );
        assert_eq!(plan.expected.removed, vec![103]);
        assert_eq!(plan.expected.kept.len(), 2);
    }

    #[test]
    fn an_id_from_another_issue_is_refused_and_the_real_ids_are_listed() {
        let err = plan_checklist(&current(), &[op(ChecklistOpKind::Check, Some(9999), None)])
            .unwrap_err();
        let message = err.to_string();
        assert!(message.contains("9999 is not on"), "{message}");
        assert!(message.contains("101, 102, 103"), "{message}");
    }

    #[test]
    fn ops_other_than_add_need_an_id() {
        let err =
            plan_checklist(&current(), &[op(ChecklistOpKind::Check, None, None)]).unwrap_err();
        assert!(err.to_string().contains("id is required"), "{err}");
    }

    #[test]
    fn a_no_op_is_refused() {
        let checked =
            plan_checklist(&current(), &[op(ChecklistOpKind::Check, Some(101), None)]).unwrap_err();
        assert!(checked.to_string().contains("already checked"), "{checked}");
        let unchecked =
            plan_checklist(&current(), &[op(ChecklistOpKind::Uncheck, Some(102), None)])
                .unwrap_err();
        assert!(
            unchecked.to_string().contains("already unchecked"),
            "{unchecked}"
        );
        let renamed = plan_checklist(
            &current(),
            &[op(ChecklistOpKind::Rename, Some(102), Some(VERSION))],
        )
        .unwrap_err();
        assert!(renamed.to_string().contains("already reads"), "{renamed}");
    }

    #[test]
    fn an_item_cannot_be_both_changed_and_removed() {
        let changed_then_removed = plan_checklist(
            &current(),
            &[
                op(ChecklistOpKind::Check, Some(102), None),
                op(ChecklistOpKind::Remove, Some(102), None),
            ],
        )
        .unwrap_err();
        assert!(
            changed_then_removed.to_string().contains("not both"),
            "{changed_then_removed}"
        );
        let removed_then_changed = plan_checklist(
            &current(),
            &[
                op(ChecklistOpKind::Remove, Some(102), None),
                op(ChecklistOpKind::Check, Some(102), None),
            ],
        )
        .unwrap_err();
        assert!(
            removed_then_changed
                .to_string()
                .contains("already being removed"),
            "{removed_then_changed}"
        );
    }

    #[test]
    fn blank_subjects_are_refused() {
        let added =
            plan_checklist(&current(), &[op(ChecklistOpKind::Add, None, Some("   "))]).unwrap_err();
        assert!(added.to_string().contains("subject is required"), "{added}");
        let renamed = plan_checklist(&current(), &[op(ChecklistOpKind::Rename, Some(102), None)])
            .unwrap_err();
        assert!(
            renamed.to_string().contains("subject is required"),
            "{renamed}"
        );
    }

    #[test]
    fn section_headings_cannot_be_checked() {
        let mut items = current();
        items.push(ChecklistItem {
            id: 200,
            subject: "Testing".to_string(),
            is_done: false,
            position: Some(4),
            is_section: true,
        });
        let err =
            plan_checklist(&items, &[op(ChecklistOpKind::Check, Some(200), None)]).unwrap_err();
        assert!(err.to_string().contains("section heading"), "{err}");
    }

    #[test]
    fn verify_passes_when_the_refetch_matches() {
        let plan = plan_checklist(
            &current(),
            &[
                op(ChecklistOpKind::Check, Some(102), None),
                op(ChecklistOpKind::Remove, Some(103), None),
                op(ChecklistOpKind::Add, None, Some("Migrate callers")),
            ],
        )
        .unwrap();
        let after = vec![
            current()[0].clone(),
            item(102, VERSION, true, 2),
            item(300, "Migrate callers", false, 4),
        ];
        let verified = verify_checklist(&plan, &after);
        assert!(verified.problems.is_empty(), "{:?}", verified.problems);
        assert_eq!(
            verified
                .added
                .iter()
                .map(|item| item.id)
                .collect::<Vec<_>>(),
            vec![300]
        );
    }

    #[test]
    fn verify_catches_a_dropped_check_a_surviving_removal_and_a_missing_add() {
        let plan = plan_checklist(
            &current(),
            &[
                op(ChecklistOpKind::Check, Some(102), None),
                op(ChecklistOpKind::Remove, Some(103), None),
                op(ChecklistOpKind::Add, None, Some("Migrate callers")),
            ],
        )
        .unwrap();
        let verified = verify_checklist(&plan, &current());
        assert_eq!(verified.problems.len(), 3);
        let joined = verified.problems.join("\n");
        assert!(
            joined.contains("102 is unchecked, expected checked"),
            "{joined}"
        );
        assert!(joined.contains("103 was not removed"), "{joined}");
        assert!(
            joined.contains("\"Migrate callers\" did not appear"),
            "{joined}"
        );
    }

    #[test]
    fn sort_checklist_orders_by_position_then_id() {
        let sorted = sort_checklist(&[
            item(15, "B", false, 1),
            item(14, "A", false, 1),
            item(6, "C", false, 2),
        ]);
        assert_eq!(
            sorted.iter().map(|item| item.id).collect::<Vec<_>>(),
            vec![14, 15, 6]
        );
    }
}
