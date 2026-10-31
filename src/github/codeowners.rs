use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq, Eq)]
pub struct Rule {
    pub pattern: String,
    pub owners: Vec<String>,
}

pub fn find_codeowners(repo_root: &Path) -> Option<PathBuf> {
    [".github/CODEOWNERS", "CODEOWNERS", "docs/CODEOWNERS"]
        .into_iter()
        .map(|name| repo_root.join(name))
        .find(|path| path.is_file())
}

pub fn parse(text: &str) -> Vec<Rule> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split('#').next()?.split_whitespace();
            let pattern = fields.next()?;
            // CODEOWNERS has no negation rules.
            if pattern.starts_with('!') {
                return None;
            }
            let owners = fields
                .filter(|owner| {
                    owner.starts_with('@') && owner.len() > 1 && !owner[1..].contains('@')
                })
                .map(str::to_owned)
                .collect();
            Some(Rule {
                pattern: pattern.into(),
                owners,
            })
        })
        .collect()
}

pub fn owners_for(rules: &[Rule], changed: &[String], author: Option<&str>) -> Vec<String> {
    let compiled: Vec<_> = rules
        .iter()
        .filter_map(|rule| {
            let anchored = rule.pattern.starts_with('/');
            let directory = rule.pattern.ends_with('/');
            let pattern = rule.pattern.trim_matches('/');
            if pattern.is_empty() {
                return None;
            }
            let any_depth = !anchored && !pattern.contains('/');
            let pattern = format!(
                "{}{}{}",
                if any_depth { "**/" } else { "" },
                pattern,
                if directory { "/**" } else { "" }
            );
            let matcher = globset::GlobBuilder::new(&pattern)
                .literal_separator(true)
                .build()
                .ok()?
                .compile_matcher();
            Some((rule, matcher))
        })
        .collect();
    let mut owners = std::collections::BTreeSet::new();
    for path in changed {
        let path = path.trim_start_matches('/');
        let matching = compiled.iter().rev().find(|(_, matcher)| {
            if matcher.is_match(path) {
                return true;
            }
            // A pattern matching a directory also owns its descendants.
            path.match_indices('/')
                .any(|(index, _)| matcher.is_match(&path[..index]))
        });
        if let Some((rule, _)) = matching {
            for owner in &rule.owners {
                let owner = owner.trim_start_matches('@');
                if !owner.is_empty()
                    && !author.is_some_and(|author| {
                        owner.eq_ignore_ascii_case(author.trim_start_matches('@'))
                    })
                {
                    owners.insert(owner.to_owned());
                }
            }
        }
    }
    owners.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owners(text: &str, changed: &[&str], author: Option<&str>) -> Vec<String> {
        owners_for(
            &parse(text),
            &changed
                .iter()
                .map(|path| (*path).into())
                .collect::<Vec<_>>(),
            author,
        )
    }

    #[test]
    fn last_match_wins() {
        assert_eq!(owners("* @a\n/docs/ @b\n", &["docs/x.md"], None), ["b"]);
    }

    #[test]
    fn anchored_vs_any_depth() {
        assert!(owners("/build.rs @a\n", &["src/build.rs"], None).is_empty());
        assert_eq!(
            owners("build.rs @b\n", &["src/build.rs", "build.rs"], None),
            ["b"]
        );
        assert_eq!(owners("/build.rs @a\n", &["build.rs"], None), ["a"]);
    }

    #[test]
    fn double_star() {
        assert_eq!(
            owners(
                "src/**/test.rs @a\n",
                &["src/test.rs", "src/a/b/test.rs"],
                None
            ),
            ["a"]
        );
        assert!(owners("src/*.rs @b\n", &["src/a/test.rs"], None).is_empty());
    }

    #[test]
    fn drops_author_and_emails() {
        let text = "# comment\n\n*.rs @me example@example.com @other # @comment\n";
        assert_eq!(owners(text, &["src/lib.rs"], Some("me")), ["other"]);
        assert_eq!(parse(text)[0].owners, ["@me", "@other"]);
    }

    #[test]
    fn teams_kept() {
        assert_eq!(owners("* @org/team\n", &["src/lib.rs"], None), ["org/team"]);
    }

    #[test]
    fn no_rules_no_owners() {
        assert!(owners("# nothing here\n", &["src/lib.rs"], None).is_empty());
    }

    #[test]
    fn directories_match_descendants_at_any_depth() {
        assert_eq!(owners("docs @a\n", &["project/docs/a/b.md"], None), ["a"]);
        assert_eq!(owners("/docs/ @a\n", &["docs/sub/x.md"], None), ["a"]);
        assert!(owners("/docs/ @a\n", &["project/docs/x.md"], None).is_empty());
        assert_eq!(owners("docs/* @a\n", &["docs/sub/x.md"], None), ["a"]);
    }

    #[test]
    fn empty_rule_clears_owners() {
        assert!(owners("* @a\n/docs/\n", &["docs/x.md"], None).is_empty());
    }

    #[test]
    fn codeowners_search_order() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".github")).unwrap();
        std::fs::create_dir_all(dir.path().join("docs")).unwrap();
        for name in ["docs/CODEOWNERS", "CODEOWNERS", ".github/CODEOWNERS"] {
            std::fs::write(dir.path().join(name), "* @owner\n").unwrap();
            assert_eq!(find_codeowners(dir.path()), Some(dir.path().join(name)));
        }
    }
}
