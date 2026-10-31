use crate::{error::ShepherdError, proc};
use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub const NO_TOKEN: &str = "shepherd-agent-no-token";

/// Prepare only a trusted provider subprocess. Raw shim forwarding passes
/// false so its credentials remain stripped; checked gate and transport calls
/// may restore the user's default gh/keyring lookup without reading credentials.
pub(crate) fn prepare_provider_command(
    command: &mut Command,
    state_dir: &Path,
    restore_auth: bool,
) -> Result<(), ShepherdError> {
    // A nested shepherd command must invoke the provider, not re-enter
    // the credential-stripped shim installed on its caller's PATH.
    let shim = state_dir.join("shim");
    let shim = shim.canonicalize().unwrap_or(shim);
    let inherited_path = std::env::var_os("PATH").unwrap_or_default();
    let kept: Vec<_> = std::env::split_paths(&inherited_path)
        .filter(|entry| entry.canonicalize().unwrap_or_else(|_| entry.clone()) != shim)
        .collect();
    command.env(
        "PATH",
        std::env::join_paths(kept).map_err(anyhow::Error::from)?,
    );
    let tokens = [
        "GH_TOKEN",
        "GITHUB_TOKEN",
        "GH_ENTERPRISE_TOKEN",
        "GITHUB_ENTERPRISE_TOKEN",
    ];
    let owned_directory = state_dir.join("agent-gh");
    let caller_directory = std::env::var_os("GH_CONFIG_DIR").map(PathBuf::from);
    let own_sentinels = caller_directory.as_ref().is_some_and(|path| {
        path == &owned_directory
            || path
                .canonicalize()
                .ok()
                .zip(owned_directory.canonicalize().ok())
                .is_some_and(|(left, right)| left == right)
    }) && tokens
        .iter()
        .any(|key| std::env::var(key).as_deref() == Ok(NO_TOKEN));
    if restore_auth && own_sentinels {
        // Only a checked shepherd operation may return to the user's
        // default gh/keyring lookup. Never copy or persist credentials.
        command.env_remove("GH_CONFIG_DIR");
        for token in tokens {
            if std::env::var(token).as_deref() == Ok(NO_TOKEN) {
                command.env_remove(token);
            }
        }
    }
    Ok(())
}

