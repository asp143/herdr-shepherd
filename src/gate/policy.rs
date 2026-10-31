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

/// Read additive branch protection without silently dropping malformed values.
pub fn extra_patterns(cfg: &crate::config::Config) -> Result<Vec<String>, ShepherdError> {
    let Some(shepherd) = cfg.raw.get("shepherd") else {
        return Ok(Vec::new());
    };
    let shepherd = shepherd
        .as_table()
        .ok_or_else(|| ShepherdError::Usage("shepherd must be a table".into()))?;
    match shepherd.get("protected_branches") {
        None => Ok(Vec::new()),
        Some(toml::Value::Array(values)) => values
            .iter()
            .map(|value| {
                value.as_str().map(str::to_owned).ok_or_else(|| {
                    ShepherdError::Usage("protected_branches must contain strings".into())
                })
            })
            .collect(),
        Some(_) => Err(ShepherdError::Usage(
            "protected_branches must be an array of globs".into(),
        )),
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
    #[test]
    fn extra_patterns_rejects_invalid_config_types() {
        let dir = tempfile::tempdir().unwrap();
        for value in ["'feat/t'", "['other/*', 1]"] {
            let mut cfg = crate::config::load(dir.path(), None).unwrap();
            cfg.raw = format!("[shepherd]\nprotected_branches = {value}\n")
                .parse()
                .unwrap();
            assert!(matches!(extra_patterns(&cfg), Err(ShepherdError::Usage(_))));
        }
    }

    #[test]
    fn extra_patterns_preserves_configured_globs() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = crate::config::load(dir.path(), None).unwrap();
        assert!(extra_patterns(&cfg).unwrap().is_empty());
        cfg.raw = "[shepherd]\nprotected_branches = ['custom/*', 'trunk']\n"
            .parse()
            .unwrap();
        assert_eq!(extra_patterns(&cfg).unwrap(), ["custom/*", "trunk"]);
    }
}
