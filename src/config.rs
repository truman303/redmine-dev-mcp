use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

use crate::error::ConfigError;
use crate::parse::{IdForms, expected_forms, parse_issue_id};

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum FieldRef {
    Id(u64),
    Label(String),
}

#[derive(Clone, PartialEq, Eq)]
pub struct Config {
    pub base_url: String,
    pub api_key: String,
    pub read_only: bool,
    pub write_projects: Vec<String>,
    pub write_issues: Vec<u64>,
    pub timeout: Duration,
    pub max_journals: u64,
    pub max_note_chars: u64,
    pub max_description_chars: u64,
    pub max_detail_chars: u64,
    pub max_attachment_bytes: u64,
    pub max_list: u64,
    pub max_scan: u64,
    pub scan_ttl: Duration,
    pub catalog_ttl: Duration,
    pub issue_prefix: Option<String>,
    pub fields: BTreeMap<String, FieldRef>,
    pub status_groups: BTreeMap<String, Vec<String>>,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("base_url", &self.base_url)
            .field("api_key", &"<redacted>")
            .field("read_only", &self.read_only)
            .field("write_projects", &self.write_projects)
            .field("write_issues", &self.write_issues)
            .field("timeout", &self.timeout)
            .field("max_journals", &self.max_journals)
            .field("max_note_chars", &self.max_note_chars)
            .field("max_description_chars", &self.max_description_chars)
            .field("max_detail_chars", &self.max_detail_chars)
            .field("max_attachment_bytes", &self.max_attachment_bytes)
            .field("max_list", &self.max_list)
            .field("max_scan", &self.max_scan)
            .field("scan_ttl", &self.scan_ttl)
            .field("catalog_ttl", &self.catalog_ttl)
            .field("issue_prefix", &self.issue_prefix)
            .field("fields", &self.fields)
            .field("status_groups", &self.status_groups)
            .finish()
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct InstanceFile {
    #[serde(default)]
    issue_prefix: Option<String>,
    #[serde(default)]
    fields: BTreeMap<String, FieldRef>,
    #[serde(default)]
    status_groups: BTreeMap<String, Vec<String>>,
}

/// Names the tools use for their own parameters, list columns and facet
/// dimensions. A promoted field with one of these keys would collide with them.
pub(crate) const RESERVED_FIELD_KEYS: &[&str] = &[
    "assignee",
    "author",
    "category",
    "closed",
    "created",
    "created_on",
    "custom_fields",
    "done",
    "format",
    "group_by",
    "id",
    "include_description",
    "include_fields",
    "limit",
    "offset",
    "priority",
    "project",
    "project_id",
    "query_id",
    "sort",
    "status",
    "subject",
    "top",
    "tracker",
    "updated",
    "updated_on",
    "version",
];

/// Redmine's `api_offset_and_limit` clamps `limit` to this.
pub const REDMINE_PAGE_MAX: u64 = 100;

/// Built-in status groups. A configured group cannot redefine them.
const RESERVED_STATUS_GROUPS: &[&str] = &["open", "closed"];

struct Instance {
    issue_prefix: Option<String>,
    fields: BTreeMap<String, FieldRef>,
    status_groups: BTreeMap<String, Vec<String>>,
}

pub fn load() -> Result<Config, ConfigError> {
    let env: HashMap<String, String> = std::env::vars().collect();
    load_from(&env, read_config_file)
}

pub(crate) fn load_from(
    env: &HashMap<String, String>,
    read_file: impl Fn(&Path) -> Result<String, String>,
) -> Result<Config, ConfigError> {
    let mut errors = Vec::new();

    let base_url = match var(env, "REDMINE_URL") {
        Some(raw) => normalize_base_url(raw, &mut errors),
        None => {
            errors.push("REDMINE_URL is required".to_string());
            String::new()
        }
    };
    let api_key = match var(env, "REDMINE_API_KEY") {
        Some(raw) => normalize_api_key(raw, &mut errors),
        None => {
            errors.push("REDMINE_API_KEY is required".to_string());
            String::new()
        }
    };

    let read_only = match var(env, "REDMINE_READONLY") {
        None => true,
        Some(raw) => !raw.eq_ignore_ascii_case("false"),
    };
    let write_projects = split_csv(var(env, "REDMINE_WRITE_PROJECTS"));
    let write_issues_raw = split_csv(var(env, "REDMINE_WRITE_ISSUES"));

    let timeout = duration_ms("REDMINE_TIMEOUT_MS", env, 15_000, &mut errors);
    let max_journals = positive_u64("REDMINE_MAX_JOURNALS", env, 50, &mut errors);
    let max_note_chars = positive_u64("REDMINE_MAX_NOTE_CHARS", env, 6_000, &mut errors);
    let max_description_chars =
        positive_u64("REDMINE_MAX_DESCRIPTION_CHARS", env, 20_000, &mut errors);
    let max_detail_chars = positive_u64("REDMINE_MAX_DETAIL_CHARS", env, 160, &mut errors);
    let max_attachment_bytes =
        positive_u64("REDMINE_MAX_ATTACHMENT_BYTES", env, 102_400, &mut errors);
    let max_list = positive_u64("REDMINE_MAX_LIST", env, 100, &mut errors);
    // Redmine clamps an API page to 100 rows. A higher cap would make every page
    // heading claim rows that never arrive.
    if max_list > REDMINE_PAGE_MAX {
        errors.push(format!(
            "REDMINE_MAX_LIST must be at most {REDMINE_PAGE_MAX}, Redmine's own page limit"
        ));
    }
    let max_scan = positive_u64("REDMINE_MAX_SCAN", env, 5_000, &mut errors);
    let scan_ttl = duration_ms("REDMINE_SCAN_TTL_MS", env, 60_000, &mut errors);
    let catalog_ttl = duration_ms("REDMINE_CATALOG_TTL_MS", env, 600_000, &mut errors);

    let instance = match load_instance(env, &read_file, &mut errors) {
        Some(instance) => instance,
        None => {
            if !write_issues_raw.is_empty() {
                errors.push(
                    "REDMINE_WRITE_ISSUES was not checked because the config file failed to load."
                        .to_string(),
                );
            }
            return fail(errors);
        }
    };

    let forms = IdForms {
        prefix: instance.issue_prefix.as_deref(),
        base_url: Some(base_url.as_str()).filter(|url| !url.is_empty()),
    };
    let mut write_issues = Vec::new();
    for entry in &write_issues_raw {
        match parse_issue_id(entry, forms) {
            Ok(id) if !write_issues.contains(&id) => write_issues.push(id),
            Ok(_) => {}
            Err(err) => errors.push(format!(
                "REDMINE_WRITE_ISSUES: {err} {}",
                expected_forms(forms).replace("Expected", "Use")
            )),
        }
    }

    if !errors.is_empty() {
        return fail(errors);
    }

    Ok(Config {
        base_url,
        api_key,
        read_only,
        write_projects,
        write_issues,
        timeout,
        max_journals,
        max_note_chars,
        max_description_chars,
        max_detail_chars,
        max_attachment_bytes,
        max_list,
        max_scan,
        scan_ttl,
        catalog_ttl,
        issue_prefix: instance.issue_prefix,
        fields: instance.fields,
        status_groups: instance.status_groups,
    })
}

fn fail(errors: Vec<String>) -> Result<Config, ConfigError> {
    Err(ConfigError(format!(
        "Invalid configuration:\n{}",
        errors
            .iter()
            .map(|error| format!("  - {error}"))
            .collect::<Vec<_>>()
            .join("\n")
    )))
}

fn read_config_file(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            format!("config file not found: {}", path.display())
        } else {
            format!("could not read config file {}: {err}", path.display())
        }
    })
}