/// Copy safe global Git settings without following includes or preserving
/// credential discovery, HTTP authentication, SSH commands, or URL rewrites.
pub fn prepare_agent_gitconfig(
    base: &HashMap<String, String>,
    state_dir: &Path,
) -> Result<PathBuf, ShepherdError> {
    for key in ["HOME", "XDG_CONFIG_HOME", "GIT_CONFIG_GLOBAL"] {
        if let Some(value) = base.get(key) {
            for part in Path::new(value).components() {
                let name = part.as_os_str().to_string_lossy();
                if name.starts_with(".env")
                    || name == "Credentials"
                    || name.ends_with(".pem")
                    || name.ends_with(".key")
                    || matches!(name.as_ref(), "id_rsa" | "id_ed25519" | "credentials")
                {
                    return Err(ShepherdError::Usage(
                        "secret configuration paths cannot be read".into(),
                    ));
                }
            }
        }
    }
    let candidates = if let Some(path) = base.get("GIT_CONFIG_GLOBAL") {
        vec![PathBuf::from(path)]
    } else {
        let mut paths = Vec::new();
        if let Some(home) = base.get("HOME") {
            let xdg = base
                .get("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| Path::new(home).join(".config"));
            paths.push(xdg.join("git/config"));
            paths.push(Path::new(home).join(".gitconfig"));
        }
        paths
    };
    fs::create_dir_all(state_dir).map_err(anyhow::Error::from)?;
    let state_root = state_dir.canonicalize().map_err(anyhow::Error::from)?;
    let mut global = String::new();
    for candidate in candidates.into_iter().filter(|path| path.exists()) {
        let actual = candidate.canonicalize().map_err(anyhow::Error::from)?;
        for part in actual.components() {
            let name = part.as_os_str().to_string_lossy();
            if name.starts_with(".env")
                || name == "Credentials"
                || name.ends_with(".pem")
                || name.ends_with(".key")
                || matches!(name.as_ref(), "id_rsa" | "id_ed25519" | "credentials")
            {
                return Err(ShepherdError::Usage(
                    "secret configuration paths cannot be read".into(),
                ));
            }
        }
        // Git can read global config during startup before parsing config's
        // --no-includes flag. Suppress those implicit sources and name each
        // authorized global file explicitly, without following its includes.
        let output = proc::capture(
            Command::new("git")
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CEILING_DIRECTORIES", &state_root)
                .current_dir(&state_root)
                .args(["config", "--file"])
                .arg(&actual)
                .args(["--no-includes", "--null", "--list"]),
            Duration::from_secs(20),
            None,
        )?;
        if !output.status.success() {
            return Err(
                anyhow::anyhow!("cannot read global Git configuration without includes").into(),
            );
        }
        global.push_str(&output.stdout);
    }
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = state_root.join(format!(
        ".agent-gitconfig-{}-{nonce}.tmp",
        std::process::id()
    ));
    let target = state_root.join("agent-gitconfig");
    let result = (|| -> Result<(), ShepherdError> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(anyhow::Error::from)?;
        file.write_all(b"# Safe global Git defaults for shepherd agents.\n")
            .map_err(anyhow::Error::from)?;
        drop(file);
        for entry in global.split('\0').filter(|entry| !entry.is_empty()) {
            let (key, value) = entry.split_once('\n').unwrap_or((entry, "true"));
            let lower = key.to_ascii_lowercase();
            let field = lower.rsplit('.').next().unwrap_or_default();
            let http_auth = lower.starts_with("http.")
                && matches!(
                    field,
                    "extraheader" | "cookiefile" | "savecookies" | "sslcert" | "sslkey" | "proxy"
                );
            let remote_proxy = lower.starts_with("remote.") && field == "proxy";
            if http_auth
                || remote_proxy
                || lower.starts_with("credential.")
                || lower == "credential"
                || lower == "core.sshcommand"
                || lower.starts_with("url.")
                || lower.starts_with("include")
            {
                continue;
            }
            let write = proc::capture(
                Command::new("git")
                    .env_clear()
                    .env("PATH", "/usr/bin:/bin")
                    .env("GIT_CONFIG_NOSYSTEM", "1")
                    .env("GIT_CONFIG_GLOBAL", "/dev/null")
                    .env("GIT_CEILING_DIRECTORIES", &state_root)
                    .current_dir(&state_root)
                    .args(["config", "--file"])
                    .arg(&temporary)
                    .args(["--add", key, value]),
                Duration::from_secs(20),
                None,
            )?;
            if !write.status.success() {
                return Err(anyhow::anyhow!("cannot write sanitized Git configuration").into());
            }
        }
        OpenOptions::new()
            .read(true)
            .open(&temporary)
            .and_then(|file| file.sync_all())
            .map_err(anyhow::Error::from)?;
        fs::rename(&temporary, &target).map_err(anyhow::Error::from)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result?;
    Ok(target)
}

pub fn agent_env(base: &HashMap<String, String>, state_dir: &Path) -> HashMap<String, String> {
    let mut result = base.clone();
    for key in [
        "GH_TOKEN",
        "GITHUB_TOKEN",
        "GH_ENTERPRISE_TOKEN",
        "GITHUB_ENTERPRISE_TOKEN",
        "SSH_AUTH_SOCK",
        "GIT_SSH",
        "SSH_ASKPASS_REQUIRE",
    ] {
        result.remove(key);
    }
    for key in [
        "GH_TOKEN",
        "GITHUB_TOKEN",
        "GH_ENTERPRISE_TOKEN",
        "GITHUB_ENTERPRISE_TOKEN",
    ] {
        result.insert(key.into(), NO_TOKEN.into());
    }
    result.retain(|key, _| {
        !key.starts_with("GIT_CONFIG_KEY_") && !key.starts_with("GIT_CONFIG_VALUE_")
    });
    for key in [
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_COUNT",
        "GIT_CONFIG_SYSTEM",
    ] {
        result.remove(key);
    }
    result.insert(
        "GIT_CONFIG_GLOBAL".into(),
        state_dir
            .join("agent-gitconfig")
            .to_string_lossy()
            .into_owned(),
    );
    result.insert("GIT_CONFIG_NOSYSTEM".into(), "1".into());
    result.insert("GIT_ASKPASS".into(), "/bin/false".into());
    result.insert("SSH_ASKPASS".into(), "/bin/false".into());
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
        for key in [
            "GH_TOKEN",
            "GITHUB_TOKEN",
            "GH_ENTERPRISE_TOKEN",
            "GITHUB_ENTERPRISE_TOKEN",
        ] {
            assert_eq!(env[key], NO_TOKEN);
        }
        assert!(!env.contains_key("SSH_AUTH_SOCK"));
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
