//! Byte-exact description editing.
//!
//! Redmine has no patch API. A write sends the whole description, and the
//! differences page splits that text on whitespace. It keeps every whitespace
//! run except a single space, so a newline is its own token. Send a CRLF body
//! back with LF endings and every line break shows as a change.
//!
//! Callers pass find and replace pairs. Each pair is spliced into the exact
//! string the API returned, so bytes outside an edited span stay as they were.

/// `\r\n` or `\n`. A body with no breaks counts as `\r\n`, because a browser
/// textarea posts CRLF and that is what an empty description would have been.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Eol {
    Crlf,
    Lf,
}

impl Eol {
    fn as_str(self) -> &'static str {
        match self {
            Self::Crlf => "\r\n",
            Self::Lf => "\n",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Crlf => "CRLF",
            Self::Lf => "LF",
        }
    }
}

/// One find and replace. `old_string` must be unique unless `replace_all` is set.
#[derive(Debug, Clone, Copy)]
pub struct DescriptionEdit<'a> {
    pub old_string: &'a str,
    pub new_string: &'a str,
    pub replace_all: bool,
}

/// A splice that landed, after both sides were rewritten to the document ending.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedEdit {
    pub occurrences: usize,
    pub old_string: String,
    pub new_string: String,
}

/// The description after every edit, plus what each one changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditResult {
    pub text: String,
    pub applied: Vec<AppliedEdit>,
    pub eol: Eol,
}

/// A missing match, an ambiguous match, or an edit that changes nothing.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("{0}")]
pub struct EditError(pub String);

/// Counts for a preview. `tokens_touched` is an upper bound on what the
/// differences page will mark, taken from the spliced spans.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChangeSummary {
    pub chars_before: usize,
    pub chars_after: usize,
    pub tokens_after: usize,
    pub tokens_touched: usize,
    pub eol: Eol,
}

/// The document's line ending. See [`Eol`].
pub fn detect_eol(text: &str) -> Eol {
    let mut crlf = 0usize;
    let mut lf = 0usize;
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\r' && bytes.get(i + 1) == Some(&b'\n') {
            crlf += 1;
            i += 2;
        } else if bytes[i] == b'\n' {
            lf += 1;
            i += 1;
        } else {
            i += 1;
        }
    }
    if crlf == 0 && lf == 0 {
        return Eol::Crlf;
    }
    if crlf >= lf { Eol::Crlf } else { Eol::Lf }
}

/// Rewrite every line ending in `text` to `eol`. A `\r\n` pair is one ending.
pub fn to_eol(text: &str, eol: Eol) -> String {
    let ending = eol.as_str();
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push_str(ending);
            }
            '\n' => out.push_str(ending),
            other => out.push(other),
        }
    }
    out
}

