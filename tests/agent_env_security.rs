use herdr_shepherd::{
    gate::env::{agent_env, prepare_agent_gitconfig},
    herdr::cli::env_script,
};
use std::{
    collections::HashMap,
    fs,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Command, Output},
};

struct Repo {
    dir: tempfile::TempDir,
    root: std::path::PathBuf,
    remote: std::path::PathBuf,
    base: HashMap<String, String>,
}
impl Repo {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        let remote = dir.path().join("origin.git");
        fs::create_dir(&root).unwrap();
        let home = dir.path().join("home");
        fs::create_dir(&home).unwrap();
        let config = home.join(".gitconfig");
        fs::write(&config, "[user]\n name = Fixture\n email = fixture@example.invalid\n[commit]\n gpgsign = false\n[credential \"https://github.com\"]\n helper = !/usr/bin/gh auth git-credential\n[credential]\n helper = fixture-helper\n[core]\n sshCommand = ssh -i /fixture/key\n[url \"https://redirect.invalid/\"]\n insteadOf = https://github.com/\n[include]\n path = /not-read/fixture-include\n[includeIf \"gitdir:*\"]\n path = /not-read/fixture-conditional\n").unwrap();
        let base = HashMap::from([
            ("HOME".into(), home.to_string_lossy().into_owned()),
            ("PATH".into(), "/usr/bin:/bin".into()),
            (
                "GIT_CONFIG_GLOBAL".into(),
                config.to_string_lossy().into_owned(),
            ),
        ]);
        let repo = Self {
            dir,
            root,
            remote,
            base,
        };
        repo.git(&["init", "-b", "feat/fixture"]);
        repo.git(&["init", "--bare", repo.remote.to_str().unwrap()]);
        repo.git(&["remote", "add", "origin", repo.remote.to_str().unwrap()]);
        repo
    }
    fn git(&self, args: &[&str]) -> Output {
        let out = Command::new("git")
            .env_clear()
            .envs(&self.base)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .current_dir(&self.root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        out
    }
    fn state(&self) -> std::path::PathBuf {
        self.dir.path().join("state")
    }
    fn clean_env(&self) -> HashMap<String, String> {
        prepare_agent_gitconfig(&self.base, &self.state()).unwrap();
        agent_env(&self.base, &self.state())
    }
    fn run(&self, program: &Path, args: &[&str], env: &HashMap<String, String>) -> Output {
        Command::new(program)
            .env_clear()
            .envs(env)
            .current_dir(&self.root)
            .args(args)
            .output()
            .unwrap()
    }
}

#[test]
fn agent_git_config_removes_credential_and_redirects() {
    let repo = Repo::new();
    let env = repo.clean_env();
    let out = repo.run(
        Path::new("git"),
        &["config", "--get-regexp", "credential"],
        &env,
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    for key in [
        "core.sshCommand",
        "url.https://redirect.invalid/.insteadOf",
        "include.path",
        "includeIf.gitdir:*.path",
    ] {
        let out = repo.run(Path::new("git"), &["config", "--get", key], &env);
        assert_eq!(out.status.code(), Some(1), "{key}");
    }
    let name = repo.run(Path::new("git"), &["config", "user.name"], &env);
    assert_eq!(String::from_utf8(name.stdout).unwrap().trim(), "Fixture");
    let signing = repo.run(Path::new("git"), &["config", "commit.gpgsign"], &env);
    assert_eq!(String::from_utf8(signing.stdout).unwrap().trim(), "false");
}

#[test]
fn agent_gh_token_does_not_fall_back_to_keyring() {
    let repo = Repo::new();
    let fake = repo.dir.path().join("gh");
    fs::write(&fake,"#!/bin/sh\nif test -z \"${GH_TOKEN:-${GITHUB_TOKEN:-}}\"; then printf '%s\\n' valid-fixture-keyring-token; exit 0; fi\nif test \"${GH_TOKEN:-}\" = shepherd-agent-no-token; then printf '%s\\n' 'invalid fixture token' >&2; exit 1; fi\nprintf '%s\\n' \"$GH_TOKEN\"\n").unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
    let unguarded = repo.run(
        Path::new("/bin/sh"),
        &[fake.to_str().unwrap(), "auth", "token"],
        &repo.base,
    );
    assert_eq!(
        String::from_utf8(unguarded.stdout).unwrap().trim(),
        "valid-fixture-keyring-token"
    );
    let env = repo.clean_env();
    let protected = repo.run(
        Path::new("/bin/sh"),
        &[fake.to_str().unwrap(), "auth", "token"],
        &env,
    );
    assert!(!protected.status.success());
    assert!(protected.stdout.is_empty());
    assert_eq!(env["GH_TOKEN"], "shepherd-agent-no-token");
    assert_eq!(env["GITHUB_TOKEN"], "shepherd-agent-no-token");
    assert_eq!(env["GIT_ASKPASS"], "/bin/false");
    assert_eq!(env["SSH_ASKPASS"], "/bin/false");
}

#[test]
fn pane_environment_removes_inherited_git_config_injection() {
    let repo = Repo::new();
    let env = repo.clean_env();
    let script = format!(
        "{} git config --get-regexp credential",
        env_script(&env).unwrap()
    );
    let out = Command::new("sh")
        .env_clear()
        .envs(&repo.base)
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "credential.helper")
        .env("GIT_CONFIG_VALUE_0", "!/usr/bin/gh auth git-credential")
        .env(
            "GIT_CONFIG_PARAMETERS",
            "'credential.helper=unsafe-fixture'",
        )
        .current_dir(&repo.root)
        .args(["-c", &script])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    let script = format!("{} env", env_script(&env).unwrap());
    let out = Command::new("sh")
        .env_clear()
        .envs(&repo.base)
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "credential.helper")
        .env("GIT_CONFIG_VALUE_0", "fixture")
        .args(["-c", &script])
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(!text.contains("GIT_CONFIG_KEY_0="));
    assert!(!text.contains("GIT_CONFIG_VALUE_0="));
    assert!(!text.contains("GIT_CONFIG_COUNT="));
}

