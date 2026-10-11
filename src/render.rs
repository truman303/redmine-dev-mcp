/// Clip on characters, never past `max` including the ellipsis. A byte index
/// would panic on a multibyte title.
pub fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Wrap untrusted text in a code fence it cannot close. Without one, a file
/// with its own headings would run on as if it were part of the tool's reply.
pub fn fenced(text: &str, info: &str) -> String {
    let mut longest = 0;
    let mut run = 0;
    for c in text.chars() {
        run = if c == '`' { run + 1 } else { 0 };
        longest = longest.max(run);
    }
    let fence = "`".repeat(longest.max(2) + 1);
    let newline = if text.ends_with('\n') { "" } else { "\n" };
    format!("{fence}{info}\n{text}{newline}{fence}")
}

pub fn bytes_to_human(bytes: u64) -> String {
    if bytes == 0 {
        return "0 B".to_string();
    }
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < units.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit > 0 && value < 10.0 {
        format!("{value:.1} {}", units[unit])
    } else {
        format!("{value:.0} {}", units[unit])
    }
}

pub fn markdown_table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let header = format!("| {} |", headers.join(" | "));
    let rule = format!(
        "| {} |",
        headers
            .iter()
            .map(|_| "---")
            .collect::<Vec<_>>()
            .join(" | ")
    );
    let body = rows
        .iter()
        .map(|row| {
            let cells = row.iter().map(|cell| escape_cell(cell)).collect::<Vec<_>>();
            format!("| {} |", cells.join(" | "))
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("{header}\n{rule}\n{body}")
}

fn escape_cell(value: &str) -> String {
    value.replace('|', "\\|").replace(['\n', '\r'], " ")
}

pub fn page_heading(
    label: &str,
    offset: u64,
    count: usize,
    total: Option<u64>,
    limit: u64,
) -> String {
    let total_label = total
        .map(|total| format!(" of {total}"))
        .unwrap_or_default();
    format!(
        "{label} {} to {}{total_label} (limit {limit}):",
        offset + 1,
        offset + count as u64
    )
}

/// `serde_json::Value` always serialises, so this cannot fail.
pub fn compact_json(value: &serde_json::Value) -> String {
    value.to_string()
}

/// One page of rows as `{"columns":[..],"rows":[[..]],...}`. Column names appear
/// once, not on every row, which is most of the saving over an array of objects.
pub fn columnar_json(
    columns: &[&str],
    rows: Vec<Vec<serde_json::Value>>,
    page: serde_json::Map<String, serde_json::Value>,
) -> String {
    let mut out = serde_json::Map::new();
    out.insert("columns".to_string(), serde_json::json!(columns));
    out.insert("rows".to_string(), serde_json::Value::from(rows));
    out.extend(page);
    compact_json(&serde_json::Value::Object(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_counts_characters_not_bytes() {
        let title = "é".repeat(100);
        let clipped = clip(&title, 90);
        assert!(clipped.ends_with('…'));
        assert_eq!(clipped.chars().count(), 90);
        assert_eq!(clip("short", 90), "short");
        assert!(std::str::from_utf8(clipped.as_bytes()).is_ok());
    }

    #[test]
    fn table_escapes_pipes_and_newlines() {
        let table = markdown_table(&["name"], &[vec!["a|b\nc".to_string()]]);
        assert!(table.contains("a\\|b c"));
        assert!(!table.contains("a|b"));
    }

    #[test]
    fn fence_outruns_backticks_in_the_content() {
        assert_eq!(fenced("hello", ""), "```\nhello\n```");
        let tricky = "## Heading\n````\nnot a fence\n````\n";
        let out = fenced(tricky, "text");
        assert!(out.starts_with("`````text\n"), "{out}");
        assert!(out.ends_with("\n`````"), "{out}");
    }
}
