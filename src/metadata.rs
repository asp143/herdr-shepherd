use crate::{
    error::ShepherdError,
    github::{Checks, PrState, PrStatus, ReviewDecision},
};

/// Build a metadata token name accepted by herdr.
pub fn token(prefix: &str, key: &str) -> Result<String, ShepherdError> {
    let name = format!("{prefix}{key}");
    if name.is_empty()
        || name.len() > 32
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(ShepherdError::Usage(
            "metadata token must match [A-Za-z0-9_-]{1,32}".into(),
        ));
    }
    Ok(name)
}

pub fn pr_token_value(status: &PrStatus) -> &'static str {
    match status.state {
        PrState::Merged => "merged",
        PrState::Closed => "closed",
        PrState::Open => match (&status.checks, &status.review) {
            (Checks::Failing, _) => "ci-failing",
            (_, ReviewDecision::ChangesRequested) => "changes-requested",
            (Checks::Pending, _) => "ci-pending",
            _ => "open",
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::{Checks, PrState, PrStatus, ReviewDecision};

    #[test]
    fn token_prefixing() {
        assert_eq!(token("shepherd_", "pr").unwrap(), "shepherd_pr");
    }

    #[test]
    fn token_too_long_rejected() {
        assert!(matches!(
            token("", &"a".repeat(33)),
            Err(ShepherdError::Usage(_))
        ));
        for key in ["", "bad.key", "with space", "a=b", "é"] {
            assert!(matches!(token("", key), Err(ShepherdError::Usage(_))));
        }
    }

    #[test]
    fn pr_token_values() {
        for (state, checks, review, expected) in [
            (
                PrState::Merged,
                Checks::Failing,
                ReviewDecision::ChangesRequested,
                "merged",
            ),
            (
                PrState::Closed,
                Checks::Failing,
                ReviewDecision::ChangesRequested,
                "closed",
            ),
            (
                PrState::Open,
                Checks::Failing,
                ReviewDecision::ChangesRequested,
                "ci-failing",
            ),
            (
                PrState::Open,
                Checks::Pending,
                ReviewDecision::ChangesRequested,
                "changes-requested",
            ),
            (
                PrState::Open,
                Checks::Pending,
                ReviewDecision::Approved,
                "ci-pending",
            ),
            (
                PrState::Open,
                Checks::Passing,
                ReviewDecision::Approved,
                "open",
            ),
            (PrState::Open, Checks::None, ReviewDecision::None, "open"),
        ] {
            let status = PrStatus {
                number: 1,
                url: "https://github.com/example/repo/pull/1".into(),
                state,
                merged_at: None,
                base: "main".into(),
                head: "feat/task".into(),
                checks,
                review,
                comment_ids: vec![],
                head_oid: None,
            };
            assert_eq!(pr_token_value(&status), expected);
        }
    }
}