fn load_instance(
    env: &HashMap<String, String>,
    read_file: &impl Fn(&Path) -> Result<String, String>,
    errors: &mut Vec<String>,
) -> Option<Instance> {
    let Some(path) = var(env, "REDMINE_CONFIG") else {
        return Some(Instance {
            issue_prefix: None,
            fields: BTreeMap::new(),
            status_groups: BTreeMap::new(),
        });
    };
    // MCP clients start servers from whatever directory they like (Claude Desktop
    // uses `/`), so a relative path would work under `cargo run` and fail in use.
    if !Path::new(path).is_absolute() {
        errors.push(format!(
            "REDMINE_CONFIG must be an absolute path, got \"{path}\""
        ));
        return None;
    }
    let text = match read_file(Path::new(path)) {
        Ok(text) => text,
        Err(err) => {
            errors.push(format!("REDMINE_CONFIG: {err}"));
            return None;
        }
    };
    let file: InstanceFile = match serde_json::from_str(&text) {
        Ok(file) => file,
        Err(err) => {
            errors.push(format!("REDMINE_CONFIG: {err}"));
            return None;
        }
    };
    match validate_instance(file) {
        Ok(instance) => Some(instance),
        Err(err) => {
            errors.push(format!("REDMINE_CONFIG: {err}"));
            None
        }
    }
}