/// Apply `edits` in order against the exact source bytes.
///
/// Both sides of every edit are rewritten to the document's line ending first,
/// so an `old_string` copied with LF still matches a CRLF body and the splice
/// still writes CRLF back.
///
/// A missing or ambiguous match is an error. Guessing would let a note describe
/// an edit that never happened, or one that landed in the wrong place.
pub fn apply_edits(source: &str, edits: &[DescriptionEdit<'_>]) -> Result<EditResult, EditError> {
    if edits.is_empty() {
        return Err(EditError("No description edits supplied.".to_string()));
    }
    let eol = detect_eol(source);
    let mut text = source.to_string();
    let mut applied = Vec::with_capacity(edits.len());

    for (index, edit) in edits.iter().enumerate() {
        let label = format!("Edit {}", index + 1);
        let old_str = to_eol(edit.old_string, eol);
        let new_str = to_eol(edit.new_string, eol);

        if old_str.is_empty() {
            return Err(EditError(format!("{label}: old_string is empty.")));
        }
        if old_str == new_str {
            return Err(EditError(format!(
                "{label}: old_string and new_string are identical, so it changes nothing."
            )));
        }

        let found = count_occurrences(&text, &old_str);
        if found == 0 {
            return Err(EditError(format!(
                "{label}: old_string not found in the description. Quote it exactly, including punctuation and any non-ASCII characters. Looked for: \"{}\"",
                clip_snippet(&old_str, 80)
            )));
        }
        if found > 1 && !edit.replace_all {
            return Err(EditError(format!(
                "{label}: old_string appears {found} times. Extend it with surrounding lines so it is unique, or pass replace_all=true to change all {found}. Looked for: \"{}\"",
                clip_snippet(&old_str, 80)
            )));
        }

        let occurrences = if edit.replace_all { found } else { 1 };
        text = if edit.replace_all {
            replace_all(&text, &old_str, &new_str)
        } else {
            replace_first(&text, &old_str, &new_str)
        };
        applied.push(AppliedEdit {
            occurrences,
            old_string: old_str,
            new_string: new_str,
        });
    }

    if text == source {
        return Err(EditError(
            "The edits produced no change to the description.".to_string(),
        ));
    }
    Ok(EditResult { text, applied, eol })
}

/// Redmine's diff tokens: split on whitespace runs, keep the separators, drop a
/// single space. A newline stays, which is why an LF write into a CRLF body
/// repaints the diff. Mirrors the split in `lib/redmine/helpers/diff.rb`.
pub fn redmine_tokens(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return vec![""];
    }
    let mut tokens = Vec::new();
    // `split` keeps an empty string on either side of a leading or trailing run.
    if text.starts_with(is_ws) {
        tokens.push("");
    }
    let mut start = 0;
    let mut prev_ws: Option<bool> = None;
    for (idx, ch) in text.char_indices() {
        let ws = is_ws(ch);
        if prev_ws.is_some_and(|previous| previous != ws) {
            push_token(&mut tokens, &text[start..idx]);
            start = idx;
        }
        prev_ws = Some(ws);
    }
    push_token(&mut tokens, &text[start..]);
    if text.ends_with(is_ws) {
        tokens.push("");
    }
    tokens
}

pub fn summarize(before: &str, after: &str, result: &EditResult) -> ChangeSummary {
    let mut touched = 0;
    for applied in &result.applied {
        let removed = redmine_tokens(&applied.old_string).len();
        let added = redmine_tokens(&applied.new_string).len();
        touched += removed.max(added) * applied.occurrences;
    }
    ChangeSummary {
        chars_before: before.chars().count(),
        chars_after: after.chars().count(),
        tokens_after: redmine_tokens(after).len(),
        tokens_touched: touched,
        eol: result.eol,
    }
}

/// What will be spliced, for a dry run. Nothing here is written.
pub fn render_preview(result: &EditResult, summary: &ChangeSummary) -> String {
    let mut lines = Vec::with_capacity(result.applied.len() * 4 + 1);
    lines.push(format!(
        "{} edit(s), {} → {} chars, ~{} of {} diff tokens touched, line endings {} preserved.",
        result.applied.len(),
        summary.chars_before,
        summary.chars_after,
        summary.tokens_touched,
        summary.tokens_after,
        summary.eol.name()
    ));
    for (i, applied) in result.applied.iter().enumerate() {
        let times = if applied.occurrences > 1 {
            format!(" ({}x)", applied.occurrences)
        } else {
            String::new()
        };
        lines.push(String::new());
        lines.push(format!("**Edit {}**{times}", i + 1));
        lines.push(format!("- `{}`", clip_snippet(&applied.old_string, 160)));
        lines.push(format!("+ `{}`", clip_snippet(&applied.new_string, 160)));
    }
    lines.join("\n")
}

fn is_ws(c: char) -> bool {
    c.is_whitespace()
}

fn push_token<'a>(tokens: &mut Vec<&'a str>, token: &'a str) {
    if token != " " {
        tokens.push(token);
    }
}

fn count_occurrences(haystack: &str, needle: &str) -> usize {
    if needle.is_empty() {
        return 0;
    }
    let mut count = 0;
    let mut from = 0;
    while let Some(at) = haystack[from..].find(needle) {
        count += 1;
        from += at + needle.len();
    }
    count
}

