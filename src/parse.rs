use reqwest::Url;

/// What an issue id may look like on this instance.
#[derive(Debug, Clone, Copy, Default)]
pub struct IdForms<'a> {
    /// Slug prefix, as in `ticket-123`. `None` accepts no slug.
    pub prefix: Option<&'a str>,
    /// `REDMINE_URL`. Issue URLs must point at it. `None` accepts no URL.
    pub base_url: Option<&'a str>,
}

/// Forms an issue id can take: `123`, `#123`, an issue URL on this instance, and
/// `{prefix}-123` when a prefix is configured.
pub fn parse_issue_id(raw: &str, forms: IdForms<'_>) -> Result<u64, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("Issue id is empty. Pass a number or an issue URL.".to_string());
    }
    if trimmed.contains("://") {
        return id_from_issue_url(trimmed, forms.base_url);
    }
    if let Some(prefix) = forms.prefix.filter(|prefix| !prefix.is_empty()) {
        let head = format!("{prefix}-");
        // `get`, not indexing: the input can be multibyte, and head.len() may
        // not fall on a char boundary.
        let has_head = trimmed
            .get(..head.len())
            .is_some_and(|start| start.eq_ignore_ascii_case(&head));
        if has_head {
            let rest = &trimmed[head.len()..];
            if is_digits(rest) {
                return id_from_digits(rest);
            }
        }
    }
    let bare = trimmed.strip_prefix('#').unwrap_or(trimmed);
    if is_digits(bare) {
        return id_from_digits(bare);
    }
    Err(format!(
        "Could not parse issue id from \"{trimmed}\". {}",
        expected_forms(forms)
    ))
}

pub fn expected_forms(forms: IdForms<'_>) -> String {
    let mut list = vec!["123".to_string(), "#123".to_string()];
    if let Some(prefix) = forms.prefix.filter(|prefix| !prefix.is_empty()) {
        list.push(format!("{prefix}-123"));
    }
    if let Some(base) = forms.base_url {
        list.push(format!("an issue URL on {base}"));
    }
    let last = list.pop().unwrap_or_default();
    if list.len() == 1 {
        format!("Expected {} or {last}.", list[0])
    } else {
        format!("Expected {}, or {last}.", list.join(", "))
    }
}

/// An issue URL counts only when it is on this instance: same host and port, under
/// the base path. Otherwise a link from another Redmine would quietly name an
/// unrelated issue here, which matters most in the write allowlist.
fn id_from_issue_url(raw: &str, base: Option<&str>) -> Result<u64, String> {
    let Some(base_raw) = base else {
        return Err(format!(
            "\"{raw}\" is a URL, and issue URLs need REDMINE_URL to check against."
        ));
    };
    let not_here = || format!("\"{raw}\" is not an issue URL on {base_raw}.");
    let url = Url::parse(raw).map_err(|_| not_here())?;
    let base = Url::parse(base_raw).map_err(|_| not_here())?;
    if url.host_str() != base.host_str() || url.port() != base.port() {
        return Err(not_here());
    }
    let issues_root = format!("{}/issues/", base.path().trim_end_matches('/'));
    let rest = url.path().strip_prefix(&issues_root).ok_or_else(not_here)?;
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    let digits = &rest[..end];
    if digits.is_empty() || !matches!(rest[end..].chars().next(), None | Some('/' | '.')) {
        return Err(not_here());
    }
    id_from_digits(digits)
}

fn is_digits(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())
}

fn id_from_digits(digits: &str) -> Result<u64, String> {
    match digits.parse::<u64>() {
        Ok(id) if id > 0 => Ok(id),
        _ => Err(format!("Invalid issue id \"{digits}\".")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: IdForms<'static> = IdForms {
        prefix: None,
        base_url: Some("https://redmine.example/redmine"),
    };

    #[test]
    fn accepts_the_plain_forms() {
        assert_eq!(parse_issue_id("123", BASE).unwrap(), 123);
        assert_eq!(parse_issue_id("  #123 ", BASE).unwrap(), 123);
        assert_eq!(parse_issue_id("00012", BASE).unwrap(), 12);
    }

    #[test]
    fn accepts_an_issue_url_on_this_instance() {
        let ok = [
            ("https://redmine.example/redmine/issues/103", 103),
            ("https://redmine.example/redmine/issues/9/edit", 9),
            ("https://redmine.example/redmine/issues/9.json", 9),
            ("https://redmine.example/redmine/issues/9#note-3", 9),
            ("http://REDMINE.example/redmine/issues/9?tab=history", 9),
        ];
        for (raw, id) in ok {
            assert_eq!(parse_issue_id(raw, BASE), Ok(id), "{raw}");
        }
    }

    #[test]
    fn rejects_urls_and_paths_that_are_not_this_instance() {
        let refused = [
            "https://other.example/redmine/issues/77",
            "https://redmine.example:8443/redmine/issues/77",
            "https://redmine.example/issues/77",
            "https://redmine.example/redmine/projects/x/issues/77",
            "https://redmine.example/redmine/issues/77abc",
        ];
        for raw in refused {
            let err = parse_issue_id(raw, BASE).unwrap_err();
            assert!(err.contains("is not an issue URL on"), "{raw}: {err}");
        }
        assert!(parse_issue_id("see /issues/5", BASE).is_err());
        let no_base = parse_issue_id("https://redmine.example/issues/5", IdForms::default());
        assert!(no_base.unwrap_err().contains("need REDMINE_URL"));
    }

    #[test]
    fn slug_needs_a_prefix_and_matches_regardless_of_case() {
        let forms = IdForms {
            prefix: Some("ticket"),
            ..BASE
        };
        assert!(parse_issue_id("ticket-15", BASE).is_err());
        assert_eq!(parse_issue_id("Ticket-15", forms).unwrap(), 15);
        assert!(parse_issue_id("ticket-", forms).is_err());
    }

    #[test]
    fn multibyte_input_is_an_error_not_a_panic() {
        let forms = IdForms {
            prefix: Some("ticket"),
            ..BASE
        };
        assert!(parse_issue_id("ticketé5", forms).is_err());
        assert!(parse_issue_id("tické-5", forms).is_err());
        assert!(parse_issue_id("１２３", forms).is_err());
    }

    #[test]
    fn rejects_empty_zero_and_junk() {
        assert!(parse_issue_id("  ", BASE).unwrap_err().contains("empty"));
        assert!(parse_issue_id("0", BASE).is_err());
        assert!(parse_issue_id("#0", BASE).is_err());
        let err = parse_issue_id("nope", BASE).unwrap_err();
        assert!(err.contains("nope"));
        assert!(err.contains("Expected 123, #123, or an issue URL on"));
        assert_eq!(expected_forms(IdForms::default()), "Expected 123 or #123.");
    }
}
