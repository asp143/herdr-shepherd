use herdr_shepherd::store::{Store, Ticket, TicketState};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
};

struct Repo {
    dir: tempfile::TempDir,
    root: PathBuf,
    ticket: PathBuf,
    origin: PathBuf,
    evil: PathBuf,
    state: PathBuf,
    fake_bin: PathBuf,
    marker: PathBuf,
}
impl Repo {
    fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut c = Command::new(program);
        c.env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.dir.path())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .current_dir(&self.root);
        c
    }
    fn git(&self, path: &Path, args: &[&str]) -> String {
        let out = self
            .command("git")
            .current_dir(path)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().into()
    }
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("main");
        let ticket = dir.path().join("ticket");
        let origin = dir.path().join("origin.git");
        let evil = dir.path().join("evil.git");
        let state = dir.path().join("state");
        let fake_bin = dir.path().join("bin");
        let marker = dir.path().join("evil-ran");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&fake_bin).unwrap();
        let r = Self {
            dir,
            root,
            ticket,
            origin,
            evil,
            state,
            fake_bin,
            marker,
        };
        r.git(&r.root, &["init", "-b", "trunk"]);
        r.git(&r.root, &["config", "user.name", "Fixture"]);
        r.git(
            &r.root,
            &["config", "user.email", "fixture@example.invalid"],
        );
        r.git(&r.root, &["config", "commit.gpgsign", "false"]);
        r.git(&r.root, &["commit", "--allow-empty", "-m", "base"]);
        for remote in [&r.origin, &r.evil] {
            r.git(&r.root, &["init", "--bare", remote.to_str().unwrap()]);
        }
        r.git(
            &r.root,
            &["remote", "add", "origin", r.origin.to_str().unwrap()],
        );
        r.git(&r.root, &["push", "origin", "trunk"]);
        r.git(
            &r.root,
            &[
                "worktree",
                "add",
                "-b",
                "feat/x",
                r.ticket.to_str().unwrap(),
            ],
        );
        r.git(&r.ticket, &["commit", "--allow-empty", "-m", "ticket"]);
        fs::write(
            r.fake_bin.join("gh"),
            "#!/bin/sh\necho 'no pull requests found for branch feat/x' >&2\nexit 1\n",
        )
        .unwrap();
        fs::set_permissions(r.fake_bin.join("gh"), fs::Permissions::from_mode(0o700)).unwrap();
        let store = Store::open(&r.state, &r.root).unwrap();
        store
            .update(|data| {
                data.tickets.push(Ticket {
                    id: "X".into(),
                    branch: "feat/x".into(),
                    base: "trunk".into(),
                    state: TicketState::Building,
                    worktree: Some(r.ticket.clone()),
                    ..Ticket::default()
                });
                Ok(())
            })
            .unwrap();
        let file = store.root().join("state.json");
        let mut data: serde_json::Value =
            serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
        data["trust"] = serde_json::json!({"project_root":r.root,"common_git_dir":r.root.join(".git"),"origin_url":r.origin.to_str().unwrap(),"push_url":r.origin.to_str().unwrap(),"default_branch":"trunk","config_commit":null,"config_blob":null,"config_text":null,"trusted_at":"fixture","global_config":"/dev/null","object_format":"sha1"});
        fs::write(file, serde_json::to_vec(&data).unwrap()).unwrap();
        r
    }
    fn push(&self) -> Output {
        self.command(env!("CARGO_BIN_EXE_herdr-shepherd"))
            .env(
                "PATH",
                std::env::join_paths([self.fake_bin.clone(), "/usr/bin".into(), "/bin".into()])
                    .unwrap(),
            )
            .env("SHEPHERD_TEST_ALLOW_LOCAL_ORIGIN", "allow-local-in-tmp")
            .args(["--json", "--state-dir"])
            .arg(&self.state)
            .args(["--config-dir"])
            .arg(self.dir.path().join("cfg"))
            .args(["pr", "push", "--branch", "feat/x", "--worktree"])
            .arg(&self.ticket)
            .output()
            .unwrap()
    }
    fn assert_original_only(&self, out: Output) {
        assert!(
            out.status.success(),
            "{} {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            !self.marker.exists(),
            "untrusted Git configuration executed in shepherd"
        );
        assert_eq!(
            self.git(&self.origin, &["rev-parse", "refs/heads/feat/x"]),
            self.git(&self.ticket, &["rev-parse", "HEAD"])
        );
        assert!(self.git(&self.evil, &["for-each-ref"]).is_empty());
    }
}

#[test]
fn shared_pushurl_cannot_redirect_shepherd_to_evil_remote() {
    let r = Repo::new();
    let hook = r.evil.join("hooks/pre-receive");
    fs::write(
        &hook,
        format!("#!/bin/sh\nprintf ran > '{}'\n", r.marker.display()),
    )
    .unwrap();
    fs::set_permissions(hook, fs::Permissions::from_mode(0o700)).unwrap();
    r.git(
        &r.ticket,
        &["config", "remote.origin.pushurl", r.evil.to_str().unwrap()],
    );
    r.assert_original_only(r.push());
}

