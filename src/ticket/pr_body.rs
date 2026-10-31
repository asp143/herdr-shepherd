use std::sync::LazyLock;

use regex::Regex;

use crate::{
    error::ShepherdError,
    store::{Severity, Ticket},
};

pub fn build_title(t: &Ticket) -> String {
    if t.id.is_empty() {
        t.title.clone()
    } else {
        format!("{}: {}", t.id, t.title)
    }
}

/// Remaining Mid/Low findings are listed as not addressed. Callers should remove
/// resolved findings from the ticket before building its PR body.
pub fn build_body(t: &Ticket, summary: &str, changes: &[String], testing: &[String]) -> String {
    let mut body = format!("## Summary\n\n{}\n\n## Changes\n\n", summary.trim());
    append_items(&mut body, changes.iter().map(String::as_str));
    body.push_str("\n## Testing\n\n");
    append_items(&mut body, testing.iter().map(String::as_str));
    let remaining: Vec<_> = t
        .findings
        .iter()
        .filter(|f| matches!(f.severity, Severity::Mid | Severity::Low))
        .map(|f| match &f.location {
            Some(location) => format!("{} ({location})", f.text),
            None => f.text.clone(),
        })
        .collect();
    if !remaining.is_empty() {
        body.push_str("\n## Not addressed\n\n");
        append_items(&mut body, remaining.iter().map(String::as_str));
    }
    if let Some(url) = &t.url {
        body.push_str("\n## Ticket\n\n");
        if t.id.is_empty() {
            body.push_str(url);
        } else {
            body.push_str(&format!("[{}]({url})", t.id));
        }
        body.push('\n');
    }
    body
}

fn append_items<'a>(body: &mut String, items: impl Iterator<Item = &'a str>) {
    for item in items {
        body.push_str("- ");
        body.push_str(item);
        body.push('\n');
    }
}

pub fn check_attribution(text: &str) -> Result<(), ShepherdError> {
    static ATTRIBUTION: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)(generated (with|by) (claude|codex|chatgpt|gpt|an? ai)|co-authored-by:.*(claude|codex|openai|anthropic|gpt)|\bas an ai\b|🤖|claude\.com/claude-code)")
            .expect("built-in attribution regex is valid")
    });
    if ATTRIBUTION.is_match(text) {
        Err(ShepherdError::Attribution)
    } else {
        Ok(())
    }
}