fn validate_instance(file: InstanceFile) -> Result<Instance, String> {
    let issue_prefix = match file.issue_prefix {
        None => None,
        Some(prefix) => Some(validate_prefix(&prefix)?),
    };
    for (key, value) in &file.fields {
        validate_name("field", key)?;
        if RESERVED_FIELD_KEYS.contains(&key.to_ascii_lowercase().as_str()) {
            return Err(format!(
                "field \"{key}\" is a built-in parameter name. Pick another key."
            ));
        }
        match value {
            FieldRef::Id(0) => return Err(format!("field \"{key}\" has id 0")),
            FieldRef::Id(_) => {}
            FieldRef::Label(label) if label.trim().is_empty() => {
                return Err(format!("field \"{key}\" has an empty label"));
            }
            // Untagged, so "2" would be looked up as a label and miss field 2.
            FieldRef::Label(label) if label.bytes().all(|b| b.is_ascii_digit()) => {
                return Err(format!(
                    "field \"{key}\" has the label \"{label}\". For the field id, write {label} without quotes."
                ));
            }
            FieldRef::Label(_) => {}
        }
    }
    for (group, statuses) in &file.status_groups {
        validate_name("status group", group)?;
        if RESERVED_STATUS_GROUPS.contains(&group.to_ascii_lowercase().as_str()) {
            return Err(format!(
                "status group \"{group}\" is built in and cannot be redefined"
            ));
        }
        if statuses.is_empty() {
            return Err(format!("status group \"{group}\" has no statuses"));
        }
        for status in statuses {
            if status.trim().is_empty() {
                return Err(format!("status group \"{group}\" has an empty status name"));
            }
        }
    }
    Ok(Instance {
        issue_prefix,
        fields: file.fields,
        status_groups: file.status_groups,
    })
}

fn validate_prefix(prefix: &str) -> Result<String, String> {
    let ok = !prefix.is_empty()
        && !prefix.starts_with('-')
        && !prefix.ends_with('-')
        && prefix
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if !ok {
        return Err(
            "issue_prefix must be letters, digits, '_' or '-', and must not start or end with '-'"
                .to_string(),
        );
    }
    Ok(prefix.to_string())
}

fn validate_name(kind: &str, name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && !name.starts_with('-')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if ok {
        Ok(())
    } else {
        Err(format!(
            "{kind} \"{name}\" must be letters, digits, '_' or '-'"
        ))
    }
}

fn normalize_base_url(raw: &str, errors: &mut Vec<String>) -> String {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        errors.push("REDMINE_URL is required".to_string());
        return String::new();
    }
    let Ok(url) = reqwest::Url::parse(trimmed) else {
        errors.push(format!("REDMINE_URL is not a URL: {trimmed}"));
        return String::new();
    };
    if url.scheme() != "http" && url.scheme() != "https" {
        errors.push("REDMINE_URL must be http or https".to_string());
        return String::new();
    }
    if !url.username().is_empty() || url.password().is_some() {
        errors.push("REDMINE_URL must not include a username or password".to_string());
        return String::new();
    }
    if url.host_str().is_none() {
        errors.push(format!("REDMINE_URL is not a URL: {trimmed}"));
        return String::new();
    }
    if url.query().is_some() || url.fragment().is_some() {
        errors.push("REDMINE_URL must not include a query or fragment".to_string());
        return String::new();
    }
    trimmed.to_string()
}

fn normalize_api_key(raw: &str, errors: &mut Vec<String>) -> String {
    if raw.is_empty()
        || raw
            .bytes()
            .any(|b| b.is_ascii_whitespace() || !b.is_ascii())
    {
        errors.push("REDMINE_API_KEY must be a single-line ASCII token".to_string());
        return String::new();
    }
    raw.to_string()
}

/// An env var with surrounding whitespace removed. Blank counts as unset.
fn var<'a>(env: &'a HashMap<String, String>, name: &str) -> Option<&'a str> {
    env.get(name)
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
}

