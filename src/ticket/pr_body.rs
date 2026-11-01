use std::{fmt, marker::PhantomData, sync::LazyLock};

use regex::Regex;
use serde::{
    Deserialize, Deserializer,
    de::{MapAccess, Visitor, value::MapAccessDeserializer},
};

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

/// Optional publication metadata. Stored findings and reports remain unchanged.
#[derive(Debug)]
pub struct FindingDispositions {
    ticket_id: String,
    findings: Vec<FindingDisposition>,
}

#[derive(Debug)]
struct FindingDisposition {
    index: usize,
    status: FindingStatus,
    reason: Option<String>,
}

impl<'de> Deserialize<'de> for FindingDispositions {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Fields {
            ticket_id: String,
            findings: Vec<FindingDisposition>,
        }
        let fields: Fields = deserialize_object(deserializer)?;
        Ok(Self {
            ticket_id: fields.ticket_id,
            findings: fields.findings,
        })
    }
}

impl<'de> Deserialize<'de> for FindingDisposition {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Fields {
            index: usize,
            status: FindingStatus,
            reason: Option<String>,
        }
        let fields: Fields = deserialize_object(deserializer)?;
        Ok(Self {
            index: fields.index,
            status: fields.status,
            reason: fields.reason,
        })
    }
}

// Derived structs also accept positional sequences. Require a map before
// delegating to their field validation, preserving duplicate-key rejection.
fn deserialize_object<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> Result<T, D::Error> {
    struct ObjectVisitor<T>(PhantomData<T>);
    impl<'de, T: Deserialize<'de>> Visitor<'de> for ObjectVisitor<T> {
        type Value = T;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("an object")
        }

        fn visit_map<M: MapAccess<'de>>(self, map: M) -> Result<T, M::Error> {
            T::deserialize(MapAccessDeserializer::new(map))
        }
    }
    deserializer.deserialize_map(ObjectVisitor(PhantomData))
}

#[derive(Clone, Copy, Debug)]
enum FindingStatus {
    Open,
    Deferred,
    Resolved,
}

impl<'de> Deserialize<'de> for FindingStatus {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match String::deserialize(deserializer)?.as_str() {
            "open" => Ok(Self::Open),
            "deferred" => Ok(Self::Deferred),
            "resolved" => Ok(Self::Resolved),
            other => Err(serde::de::Error::unknown_variant(
                other,
                &["open", "deferred", "resolved"],
            )),
        }
    }
}

impl FindingDispositions {
    pub fn parse(text: &str) -> Result<Self, ShepherdError> {
        serde_json::from_str(text)
            .map_err(|e| ShepherdError::Usage(format!("invalid finding dispositions: {e}")))
    }

    fn for_ticket<'a>(
        &'a self,
        ticket: &Ticket,
    ) -> Result<Vec<Option<&'a FindingDisposition>>, ShepherdError> {
        if self.ticket_id != ticket.id {
            return Err(ShepherdError::Usage(
                "finding dispositions ticket_id does not match ticket".into(),
            ));
        }
        let mut entries = vec![None; ticket.findings.len()];
        for entry in &self.findings {
            let slot = entries.get_mut(entry.index).ok_or_else(|| {
                ShepherdError::Usage(format!("finding index out of range: {}", entry.index))
            })?;
            if slot.replace(entry).is_some() {
                return Err(ShepherdError::Usage(format!(
                    "duplicate finding index: {}",
                    entry.index
                )));
            }
        }
        Ok(entries)
    }
}

/// Stored findings are history unless explicitly classified for publication.
pub fn build_body(t: &Ticket, summary: &str, changes: &[String], testing: &[String]) -> String {
    render_body(t, summary, changes, testing, &[])
}

/// Validate metadata against the current ticket snapshot before rendering.
pub fn build_body_with_dispositions(
    t: &Ticket,
    summary: &str,
    changes: &[String],
    testing: &[String],
    dispositions: Option<&FindingDispositions>,
) -> Result<String, ShepherdError> {
    let entries = dispositions
        .map(|d| d.for_ticket(t))
        .transpose()?
        .unwrap_or_default();
    Ok(render_body(t, summary, changes, testing, &entries))
}

fn render_body(
    t: &Ticket,
    summary: &str,
    changes: &[String],
    testing: &[String],
    entries: &[Option<&FindingDisposition>],
) -> String {
    let mut body = format!("## Summary\n\n{}\n\n## Changes\n\n", summary.trim());
    append_items(&mut body, changes.iter().map(String::as_str));
    body.push_str("\n## Testing\n\n");
    append_items(&mut body, testing.iter().map(String::as_str));
    let mut current = String::new();
    let mut historical = String::new();
    for (index, finding) in t.findings.iter().enumerate() {
        let entry = entries.get(index).copied().flatten();
        let (section, status) = match entry.map(|e| e.status) {
            Some(FindingStatus::Open) => (&mut current, "open"),
            Some(FindingStatus::Deferred) => (&mut current, "deferred"),
            Some(FindingStatus::Resolved) => (&mut historical, "resolved"),
            None => (&mut historical, "current disposition unclassified"),
        };
        let severity = match finding.severity {
            Severity::High => "High (Blocker)",
            Severity::Mid => "Mid",
            Severity::Low => "Low",
            Severity::Wish => "Wish",
        };
        section.push_str(&format!(
            "- #{index} [{severity}; {status}] {}",
            finding.text
        ));
        if let Some(location) = &finding.location {
            section.push_str(&format!(" ({location})"));
        }
        section.push('\n');
        if let Some(reason) = entry.and_then(|e| e.reason.as_deref()) {
            section.push_str(&format!("  Reason: {reason}\n"));
        }
    }
    if !current.is_empty() {
        body.push_str("\n## Open and deferred findings\n\n");
        body.push_str(&current);
    }
    if !historical.is_empty() {
        body.push_str("\n## Finding history\n\n");
        body.push_str(&historical);
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
        Regex::new(r"(?i)(generated\s+(with|by)\s+(claude|codex|chatgpt|gpt|an?\s+ai)|co-authored-by:.*(claude|codex|openai|anthropic|gpt)|\bas\s+an\s+ai\b|🤖|claude\.com/claude-code)")
            .expect("built-in attribution regex is valid")
    });
    if ATTRIBUTION.is_match(text) {
        Err(ShepherdError::Attribution)
    } else {
        Ok(())
    }
}