#[test]
fn shared_ssh_helper_include_and_url_rewrite_never_execute() {
    let r = Repo::new();
    let helper = r.dir.path().join("malicious-helper");
    fs::write(
        &helper,
        format!("#!/bin/sh\nprintf ran > '{}'\nexit 1\n", r.marker.display()),
    )
    .unwrap();
    fs::set_permissions(&helper, fs::Permissions::from_mode(0o700)).unwrap();
    let include = r.dir.path().join("injected.gitconfig");
    fs::write(
        &include,
        format!(
            "[core]\nsshCommand = /bin/sh '{}'\n[credential]\nhelper = !/bin/sh '{}'\n",
            helper.display(),
            helper.display()
        ),
    )
    .unwrap();
    r.git(
        &r.ticket,
        &["config", "include.path", include.to_str().unwrap()],
    );
    r.git(
        &r.ticket,
        &[
            "config",
            "url.ssh://fixture.invalid/repo.pushInsteadOf",
            r.origin.to_str().unwrap(),
        ],
    );
    r.git(
        &r.ticket,
        &[
            "config",
            "remote.origin.receivepack",
            helper.to_str().unwrap(),
        ],
    );
    r.assert_original_only(r.push());
}

#[test]
fn local_transport_requires_the_explicit_debug_test_guard() {
    let r = Repo::new();
    let out = r
        .command(env!("CARGO_BIN_EXE_herdr-shepherd"))
        .env(
            "PATH",
            std::env::join_paths([r.fake_bin.clone(), "/usr/bin".into(), "/bin".into()]).unwrap(),
        )
        .args(["--json", "--state-dir"])
        .arg(&r.state)
        .args(["--config-dir"])
        .arg(r.dir.path().join("cfg"))
        .args(["pr", "push", "--branch", "feat/x", "--worktree"])
        .arg(&r.ticket)
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(4),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        r.git(&r.origin, &["for-each-ref", "refs/heads/feat/x"])
            .is_empty()
    );
}

#[test]
fn stored_branch_must_match_the_ticket_head() {
    let r = Repo::new();
    let tip = r.git(&r.ticket, &["rev-parse", "HEAD"]);
    r.git(&r.ticket, &["update-ref", "refs/heads/other", &tip]);
    r.git(&r.ticket, &["symbolic-ref", "HEAD", "refs/heads/other"]);
    let out = r.push();
    assert_eq!(
        out.status.code(),
        Some(4),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        r.git(&r.origin, &["for-each-ref", "refs/heads/feat/x"])
            .is_empty()
    );
}

#[test]
fn packed_ticket_ref_is_verified_and_pushed_from_owned_bare_repo() {
    let r = Repo::new();
    r.git(&r.ticket, &["pack-refs", "--all"]);
    r.assert_original_only(r.push());
    let bare = r.state.join("push").join(format!(
        "{}.git",
        herdr_shepherd::store::project_hash(&r.root)
    ));
    assert!(bare.join("objects/info/alternates").is_file());
    assert!(
        !fs::read_to_string(bare.join("config"))
            .unwrap()
            .contains("remote")
    );
}

#[test]
fn global_include_of_shared_config_cannot_bypass_url_guard() {
    let r = Repo::new();
    let global = r.dir.path().join("global.gitconfig");
    fs::write(
        &global,
        format!(
            "[include]\npath = \"{}\"\n",
            r.root.join(".git/config").display()
        ),
    )
    .unwrap();
    let hook = r.evil.join("hooks/pre-receive");
    fs::write(
        &hook,
        format!("#!/bin/sh\nprintf ran > '{}'\n", r.marker.display()),
    )
    .unwrap();
    fs::set_permissions(hook, fs::Permissions::from_mode(0o700)).unwrap();
    r.git(
        &r.ticket,
        &[
            "config",
            &format!("url.{}.pushInsteadOf", r.evil.display()),
            "https://fixture.invalid/repo",
        ],
    );
    let store = Store::open(&r.state, &r.root).unwrap();
    store
        .update(|data| {
            let trust = data.trust.as_mut().unwrap();
            trust.push_url = "https://fixture.invalid/repo".into();
            trust.origin_url = trust.push_url.clone();
            trust.global_config = Some(global);
            Ok(())
        })
        .unwrap();
    let out = r.push();
    assert_eq!(
        out.status.code(),
        Some(4),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        !r.marker.exists(),
        "shared config reached transport through a GLOBAL include"
    );
    assert!(r.git(&r.evil, &["for-each-ref"]).is_empty());
}

#[test]
fn scp_origin_with_real_empty_global_config_verifies_head_without_network() {
    let r = Repo::new();
    let global = r.dir.path().join("global.gitconfig");
    fs::write(&global, "").unwrap();
    Store::open(&r.state, &r.root)
        .unwrap()
        .update(|data| {
            let trust = data.trust.as_mut().unwrap();
            trust.origin_url = "git@github.com:o/r.git".into();
            trust.push_url = trust.origin_url.clone();
            trust.global_config = Some(global);
            Ok(())
        })
        .unwrap();
    let transport =
        herdr_shepherd::gate::transport::PushTransport::open(&r.state, &r.ticket).unwrap();
    assert_eq!(
        transport.verified_ticket_head("feat/x").unwrap(),
        r.git(&r.ticket, &["rev-parse", "HEAD"])
    );
}