fn split_csv(raw: Option<&str>) -> Vec<String> {
    raw.unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect()
}

fn duration_ms(
    name: &str,
    env: &HashMap<String, String>,
    default: u64,
    errors: &mut Vec<String>,
) -> Duration {
    Duration::from_millis(positive_u64(name, env, default, errors))
}

fn positive_u64(
    name: &str,
    env: &HashMap<String, String>,
    default: u64,
    errors: &mut Vec<String>,
) -> u64 {
    match var(env, name) {
        None => default,
        Some(raw) => match raw.parse::<u64>() {
            Ok(value) if value > 0 => value,
            _ => {
                errors.push(format!("{name} must be a positive integer, got \"{raw}\""));
                default
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    fn load_env(pairs: &[(&str, &str)]) -> Result<Config, ConfigError> {
        load_from(&env(pairs), |_| unreachable!("no config file"))
    }

    fn load_with_file(pairs: &[(&str, &str)], file: &str) -> Result<Config, ConfigError> {
        let text = file.to_string();
        load_from(&env(pairs), move |_| Ok(text.clone()))
    }

    fn required() -> Vec<(&'static str, &'static str)> {
        vec![
            ("REDMINE_URL", "https://redmine.example/"),
            ("REDMINE_API_KEY", "secret-key"),
        ]
    }

    #[test]
    fn requires_url_and_key_together() {
        let err = load_env(&[]).unwrap_err().to_string();
        assert!(err.contains("REDMINE_URL is required"));
        assert!(err.contains("REDMINE_API_KEY is required"));
    }

    #[test]
    fn defaults_are_read_only_and_the_documented_caps() {
        let config = load_env(&required()).unwrap();
        assert_eq!(config.base_url, "https://redmine.example");
        assert!(config.read_only);
        assert!(config.write_projects.is_empty());
        assert!(config.write_issues.is_empty());
        assert_eq!(config.timeout, Duration::from_millis(15_000));
        assert_eq!(config.max_journals, 50);
        assert_eq!(config.max_note_chars, 6_000);
        assert_eq!(config.max_description_chars, 20_000);
        assert_eq!(config.max_detail_chars, 160);
        assert_eq!(config.max_attachment_bytes, 102_400);
        assert_eq!(config.max_list, 100);
        assert_eq!(config.max_scan, 5_000);
        assert_eq!(config.scan_ttl, Duration::from_millis(60_000));
        assert_eq!(config.catalog_ttl, Duration::from_millis(600_000));
        assert!(config.issue_prefix.is_none());
        assert!(format!("{config:?}").contains("<redacted>"));
        assert!(!format!("{config:?}").contains("secret-key"));
    }

    #[test]
    fn false_is_the_only_way_to_enable_writes() {
        let mut pairs = required();
        pairs.push(("REDMINE_READONLY", "false"));
        assert!(!load_env(&pairs).unwrap().read_only);

        let mut pairs = required();
        pairs.push(("REDMINE_READONLY", "FALSE"));
        assert!(!load_env(&pairs).unwrap().read_only);

        let mut pairs = required();
        pairs.push(("REDMINE_READONLY", "true"));
        assert!(load_env(&pairs).unwrap().read_only);
    }

    #[test]
    fn write_issues_accept_every_form_and_a_bad_entry_stops_startup() {
        let mut pairs = required();
        pairs.push((
            "REDMINE_WRITE_ISSUES",
            "101, #102, https://redmine.example/issues/103, 101",
        ));
        assert_eq!(load_env(&pairs).unwrap().write_issues, vec![101, 102, 103]);

        let mut pairs = required();
        pairs.push(("REDMINE_WRITE_ISSUES", "101, 1O2"));
        let err = load_env(&pairs).unwrap_err().to_string();
        assert!(err.contains("\"1O2\""), "{err}");

        let mut pairs = required();
        pairs.push((
            "REDMINE_WRITE_ISSUES",
            "https://other-redmine.example/issues/103",
        ));
        let err = load_env(&pairs).unwrap_err().to_string();
        assert!(err.contains("not an issue URL on"), "{err}");
    }

    #[test]
    fn slug_allowlist_needs_the_prefix_from_the_json_file() {
        let mut pairs = required();
        pairs.push(("REDMINE_CONFIG", "/etc/redmine.json"));
        pairs.push(("REDMINE_WRITE_ISSUES", "ticket-15"));
        let err = load_with_file(&pairs, "{}").unwrap_err().to_string();
        assert!(err.contains("\"ticket-15\""), "{err}");

        let file = r#"{"issue_prefix":"ticket","fields":{"product":2,"customer":"Customer"},"status_groups":{"in-review":["Review: Waiting","Review: In progress"]}}"#;
        let config = load_with_file(&pairs, file).unwrap();
        assert_eq!(config.issue_prefix.as_deref(), Some("ticket"));
        assert_eq!(config.write_issues, vec![15]);
        assert_eq!(config.fields["product"], FieldRef::Id(2));
        assert_eq!(
            config.fields["customer"],
            FieldRef::Label("Customer".to_string())
        );
        assert_eq!(
            config.status_groups["in-review"],
            vec![
                "Review: Waiting".to_string(),
                "Review: In progress".to_string()
            ]
        );
    }

    #[test]
    fn broken_config_file_stops_startup() {
        let mut pairs = required();
        pairs.push(("REDMINE_CONFIG", "/etc/redmine.json"));
        pairs.push(("REDMINE_WRITE_ISSUES", "15"));
        let missing = load_from(&env(&pairs), |_| {
            Err("config file not found: redmine.json".to_string())
        })
        .unwrap_err()
        .to_string();
        assert!(missing.contains("config file not found"));
        assert!(missing.contains("REDMINE_WRITE_ISSUES was not checked"));

        let unknown = load_with_file(&pairs, r#"{"nope":true}"#)
            .unwrap_err()
            .to_string();
        assert!(unknown.contains("unknown field"));

        let empty_group = load_with_file(&pairs, r#"{"status_groups":{"in-review":[]}}"#)
            .unwrap_err()
            .to_string();
        assert!(empty_group.contains("has no statuses"));
    }

    #[test]
    fn rejects_a_zero_cap_and_a_url_with_credentials() {
        let mut pairs = required();
        pairs.push(("REDMINE_TIMEOUT_MS", "0"));
        assert!(
            load_env(&pairs)
                .unwrap_err()
                .to_string()
                .contains("REDMINE_TIMEOUT_MS")
        );

        let pairs = [
            ("REDMINE_URL", "https://user:secret@redmine.example"),
            ("REDMINE_API_KEY", "secret-key"),
        ];
        assert!(
            load_env(&pairs)
                .unwrap_err()
                .to_string()
                .contains("must not include a username or password")
        );
    }

    #[test]
    fn config_path_must_be_absolute() {
        let mut pairs = required();
        pairs.push(("REDMINE_CONFIG", "redmine.json"));
        let err = load_with_file(&pairs, "{}").unwrap_err().to_string();
        assert!(err.contains("must be an absolute path"), "{err}");
    }

    #[test]
    fn reserved_names_and_quoted_ids_are_refused() {
        let mut pairs = required();
        pairs.push(("REDMINE_CONFIG", "/etc/redmine.json"));
        let cases = [
            (r#"{"fields":{"status":2}}"#, "built-in parameter name"),
            (
                r#"{"fields":{"Custom_Fields":2}}"#,
                "built-in parameter name",
            ),
            (
                r#"{"status_groups":{"Open":["New"]}}"#,
                "cannot be redefined",
            ),
            (r#"{"fields":{"product":"2"}}"#, "write 2 without quotes"),
        ];
        for (file, expected) in cases {
            let err = load_with_file(&pairs, file).unwrap_err().to_string();
            assert!(err.contains(expected), "{file}: {err}");
        }
    }

    #[test]
    fn env_values_are_trimmed() {
        let pairs = [
            ("REDMINE_URL", " https://redmine.example "),
            ("REDMINE_API_KEY", "secret-key\n"),
            ("REDMINE_READONLY", " false "),
            ("REDMINE_TIMEOUT_MS", "15000 "),
        ];
        let config = load_env(&pairs).unwrap();
        assert_eq!(config.api_key, "secret-key");
        assert!(!config.read_only);
        assert_eq!(config.timeout, Duration::from_millis(15_000));
    }

    #[test]
    fn max_list_cannot_pass_redmines_page_limit() {
        let mut pairs = required();
        pairs.push(("REDMINE_MAX_LIST", "150"));
        let err = load_env(&pairs).unwrap_err().to_string();
        assert!(err.contains("at most 100"), "{err}");
    }
}
