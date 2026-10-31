use crate::error::ShepherdError;
use globset::{Glob, GlobSet, GlobSetBuilder};

pub struct BranchPolicy {
    patterns: GlobSet,
}

impl BranchPolicy {
    pub fn new(
        extra: &[String],
        default_branch: Option<&str>,
        base: Option<&str>,
    ) -> Result<Self, ShepherdError> {
        let mut patterns = GlobSetBuilder::new();
        for pattern in [
            "main",
            "master",
            "development",
            "develop",
            "production",
            "release-*",
            "sync/*",
        ]
        .into_iter()
        .chain(extra.iter().map(String::as_str))
        {
            patterns.add(Glob::new(pattern).map_err(|e| {
                ShepherdError::Usage(format!("invalid protected branch pattern {pattern}: {e}"))
            })?);
        }
        for branch in default_branch.into_iter().chain(base) {
            patterns.add(Glob::new(&globset::escape(branch)).map_err(anyhow::Error::from)?);
        }
        Ok(Self {
            patterns: patterns.build().map_err(anyhow::Error::from)?,
        })
    }

    pub fn is_protected(&self, branch: &str) -> bool {
        self.patterns
            .is_match(branch.strip_prefix("refs/heads/").unwrap_or(branch))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floor_branches_protected() {
        let policy = BranchPolicy::new(&[], None, None).unwrap();
        for branch in [
            "main",
            "master",
            "development",
            "develop",
            "production",
            "release-1.2",
            "sync/x",
        ] {
            assert!(policy.is_protected(branch), "{branch}");
        }
        assert!(!policy.is_protected("feat/x"));
    }

    #[test]
    fn default_and_base_protected() {
        let policy =
            BranchPolicy::new(&["custom/*".into()], Some("trunk"), Some("staging")).unwrap();
        for branch in ["main", "trunk", "staging", "custom/x"] {
            assert!(policy.is_protected(branch));
        }
    }
}
