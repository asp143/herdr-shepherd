use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct KeysSnippet {
    pub toml: String,
    pub skipped: Vec<String>,
}

pub fn keys_snippet(user_config: &str, enabled: bool) -> KeysSnippet {
    let mut result = KeysSnippet {
        toml: String::new(),
        skipped: Vec::new(),
    };
    if !enabled {
        return result;
    }
    let defaults = [
        ("prefix+alt+g", "herdr-shepherd.regroup"),
        ("prefix+alt+p", "herdr-shepherd.prune"),
    ];
    // The CLI reports parse errors. Direct callers also fail closed rather
    // than suggesting bindings that may shadow an unreadable user config.
    let Ok(config) = user_config.parse::<toml::Table>() else {
        result.skipped = defaults.iter().map(|(key, _)| (*key).into()).collect();
        return result;
    };
    let keys = config.get("keys").and_then(toml::Value::as_table);
    let commands = keys
        .and_then(|keys| keys.get("command"))
        .and_then(toml::Value::as_array);
    for (key, action) in defaults {
        let mut occupied =
            keys.is_some_and(|table| table.values().any(|value| value.as_str() == Some(key)));
        let mut replaces_legacy = false;
        for command in commands.into_iter().flatten() {
            if command.get("key").and_then(toml::Value::as_str) != Some(key) {
                continue;
            }
            let legacy = command
                .get("command")
                .and_then(toml::Value::as_str)
                .is_some_and(runs_legacy_regroup);
            // A second custom binding wins even if a legacy one also exists.
            occupied |= !legacy;
            replaces_legacy |= legacy;
        }
        if occupied {
            result.skipped.push(key.into());
            continue;
        }
        if replaces_legacy {
            result.toml.push_str("# replaces herdr-regroup binding\n");
        }
        result.toml.push_str(&format!(
            "[[keys.command]]\nkey = \"{key}\"\ntype = \"plugin_action\"\ncommand = \"{action}\"\n\n"
        ));
    }
    result
}

fn runs_legacy_regroup(command: &str) -> bool {
    // Match an executable, not a mention in another command's arguments.
    command.split_whitespace().next().is_some_and(|program| {
        std::path::Path::new(program.trim_matches(['\'', '"']))
            .file_name()
            .is_some_and(|name| name == "herdr-regroup")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emits_both_defaults_on_empty_config() {
        let snippet = keys_snippet("", true);
        assert!(snippet.skipped.is_empty());
        assert!(snippet.toml.contains("prefix+alt+g"));
        assert!(snippet.toml.contains("prefix+alt+p"));
        assert!(snippet.toml.contains("herdr-shepherd.regroup"));
        assert!(snippet.toml.contains("herdr-shepherd.prune"));
        assert_eq!(snippet.toml.matches("type = \"plugin_action\"").count(), 2);
    }

    #[test]
    fn skips_user_bound_key() {
        let snippet = keys_snippet(
            "[[keys.command]]\nkey = 'prefix+alt+p'\ntype = 'popup'\ncommand = 'lazygit'\n",
            true,
        );
        assert_eq!(snippet.skipped, ["prefix+alt+p"]);
        assert!(!snippet.toml.contains("herdr-shepherd.prune"));
        assert!(snippet.toml.contains("herdr-shepherd.regroup"));
    }

    #[test]
    fn replaces_herdr_regroup_binding() {
        let snippet = keys_snippet(
            "[[keys.command]]\nkey = 'prefix+alt+g'\ntype = 'shell'\ncommand = 'herdr-regroup'\n",
            true,
        );
        assert!(snippet.skipped.is_empty());
        assert!(snippet.toml.contains("prefix+alt+g"));
        assert!(snippet.toml.contains("# replaces herdr-regroup binding"));
    }

    #[test]
    fn disabled_emits_nothing() {
        let snippet = keys_snippet("", false);
        assert!(snippet.toml.is_empty());
        assert!(snippet.skipped.is_empty());
    }

    #[test]
    fn skips_keys_table_values() {
        let snippet = keys_snippet("[keys]\nnew_workspace = 'prefix+alt+g'\n", true);
        assert_eq!(snippet.skipped, ["prefix+alt+g"]);
        assert!(!snippet.toml.contains("herdr-shepherd.regroup"));
    }

    #[test]
    fn replaces_absolute_script_but_preserves_custom_duplicate() {
        let legacy = "[[keys.command]]\nkey = 'prefix+alt+g'\ncommand = '/home/example/.local/bin/herdr-regroup'\n";
        let replacement = keys_snippet(legacy, true);
        assert!(replacement.skipped.is_empty());
        assert!(
            replacement
                .toml
                .contains("# replaces herdr-regroup binding")
        );
        let duplicate =
            format!("{legacy}\n[[keys.command]]\nkey = 'prefix+alt+g'\ncommand = 'lazygit'\n");
        let snippet = keys_snippet(&duplicate, true);
        assert_eq!(snippet.skipped, ["prefix+alt+g"]);
        assert!(!snippet.toml.contains("herdr-shepherd.regroup"));
    }

    #[test]
    fn invalid_toml_emits_no_conflicting_keys() {
        let snippet = keys_snippet("[keys", true);
        assert!(snippet.toml.is_empty());
        assert_eq!(snippet.skipped, ["prefix+alt+g", "prefix+alt+p"]);
    }
}