fn replace_all(haystack: &str, needle: &str, replacement: &str) -> String {
    let mut out = String::with_capacity(haystack.len());
    let mut rest = haystack;
    while let Some(at) = rest.find(needle) {
        out.push_str(&rest[..at]);
        out.push_str(replacement);
        rest = &rest[at + needle.len()..];
    }
    out.push_str(rest);
    out
}

fn replace_first(haystack: &str, needle: &str, replacement: &str) -> String {
    let Some(at) = haystack.find(needle) else {
        return haystack.to_string();
    };
    let mut out = String::with_capacity(haystack.len() - needle.len() + replacement.len());
    out.push_str(&haystack[..at]);
    out.push_str(replacement);
    out.push_str(&haystack[at + needle.len()..]);
    out
}

/// One line, for an error or a preview. Newlines and tabs are escaped.
fn clip_snippet(text: &str, max_chars: usize) -> String {
    let mut flat = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' if chars.peek() == Some(&'\n') => {
                chars.next();
                flat.push_str("\\n");
            }
            '\n' => flat.push_str("\\n"),
            '\t' => flat.push_str("\\t"),
            other => flat.push(other),
        }
    }
    if flat.chars().count() <= max_chars {
        return flat;
    }
    let clipped: String = flat.chars().take(max_chars).collect();
    format!("{clipped}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CRLF throughout, a U+2026 ellipsis, two copies of the same bullet, runs
    /// of blank lines, indented continuations, and a trailing blank line. Each
    /// one is a way a careless write ruins the description diff.
    fn smoke() -> String {
        [
            "A made-up issue body for exercising the splice.",
            "",
            "The easer egg will be…",
            "",
            "",
            "h3. 1. User Story",
            "",
            "- As a user of the system,",
            "- I want to bump into the easter egg every once in a while,",
            "",
            "**Scenario 1**: Show the easter egg.",
            "",
            "- Given that it is Easter,",
            "  And the global switch is ON,",
            "- When the user logs in,",
            "",
            "**Scenario 2**: Global switch is off",
            "",
            "- Given that it is Easter,",
            "  And the global switch is OFF,",
            "- When the user logs in,",
            "",
        ]
        .join("\r\n")
    }

    fn once<'a>(old_string: &'a str, new_string: &'a str) -> DescriptionEdit<'a> {
        DescriptionEdit {
            old_string,
            new_string,
            replace_all: false,
        }
    }

    fn crlf_count(text: &str) -> usize {
        text.matches("\r\n").count()
    }

    fn bare_lf_count(text: &str) -> usize {
        let bytes = text.as_bytes();
        let mut count = 0;
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'\n' && (i == 0 || bytes[i - 1] != b'\r') {
                count += 1;
            }
            i += 1;
        }
        count
    }

    #[test]
    fn a_one_word_fix_leaves_every_other_byte_alone() {
        let source = smoke();
        let text = apply_edits(&source, &[once("The easer egg", "The easter egg")])
            .unwrap()
            .text;
        assert_eq!(crlf_count(&text), crlf_count(&source));
        assert_eq!(bare_lf_count(&text), 0, "no bare LF may appear");
        assert!(text.contains('…'), "the U+2026 ellipsis must survive");
        assert!(
            text.ends_with("\r\n"),
            "the trailing blank line must survive"
        );
        assert_eq!(text.replacen("The easter egg", "The easer egg", 1), source);
    }

    #[test]
    fn an_lf_quoted_old_string_still_matches_a_crlf_body_and_writes_crlf_back() {
        let text = apply_edits(
            &smoke(),
            &[once(
                "- Given that it is Easter,\n  And the global switch is ON,",
                "- Given that it is Easter Sunday,\n  And the global switch is ON,",
            )],
        )
        .unwrap()
        .text;
        assert_eq!(bare_lf_count(&text), 0);
        assert!(text.contains("Easter Sunday,\r\n  And the global switch is ON,"));
    }

    #[test]
    fn an_ambiguous_old_string_is_refused() {
        let err = apply_edits(
            &smoke(),
            &[once(
                "- When the user logs in,",
                "- When the user signs in,",
            )],
        )
        .unwrap_err();
        assert!(err.to_string().contains("appears 2 times"), "{err}");
    }

    #[test]
    fn replace_all_takes_both_occurrences() {
        let result = apply_edits(
            &smoke(),
            &[DescriptionEdit {
                old_string: "- When the user logs in,",
                new_string: "- When the user signs in,",
                replace_all: true,
            }],
        )
        .unwrap();
        assert_eq!(result.applied[0].occurrences, 2);
        assert_eq!(result.text.matches("signs in").count(), 2);
        assert!(!result.text.contains("logs in"));
    }

    #[test]
    fn a_missing_old_string_fails_before_anything_is_written() {
        let err = apply_edits(&smoke(), &[once("The easter egg will be...", "x")]).unwrap_err();
        assert!(err.to_string().contains("not found"), "{err}");
    }

    #[test]
    fn a_no_op_edit_is_refused() {
        let err =
            apply_edits(&smoke(), &[once("h3. 1. User Story", "h3. 1. User Story")]).unwrap_err();
        assert!(err.to_string().contains("identical"), "{err}");
    }

    #[test]
    fn edits_apply_in_order_and_can_build_on_each_other() {
        let text = apply_edits(
            &smoke(),
            &[
                once("h3. 1. User Story", "h3. 1. Story"),
                once("h3. 1. Story", "h3. 1. Background"),
            ],
        )
        .unwrap()
        .text;
        assert!(text.contains("h3. 1. Background"));
    }

    #[test]
    fn a_body_past_the_read_path_cap_keeps_its_tail() {
        // The read path truncates at REDMINE_MAX_DESCRIPTION_CHARS, 20000.
        // Splicing uses the untouched bytes, so the tail has to survive.
        let big = format!(
            "START\r\n{}END OF DESCRIPTION\r\n",
            "filler line\r\n".repeat(2000)
        );
        assert!(big.len() > 25_000);
        let text = apply_edits(&big, &[once("START", "BEGIN")]).unwrap().text;
        assert!(
            text.ends_with("END OF DESCRIPTION\r\n"),
            "tail must not be clipped"
        );
        assert_eq!(
            text.len(),
            big.len(),
            "a same-length swap must not shift anything else"
        );
    }

    #[test]
    fn detect_eol_prefers_crlf_for_a_body_with_no_line_breaks() {
        assert_eq!(detect_eol("This is a test issue."), Eol::Crlf);
        assert_eq!(detect_eol("a\r\nb"), Eol::Crlf);
        assert_eq!(detect_eol("a\nb"), Eol::Lf);
        assert_eq!(to_eol("a\nb\r\nc", Eol::Crlf), "a\r\nb\r\nc");
    }

    #[test]
    fn redmine_tokens_keep_newlines_and_drop_single_spaces() {
        assert_eq!(redmine_tokens("a b"), ["a", "b"]);
        assert_eq!(redmine_tokens("a\r\nb"), ["a", "\r\n", "b"]);
        assert_eq!(redmine_tokens("a\nb").len(), 3);
        assert_eq!(redmine_tokens(" a"), ["", "a"]);
        assert_eq!(redmine_tokens("a "), ["a", ""]);
        assert_eq!(redmine_tokens("a  b"), ["a", "  ", "b"]);
        assert_eq!(redmine_tokens(""), [""]);
        assert_eq!(redmine_tokens(" "), ["", ""]);
        assert_eq!(redmine_tokens("\n"), ["", "\n", ""]);
    }

    #[test]
    fn summarize_reports_a_small_touch_for_a_small_edit() {
        let source = smoke();
        let result = apply_edits(&source, &[once("The easer egg", "The easter egg")]).unwrap();
        let summary = summarize(&source, &result.text, &result);
        assert_eq!(summary.eol, Eol::Crlf);
        assert_eq!(summary.tokens_touched, 3);
        assert!(summary.tokens_after > 50);
        assert_eq!(summary.chars_after, summary.chars_before + 1);
    }
}
