/// Issue allowlist. Runs on the id alone, before any request.
/// An empty list means every issue is allowed.
pub fn issue_gate(id: u64, allowed: &[u64]) -> Option<String> {
    if allowed.is_empty() || allowed.contains(&id) {
        return None;
    }
    let list = allowed
        .iter()
        .map(|issue| format!("#{issue}"))
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!(
        "Writes to #{id} are not allowed. REDMINE_WRITE_ISSUES permits: {list}."
    ))
}

/// The parts of a project an allowlist entry can name.
#[derive(Debug, Clone, Copy)]
pub struct ProjectRef<'a> {
    pub id: u64,
    /// The URL slug, such as `web-platform`. Survives a rename, so prefer it.
    pub identifier: &'a str,
    pub name: &'a str,
}

/// Project allowlist. Matches the numeric id, the identifier, or the name, the
/// last two ignoring case. An empty list means every project is allowed.
pub fn project_gate(project: ProjectRef<'_>, allowed: &[String]) -> Option<String> {
    if allowed.is_empty() {
        return None;
    }
    let id = project.id.to_string();
    let ok = allowed.iter().any(|entry| {
        entry == &id
            || entry.eq_ignore_ascii_case(project.identifier)
            || entry.eq_ignore_ascii_case(project.name)
    });
    if ok {
        return None;
    }
    Some(format!(
        "Writes to project \"{}\" ({}, id {}) are not allowed. REDMINE_WRITE_PROJECTS permits: {}.",
        project.name,
        project.identifier,
        project.id,
        allowed.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEV: ProjectRef<'static> = ProjectRef {
        id: 14,
        identifier: "web-platform",
        name: "Dev",
    };

    #[test]
    fn empty_issue_list_allows_everything() {
        assert_eq!(issue_gate(12345, &[]), None);
    }

    #[test]
    fn issue_gate_names_the_allowlist() {
        assert_eq!(issue_gate(101, &[101, 102]), None);
        let refused = issue_gate(12, &[101, 102]).unwrap();
        assert!(refused.contains("#12 are not allowed"));
        assert!(refused.contains("#101, #102"));
    }

    #[test]
    fn project_gate_matches_id_identifier_or_name() {
        for entry in ["14", "web-platform", "WEB-Platform", "dev"] {
            assert_eq!(project_gate(DEV, &[entry.to_string()]), None, "{entry}");
        }
        let refused = project_gate(DEV, &["9".to_string(), "ops".to_string()]).unwrap();
        assert!(refused.contains("REDMINE_WRITE_PROJECTS permits: 9, ops"));
        assert!(refused.contains("\"Dev\" (web-platform, id 14)"));
    }

    #[test]
    fn empty_project_list_allows_everything() {
        assert_eq!(project_gate(DEV, &[]), None);
    }
}
