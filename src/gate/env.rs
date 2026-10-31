use std::{collections::HashMap, path::Path};

pub fn agent_env(base: &HashMap<String, String>, state_dir: &Path) -> HashMap<String, String> {
    let mut result = base.clone();
    for key in [
        "GH_TOKEN",
        "GITHUB_TOKEN",
        "GH_ENTERPRISE_TOKEN",
        "GITHUB_ENTERPRISE_TOKEN",
        "SSH_AUTH_SOCK",
    ] {
        result.remove(key);
    }
    result.insert(
        "GH_CONFIG_DIR".into(),
        state_dir.join("agent-gh").to_string_lossy().into_owned(),
    );
    result.insert("GIT_TERMINAL_PROMPT".into(), "0".into());
    result.insert(
        "GIT_SSH_COMMAND".into(),
        "ssh -o BatchMode=yes -o IdentitiesOnly=yes -i /dev/null".into(),
    );
    result.insert(
        "PATH".into(),
        format!(
            "{}:{}",
            state_dir.join("shim").display(),
            base.get("PATH").map(String::as_str).unwrap_or("")
        ),
    );
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_strips_credentials() {
        let mut base = HashMap::new();
        for key in [
            "GH_TOKEN",
            "GITHUB_TOKEN",
            "GH_ENTERPRISE_TOKEN",
            "GITHUB_ENTERPRISE_TOKEN",
            "SSH_AUTH_SOCK",
        ] {
            base.insert(key.into(), "fixture".into());
        }
        base.insert("PATH".into(), "/usr/bin:/bin".into());
        let env = agent_env(&base, Path::new("/state"));
        for key in base.keys().filter(|key| key.as_str() != "PATH") {
            assert!(!env.contains_key(key));
        }
        assert_eq!(env["GIT_TERMINAL_PROMPT"], "0");
        assert_eq!(env["GH_CONFIG_DIR"], "/state/agent-gh");
        assert_eq!(
            env["GIT_SSH_COMMAND"],
            "ssh -o BatchMode=yes -o IdentitiesOnly=yes -i /dev/null"
        );
        assert!(env["PATH"].starts_with("/state/shim:"));
    }

    #[test]
    fn env_keeps_other_vars() {
        let base = HashMap::from([
            ("HOME".into(), "/home/example".into()),
            ("LANG".into(), "C.UTF-8".into()),
        ]);
        let env = agent_env(&base, Path::new("/state"));
        assert_eq!(env["HOME"], base["HOME"]);
        assert_eq!(env["LANG"], base["LANG"]);
    }
}