#[test]
fn missing_global_gitconfig_uses_safe_empty_defaults() {
    let repo = Repo::new();
    let mut base = repo.base.clone();
    base.insert(
        "GIT_CONFIG_GLOBAL".into(),
        repo.dir
            .path()
            .join("missing-gitconfig")
            .to_string_lossy()
            .into_owned(),
    );
    prepare_agent_gitconfig(&base, &repo.state()).unwrap();
    let env = agent_env(&base, &repo.state());
    let out = repo.run(
        Path::new("git"),
        &["config", "--get-regexp", "credential"],
        &env,
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
}

#[test]
fn sanitized_config_preserves_implicit_boolean() {
    let repo = Repo::new();
    let config = repo.base.get("GIT_CONFIG_GLOBAL").unwrap();
    let mut text = fs::read_to_string(config).unwrap();
    text.push_str("[rerere]\n enabled\n");
    fs::write(config, text).unwrap();
    let env = repo.clean_env();
    let out = repo.run(
        Path::new("git"),
        &["config", "--type=bool", "rerere.enabled"],
        &env,
    );
    assert!(out.status.success());
    assert_eq!(String::from_utf8(out.stdout).unwrap().trim(), "true");
}

#[test]
fn sanitized_config_does_not_follow_include_files() {
    let repo = Repo::new();
    let include = repo.dir.path().join("include-target");
    fs::write(&include, "[broken fixture syntax\n").unwrap();
    let config = repo.base.get("GIT_CONFIG_GLOBAL").unwrap();
    let mut text = fs::read_to_string(config).unwrap();
    text.push_str(&format!("[include]\n path = {}\n", include.display()));
    fs::write(config, text).unwrap();
    let env = repo.clean_env();
    let out = repo.run(Path::new("git"), &["config", "user.name"], &env);
    assert!(out.status.success());
    assert_eq!(String::from_utf8(out.stdout).unwrap().trim(), "Fixture");
}

#[test]
fn agent_git_config_removes_http_auth_sources() {
    let repo = Repo::new();
    let config = repo.base.get("GIT_CONFIG_GLOBAL").unwrap();
    let mut text = fs::read_to_string(config).unwrap();
    text.push_str(
        r#"
[http]
 extraHeader = Authorization: Bearer fixture-http-token
 cookieFile = /not-read/fixture-cookie
 saveCookies = true
 sslCert = /not-read/fixture-cert
 sslKey = /not-read/fixture-private
 proxy = https://fixture:fixture-password@proxy.invalid
 version = HTTP/1.1
[http "https://github.com/"]
 extraHeader = Authorization: Bearer fixture-scoped-token
 cookieFile = /not-read/fixture-scoped-cookie
 saveCookies = true
 sslCert = /not-read/fixture-scoped-cert
 sslKey = /not-read/fixture-scoped-private
 proxy = https://fixture:fixture-password@scoped-proxy.invalid
[remote "origin"]
 proxy = https://fixture:fixture-password@remote-proxy.invalid
"#,
    );
    fs::write(config, text).unwrap();
    let env = repo.clean_env();
    for scope in ["http", "http.https://github.com/"] {
        for field in [
            "extraHeader",
            "cookieFile",
            "saveCookies",
            "sslCert",
            "sslKey",
            "proxy",
        ] {
            let key = format!("{scope}.{field}");
            let out = repo.run(Path::new("git"), &["config", "--get", &key], &env);
            assert_eq!(out.status.code(), Some(1), "retained {key}");
            assert!(out.stdout.is_empty(), "retained {key}");
        }
    }
    let proxy = repo.run(
        Path::new("git"),
        &["config", "--get", "remote.origin.proxy"],
        &env,
    );
    assert_eq!(proxy.status.code(), Some(1));
    assert!(proxy.stdout.is_empty());
    let version = repo.run(Path::new("git"), &["config", "http.version"], &env);
    assert_eq!(
        String::from_utf8(version.stdout).unwrap().trim(),
        "HTTP/1.1"
    );
    let name = repo.run(Path::new("git"), &["config", "user.name"], &env);
    assert_eq!(String::from_utf8(name.stdout).unwrap().trim(), "Fixture");
    let signing = repo.run(Path::new("git"), &["config", "commit.gpgsign"], &env);
    assert_eq!(String::from_utf8(signing.stdout).unwrap().trim(), "false");
}
