//! Push from a shepherd-owned Git repository, never the ticket's mutable config.
use crate::{
    error::ShepherdError,
    proc,
    store::{ProjectTrust, Store, project_hash},
};
use std::{
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

pub struct PushTransport {
    pub trust: ProjectTrust,
    root: PathBuf,
    state_dir: PathBuf,
    worktree: PathBuf,
    _lock: File,
    allow_local_for_tests: bool,
    global_copy: PathBuf,
}

fn denied(text: impl Into<String>) -> ShepherdError {
    ShepherdError::PolicyDenied(text.into())
}
fn checked_read(path: &Path, inside: &Path) -> Result<String, ShepherdError> {
    let actual = path.canonicalize().map_err(anyhow::Error::from)?;
    if !actual.starts_with(inside)
        || actual.components().any(|part| {
            let s = part.as_os_str().to_string_lossy();
            s.starts_with(".env")
                || s == "Credentials"
                || s.ends_with(".pem")
                || s.ends_with(".key")
                || matches!(s.as_ref(), "credentials" | "id_rsa" | "id_ed25519")
        })
    {
        return Err(denied("unsafe Git metadata path"));
    }
    fs::read_to_string(actual).map_err(|e| anyhow::Error::from(e).into())
}
fn oid(text: &str) -> bool {
    matches!(text.len(), 40 | 64) && text.bytes().all(|b| b.is_ascii_hexdigit())
}

fn global_quote(text: &str) -> String {
    format!(
        "\"{}\"",
        text.replace('\\', "\\\\")
            .replace('\"', "\\\"")
            .replace('\n', "\\n")
            .replace('\t', "\\t")
            .replace('\u{8}', "\\b")
    )
}

fn flatten_global_config(trust: &ProjectTrust, root: &Path) -> Result<PathBuf, ShepherdError> {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    let mut contents = String::new();
    if let Some(source) = &trust.global_config {
        let source = source.canonicalize().map_err(anyhow::Error::from)?;
        if source.starts_with(&trust.common_git_dir)
            || source.components().any(|part| {
                let name = part.as_os_str().to_string_lossy();
                name.starts_with(".env")
                    || name == "Credentials"
                    || name.ends_with(".pem")
                    || name.ends_with(".key")
                    || matches!(name.as_ref(), "credentials" | "id_rsa" | "id_ed25519")
            })
        {
            return Err(denied("unsafe global Git configuration path"));
        }
        if source != Path::new("/dev/null") {
            let mut command = Command::new("git");
            command.current_dir(root);
            for (key, _) in std::env::vars_os() {
                if key.to_string_lossy().starts_with("GIT_") {
                    command.env_remove(key);
                }
            }
            command
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_SYSTEM", "/dev/null");
            command
                .arg("--git-dir")
                .arg(root)
                .args(["config", "--no-includes", "--file"])
                .arg(&source)
                .args(["--list", "--null"]);
            let out = proc::capture(&mut command, Duration::from_secs(20), None)?;
            if !out.status.success() {
                return Err(denied("cannot read global Git settings without includes"));
            }
            for entry in out.stdout.split('\0').filter(|entry| !entry.is_empty()) {
                let (key, value) = entry.split_once('\n').unwrap_or((entry, "true"));
                let lower = key.to_ascii_lowercase();
                if lower.starts_with("include.") || lower.starts_with("includeif.") {
                    let path = if Path::new(value).is_absolute() {
                        PathBuf::from(value)
                    } else {
                        source
                            .parent()
                            .expect("global config has parent")
                            .join(value)
                    };
                    if path
                        .canonicalize()
                        .ok()
                        .is_some_and(|path| path.starts_with(&trust.common_git_dir))
                    {
                        return Err(denied(
                            "global Git config cannot include repository-local configuration",
                        ));
                    }
                    continue;
                }
                // The stored URL is canonical for this operation; even GLOBAL
                // rewrites must not route it to a different transport/host.
                if lower.starts_with("url.") {
                    continue;
                }
                let (section, rest) = key
                    .split_once('.')
                    .ok_or_else(|| denied("invalid global Git setting"))?;
                let (subsection, name) = rest
                    .rsplit_once('.')
                    .map_or((None, rest), |(subsection, name)| (Some(subsection), name));
                if !section
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                    || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                    || key.chars().any(|c| c.is_control())
                    || value.contains('\r')
                {
                    return Err(denied("ambiguous global Git setting"));
                }
                match subsection {
                    Some(subsection) => {
                        contents.push_str(&format!("[{section} {}]\n", global_quote(subsection)))
                    }
                    None => contents.push_str(&format!("[{section}]\n")),
                };
                contents.push_str(&format!("{name} = {}\n", global_quote(value)));
            }
        }
    }
    let target = root.join("global.gitconfig");
    let temporary = root.join(format!("global.gitconfig.tmp-{}", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)
        .map_err(anyhow::Error::from)?;
    file.write_all(contents.as_bytes())
        .map_err(anyhow::Error::from)?;
    file.sync_all().map_err(anyhow::Error::from)?;
    drop(file);
    fs::rename(&temporary, &target).map_err(anyhow::Error::from)?;
    Ok(target)
}

fn copy_object_file(
    source: &Path,
    destination: &Path,
    objects: &Path,
) -> Result<(), ShepherdError> {
    use std::{
        os::unix::fs::OpenOptionsExt,
        time::{SystemTime, UNIX_EPOCH},
    };
    if !fs::symlink_metadata(source)
        .map_err(anyhow::Error::from)?
        .file_type()
        .is_file()
    {
        return Err(denied("fetched object is not a regular file"));
    }
    if let Ok(metadata) = fs::symlink_metadata(destination) {
        if !metadata.file_type().is_file() {
            return Err(denied("source object destination is not a regular file"));
        }
        return Ok(());
    }
    let parent = destination.parent().expect("object has parent");
    fs::create_dir_all(parent).map_err(anyhow::Error::from)?;
    if !parent
        .canonicalize()
        .map_err(anyhow::Error::from)?
        .starts_with(objects)
    {
        return Err(denied("object destination left trusted object directory"));
    }
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(anyhow::Error::from)?
        .as_nanos();
    let temporary = parent.join(format!("tmp_shepherd_{}_{nonce}", std::process::id()));
    let result = (|| -> Result<(), ShepherdError> {
        let mut input = File::open(source).map_err(anyhow::Error::from)?;
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o444)
            .open(&temporary)
            .map_err(anyhow::Error::from)?;
        std::io::copy(&mut input, &mut output).map_err(anyhow::Error::from)?;
        output.sync_all().map_err(anyhow::Error::from)?;
        drop(output);
        match fs::hard_link(&temporary, destination) {
            Ok(()) => (),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (),
            Err(error) => return Err(anyhow::Error::from(error).into()),
        }
        File::open(parent)
            .and_then(|dir| dir.sync_all())
            .map_err(anyhow::Error::from)?;
        Ok(())
    })();
    let _ = fs::remove_file(&temporary);
    result
}

#[derive(Clone, Copy, Default)]
pub struct TransportOptions {
    /// Only honored in debug builds for state and remote paths under /tmp.
    pub allow_local_for_tests: bool,
}

impl PushTransport {
    pub fn open(state_dir: &Path, worktree: &Path) -> Result<Self, ShepherdError> {
        Self::open_with_options(state_dir, worktree, TransportOptions::default())
    }
    pub fn open_with_options(
        state_dir: &Path,
        worktree: &Path,
        options: TransportOptions,
    ) -> Result<Self, ShepherdError> {
        let trust = crate::trusted::load_snapshot(state_dir, worktree)?;
        let state_dir = state_dir.canonicalize().map_err(anyhow::Error::from)?;
        let push_root = state_dir.join("push");
        fs::create_dir_all(&push_root).map_err(anyhow::Error::from)?;
        let hash = project_hash(&trust.project_root);
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(push_root.join(format!("{hash}.lock")))
            .map_err(anyhow::Error::from)?;
        lock.lock().map_err(anyhow::Error::from)?;
        let root = push_root.join(format!("{hash}.git"));
        for relative in ["objects/info", "refs/heads", "hooks"] {
            fs::create_dir_all(root.join(relative)).map_err(anyhow::Error::from)?;
        }
        let root = root.canonicalize().map_err(anyhow::Error::from)?;
        if !root.starts_with(&state_dir) {
            return Err(denied("push repository must remain in shepherd state"));
        }
        if !matches!(trust.object_format.as_str(), "sha1" | "sha256") {
            return Err(denied("unsupported Git object format"));
        }
        let mut config =
            "[core]\nrepositoryformatversion = 0\nbare = true\nlogallrefupdates = false\n"
                .to_owned();
        if trust.object_format == "sha256" {
            config="[core]\nrepositoryformatversion = 1\nbare = true\nlogallrefupdates = false\n[extensions]\nobjectformat = sha256\n".into();
        }
        // The only repository config consulted by transport is written here.
        fs::write(root.join("config"), config).map_err(anyhow::Error::from)?;
        fs::write(root.join("HEAD"), "ref: refs/heads/shepherd-transport\n")
            .map_err(anyhow::Error::from)?;
        let objects = trust
            .common_git_dir
            .join("objects")
            .canonicalize()
            .map_err(anyhow::Error::from)?;
        if !objects.starts_with(&trust.common_git_dir) {
            return Err(denied(
                "source object directory left the trusted repository",
            ));
        }
        fs::write(
            root.join("objects/info/alternates"),
            format!("{}\n", objects.display()),
        )
        .map_err(anyhow::Error::from)?;
        let global_copy = flatten_global_config(&trust, &root)?;
        Ok(Self {
            trust,
            root,
            state_dir,
            worktree: worktree.to_path_buf(),
            _lock: lock,
            allow_local_for_tests: options.allow_local_for_tests,
            global_copy,
        })
    }

    pub(crate) fn state_dir(&self) -> &Path {
        &self.state_dir
    }
    pub(crate) fn allow_local_for_tests(&self) -> bool {
        self.allow_local_for_tests
    }

    fn command(&self) -> Result<Command, ShepherdError> {
        let mut command = Command::new("git");
        command.current_dir(&self.root);
        crate::gate::env::prepare_provider_command(&mut command, &self.state_dir, true)?;
        for (key, _) in std::env::vars_os() {
            let name = key.to_string_lossy();
            if name.starts_with("GIT_") || name == "SSH_ASKPASS" {
                command.env_remove(key);
            }
        }
        command
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ASKPASS", "/bin/false")
            .env("SSH_ASKPASS", "/bin/false");
        command.env("GIT_CONFIG_GLOBAL", &self.global_copy);
        command.arg("--git-dir").arg(&self.root).args([
            "-c",
            "push.followTags=false",
            "-c",
            "push.recurseSubmodules=no",
            "-c",
            "core.hooksPath=",
        ]);
        // No local credential or SSH values enter this repository. Reset the
        // generic helpers and restore only matching values from GLOBAL.
        command.args(["-c", "credential.helper="]);
        for (index, (key, value)) in self.global_helpers()?.into_iter().enumerate() {
            let variable = format!("SHEPHERD_TRANSPORT_HELPER_{index}");
            command
                .env(&variable, value)
                .arg(format!("--config-env={key}={variable}"));
        }
        let ssh = self
            .global_values("core.sshCommand")?
            .pop()
            .unwrap_or_else(|| "ssh".into());
        command
            .env("SHEPHERD_TRANSPORT_SSH", ssh)
            .arg("--config-env=core.sshCommand=SHEPHERD_TRANSPORT_SSH");
        Ok(command)
    }

    fn global_values(&self, key: &str) -> Result<Vec<String>, ShepherdError> {
        let path = &self.global_copy;
        let mut c = Command::new("git");
        c.current_dir(&self.root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1");
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("GIT_") {
                c.env_remove(key);
            }
        }
        c.env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1");
        c.args(["config", "--no-includes", "--file"]).arg(path);
        c.args(["--get-all", key]);
        let out = proc::capture(&mut c, Duration::from_secs(20), None)?;
        if out.status.code() == Some(1) {
            return Ok(Vec::new());
        }
        if !out.status.success() {
            return Err(denied("cannot read global Git transport setting"));
        }
        Ok(out.stdout.lines().map(str::to_owned).collect())
    }

    fn global_helpers(&self) -> Result<Vec<(String, String)>, ShepherdError> {
        let mut command = Command::new("git");
        command.current_dir(&self.root);
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("GIT_") {
                command.env_remove(key);
            }
        }
        command
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_SYSTEM", "/dev/null");
        command
            .arg("--git-dir")
            .arg(&self.root)
            .args(["config", "--no-includes", "--file"])
            .arg(&self.global_copy)
            .args(["--list", "--null"]);
        let out = proc::capture(&mut command, Duration::from_secs(20), None)?;
        if !out.status.success() {
            return Err(denied("cannot inspect GLOBAL credential helpers"));
        }
        Ok(out
            .stdout
            .split('\0')
            .filter_map(|entry| {
                let (key, value) = entry.split_once('\n')?;
                let lower = key.to_ascii_lowercase();
                (lower == "credential.helper"
                    || (lower.starts_with("credential.") && lower.ends_with(".helper")))
                .then(|| (key.into(), value.into()))
            })
            .collect())
    }

    pub(crate) fn execute(&self, args: &[&str]) -> Result<String, ShepherdError> {
        let output = proc::capture(self.command()?.args(args), Duration::from_secs(120), None)?;
        if !output.status.success() {
            return Err(ShepherdError::Runtime(anyhow::anyhow!(
                "Git transport failed ({}): {}",
                output.status,
                output.stderr.trim()
            )));
        }
        Ok(output.stdout)
    }

    pub fn read_ref(&self, reference: &str) -> Result<Option<String>, ShepherdError> {
        if !reference.starts_with("refs/")
            || reference
                .split('/')
                .any(|p| p.is_empty() || p == "." || p == ".." || p.ends_with(".lock"))
            || reference.contains(['\\', '\n', '\r', ' '])
        {
            return Err(denied("unsafe Git reference"));
        }
        let path = self.trust.common_git_dir.join(reference);
        let value = match fs::symlink_metadata(&path) {
            Ok(_) => Some(
                checked_read(&path, &self.trust.common_git_dir)?
                    .trim()
                    .to_owned(),
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let packed = self.trust.common_git_dir.join("packed-refs");
                if !packed.exists() {
                    None
                } else {
                    checked_read(&packed, &self.trust.common_git_dir)?
                        .lines()
                        .find_map(|line| {
                            line.split_once(' ')
                                .filter(|(_, name)| *name == reference)
                                .map(|(value, _)| value.to_owned())
                        })
                }
            }
            Err(e) => return Err(anyhow::Error::from(e).into()),
        };
        if value.as_ref().is_some_and(|value| !oid(value)) {
            return Err(denied("Git reference is not a direct object ID"));
        }
        Ok(value)
    }

    fn git_dir(&self, path: &Path) -> Result<PathBuf, ShepherdError> {
        let marker = path.join(".git");
        let git_dir = if marker.is_dir() {
            marker.canonicalize().map_err(anyhow::Error::from)?
        } else {
            let contents = checked_read(&marker, path)?;
            let relative = contents
                .trim()
                .strip_prefix("gitdir: ")
                .ok_or_else(|| denied("invalid worktree Git marker"))?;
            let git_dir = if Path::new(relative).is_absolute() {
                PathBuf::from(relative)
            } else {
                path.join(relative)
            };
            git_dir.canonicalize().map_err(anyhow::Error::from)?
        };
        if git_dir != self.trust.common_git_dir
            && !git_dir.starts_with(self.trust.common_git_dir.join("worktrees"))
        {
            return Err(denied("worktree Git directory left the trusted repository"));
        }
        Ok(git_dir)
    }
    pub fn matches_repository(&self, path: &Path) -> Result<bool, ShepherdError> {
        self.git_dir(&path.canonicalize().map_err(anyhow::Error::from)?)
            .map(|_| true)
    }
    pub fn verified_ticket_head(&self, branch: &str) -> Result<String, ShepherdError> {
        let path = self.worktree.canonicalize().map_err(anyhow::Error::from)?;
        let data = Store::open(&self.state_dir, &self.trust.project_root)?.read()?;
        let registered = data.tickets.iter().any(|t| {
            (t.branch == branch && t.worktree.as_ref() == Some(&path))
                || t.children
                    .iter()
                    .any(|c| c.branch == branch && c.path == path)
        });
        if !registered {
            return Err(denied(
                "push requires the stored ticket worktree and branch",
            ));
        }
        self.checkout_head(&path, branch)
    }

    pub(crate) fn checkout_head(&self, path: &Path, branch: &str) -> Result<String, ShepherdError> {
        let git_dir = self.git_dir(path)?;
        let head = checked_read(&git_dir.join("HEAD"), &self.trust.common_git_dir)?;
        let reference = format!("refs/heads/{branch}");
        if head.trim() != format!("ref: {reference}") {
            return Err(denied("ticket worktree is not on its stored branch"));
        }
        let tip = self
            .read_ref(&reference)?
            .ok_or_else(|| denied("ticket branch is missing"))?;
        if self.execute(&["cat-file", "-t", &tip])?.trim() != "commit" {
            return Err(denied("ticket HEAD is not a commit"));
        }
        // Bind the object resolved before network access to the current HEAD.
        if checked_read(&git_dir.join("HEAD"), &self.trust.common_git_dir)? != head
            || self.read_ref(&reference)?.as_deref() != Some(&tip)
        {
            return Err(denied("ticket HEAD changed during authorization"));
        }
        Ok(tip)
    }

    fn url(&self) -> Result<&str, ShepherdError> {
        let url = &self.trust.push_url;
        if url.starts_with("https://")
            || url.starts_with("ssh://")
            || (url.starts_with("git@") && url.contains(':'))
        {
            return Ok(url);
        }
        #[cfg(debug_assertions)]
        if (self.allow_local_for_tests
            || std::env::var("SHEPHERD_TEST_ALLOW_LOCAL_ORIGIN").as_deref()
                == Ok("allow-local-in-tmp"))
            && self.state_dir.starts_with("/tmp")
        {
            let local = url.strip_prefix("file://").unwrap_or(url);
            if Path::new(local).is_absolute()
                && Path::new(local)
                    .canonicalize()
                    .ok()
                    .is_some_and(|p| p.starts_with("/tmp"))
            {
                return Ok(url);
            }
        }
        Err(denied(
            "push URL must use HTTPS or SSH; local transports are test-only",
        ))
    }

    pub fn push_ticket(&self, branch: &str) -> Result<String, ShepherdError> {
        self.check_destination(branch)?;
        let tip = self.verified_ticket_head(branch)?;
        self.execute(&[
            "push",
            "--porcelain",
            "--",
            self.url()?,
            &format!("{tip}:refs/heads/{branch}"),
        ])
    }
    /// Bind a revision push to the exact commit that passed QA. Changes to
    /// the branch after authorization cannot change the explicit source SHA.
    pub fn push_ticket_expected(
        &self,
        branch: &str,
        expected: &str,
    ) -> Result<String, ShepherdError> {
        self.check_destination(branch)?;
        if !oid(expected) || self.verified_ticket_head(branch)? != expected {
            return Err(ShepherdError::Conflict {
                unit: 0,
                range: "ticket HEAD differs from the QA-tested commit".into(),
            });
        }
        self.execute(&[
            "push",
            "--porcelain",
            "--",
            self.url()?,
            &format!("{expected}:refs/heads/{branch}"),
        ])
    }

    pub fn remote_tip(&self, branch: &str) -> Result<Option<String>, ShepherdError> {
        let reference = format!("refs/heads/{branch}");
        let out = self.execute(&["ls-remote", "--heads", "--", self.url()?, &reference])?;
        let rows: Vec<_> = out.lines().collect();
        if rows.is_empty() {
            return Ok(None);
        }
        let parts: Vec<_> = rows[0].split_whitespace().collect();
        if rows.len() != 1 || parts.len() != 2 || parts[1] != reference || !oid(parts[0]) {
            return Err(denied("unexpected remote branch response"));
        }
        Ok(Some(parts[0].into()))
    }
    pub fn fetch_branch(&self, branch: &str) -> Result<String, ShepherdError> {
        let reference = format!("refs/heads/{branch}");
        let destination = format!("refs/shepherd/base/{branch}");
        self.execute(&[
            "fetch",
            "--no-tags",
            "--",
            self.url()?,
            &format!("+{reference}:{destination}"),
        ])?;
        let tip = self
            .execute(&["rev-parse", "--verify", &destination])?
            .trim()
            .to_owned();
        if !oid(&tip) {
            return Err(denied("fetched base is not an object ID"));
        }
        self.import_fetched_objects()?;
        Ok(tip)
    }
    /// Fetch a revision into an owned ticket ref, never a worktree remote.
    pub fn fetch_ticket(&self, ticket: &str, branch: &str) -> Result<String, ShepherdError> {
        let destination = format!("refs/shepherd/{ticket}");
        self.execute(&["check-ref-format", &destination])?;
        self.execute(&["check-ref-format", &format!("refs/heads/{branch}")])?;
        self.execute(&[
            "fetch",
            "--no-tags",
            "--",
            self.url()?,
            &format!("+refs/heads/{branch}:{destination}"),
        ])?;
        let tip = self
            .execute(&["rev-parse", "--verify", &destination])?
            .trim()
            .to_owned();
        if !oid(&tip) {
            return Err(denied("fetched revision is not an object ID"));
        }
        self.import_fetched_objects()?;
        Ok(tip)
    }

    pub(crate) fn ownership_marker(&self, path: &Path) -> Result<Option<String>, ShepherdError> {
        let common = self.trust.common_git_dir.join("config");
        if self
            .config_value(&common, "extensions.worktreeConfig", true)?
            .as_deref()
            != Some("true")
        {
            return Ok(None);
        }
        self.config_value(
            &self.git_dir(path)?.join("config.worktree"),
            "shepherd.ticketBranch",
            false,
        )
    }

    fn config_value(
        &self,
        path: &Path,
        key: &str,
        boolean: bool,
    ) -> Result<Option<String>, ShepherdError> {
        let actual = match path.canonicalize() {
            Ok(path) => path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(anyhow::Error::from(error).into()),
        };
        if !actual.starts_with(&self.trust.common_git_dir) || !actual.is_file() {
            return Err(denied(
                "worktree metadata config left the trusted repository",
            ));
        }
        let contents = checked_read(&actual, &self.trust.common_git_dir)?;
        let mut command = self.command()?;
        command.args(["config", "--no-includes", "--file", "-"]);
        if boolean {
            command.arg("--bool");
        }
        let out = proc::capture(
            command.args(["--get", key]),
            Duration::from_secs(20),
            Some(contents.as_bytes()),
        )?;
        if out.status.code() == Some(1) {
            return Ok(None);
        }
        if !out.status.success() {
            return Err(denied("cannot read worktree ownership metadata"));
        }
        Ok(Some(out.stdout.trim().into()))
    }

    fn local_command(&self, path: &Path, head: &str) -> Result<Command, ShepherdError> {
        if !oid(head) {
            return Err(denied("local operation requires a verified commit"));
        }
        let git_dir = self.git_dir(path)?;
        let index = git_dir.join("index");
        if let Ok(metadata) = fs::symlink_metadata(&index)
            && !metadata.file_type().is_file()
        {
            return Err(denied("worktree index must be a regular file"));
        }
        // A separate Git directory isolates every repository setting, including
        // filters, includes, attributesFile and worktree-specific config. Only
        // the index is shared with the registered worktree.
        let local = self.root.join("local.git");
        fs::create_dir_all(local.join("objects/info")).map_err(anyhow::Error::from)?;
        fs::create_dir_all(local.join("refs")).map_err(anyhow::Error::from)?;
        fs::create_dir_all(local.join("info")).map_err(anyhow::Error::from)?;
        let exclude = self.trust.common_git_dir.join("info/exclude");
        let ignored = if exclude.exists() {
            checked_read(&exclude, &self.trust.common_git_dir)?
        } else {
            String::new()
        };
        fs::write(local.join("info/exclude"), ignored).map_err(anyhow::Error::from)?;
        let config = if self.trust.object_format == "sha256" {
            "[core]\nrepositoryformatversion=1\nbare=false\n[extensions]\nobjectformat=sha256\n"
        } else {
            "[core]\nrepositoryformatversion=0\nbare=false\n"
        };
        fs::write(local.join("config"), config).map_err(anyhow::Error::from)?;
        fs::write(local.join("HEAD"), format!("{head}\n")).map_err(anyhow::Error::from)?;
        fs::write(
            local.join("objects/info/alternates"),
            format!("{}\n", self.trust.common_git_dir.join("objects").display()),
        )
        .map_err(anyhow::Error::from)?;
        // Split indexes name their shared immutable index by hash in GIT_DIR.
        for entry in fs::read_dir(&git_dir).map_err(anyhow::Error::from)? {
            let entry = entry.map_err(anyhow::Error::from)?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.strip_prefix("sharedindex.").is_some_and(oid) {
                if !entry.file_type().map_err(anyhow::Error::from)?.is_file() {
                    return Err(denied("shared index must be a regular file"));
                }
                fs::copy(entry.path(), local.join(name)).map_err(anyhow::Error::from)?;
            }
        }
        let mut command = Command::new("git");
        command.current_dir(&self.root);
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().starts_with("GIT_") {
                command.env_remove(name);
            }
        }
        command
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_ATTR_NOSYSTEM", "1")
            .env("GIT_INDEX_FILE", index)
            .env("GIT_NO_REPLACE_OBJECTS", "1")
            .arg("--git-dir")
            .arg(local)
            .arg("--work-tree")
            .arg(path)
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "core.fsmonitor=false",
                "-c",
                "core.attributesFile=/dev/null",
                "-c",
                "core.splitIndex=false",
            ]);
        Ok(command)
    }

    pub fn worktree_status(&self, branch: &str) -> Result<String, ShepherdError> {
        self.verified_ticket_head(branch)?;
        self.checkout_status(&self.worktree, branch)
    }

    pub(crate) fn checkout_status(
        &self,
        path: &Path,
        branch: &str,
    ) -> Result<String, ShepherdError> {
        let head = self.checkout_head(path, branch)?;
        let out = proc::capture(
            self.local_command(path, &head)?
                .args(["status", "--porcelain"]),
            Duration::from_secs(60),
            None,
        )?;
        if !out.status.success() {
            return Err(denied(format!(
                "cannot inspect ticket worktree: {}",
                out.stderr.trim()
            )));
        }
        Ok(out.stdout)
    }

    /// Update the worktree using owned config, then atomically publish only the
    /// stored branch. A detached shadow HEAD prevents merge from changing any
    /// source ref before the expected-tip check and lock are established.
    pub fn fast_forward_ticket(
        &self,
        branch: &str,
        expected: &str,
        target: &str,
    ) -> Result<(), ShepherdError> {
        self.check_destination(branch)?;
        if self.verified_ticket_head(branch)? != expected {
            return Err(denied("ticket changed before fast-forward"));
        }
        self.fast_forward_checkout(&self.worktree, branch, expected, target)
    }

    pub(crate) fn sync_merged_base(
        &self,
        proof: &crate::cleanup::MergeProof,
    ) -> Result<(), ShepherdError> {
        let (path, branch, target) = proof.base_sync_target(self)?;
        let expected = self.checkout_head(path, branch)?;
        if !self.checkout_status(path, branch)?.is_empty() {
            return Err(denied("main checkout is dirty"));
        }
        self.fast_forward_checkout(path, branch, &expected, target)
    }

    fn fast_forward_checkout(
        &self,
        path: &Path,
        branch: &str,
        expected: &str,
        target: &str,
    ) -> Result<(), ShepherdError> {
        use std::{io::Write, os::unix::fs::OpenOptionsExt};
        let conflict = || ShepherdError::Conflict {
            unit: 0,
            range: "ticket branch changed during sync".into(),
        };
        self.execute(&["check-ref-format", &format!("refs/heads/{branch}")])?;
        if self.checkout_head(path, branch)? != expected
            || !oid(target)
            || !self.is_ancestor(expected, target)?
        {
            return Err(conflict());
        }
        let mut parent = self.trust.common_git_dir.clone();
        let reference = format!("refs/heads/{branch}");
        let parts: Vec<_> = reference.split('/').collect();
        for part in &parts[..parts.len() - 1] {
            parent.push(part);
            match fs::symlink_metadata(&parent) {
                Ok(metadata) if metadata.file_type().is_dir() => (),
                Ok(_) => return Err(denied("branch directory must be a real directory")),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    fs::create_dir(&parent).map_err(anyhow::Error::from)?;
                }
                Err(error) => return Err(anyhow::Error::from(error).into()),
            }
        }
        let destination = parent.join(parts.last().expect("nonempty reference"));
        let lock_path = parent.join(format!(
            "{}.lock",
            parts.last().expect("nonempty reference")
        ));
        let mut lock = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&lock_path)
            .map_err(|_| conflict())?;
        let mut published = false;
        let result = (|| {
            if self.checkout_head(path, branch)? != expected {
                return Err(conflict());
            }
            let out = proc::capture(
                self.local_command(path, expected)?
                    .args(["merge", "--ff-only", target]),
                Duration::from_secs(60),
                None,
            )?;
            if !out.status.success() {
                return Err(ShepherdError::Conflict {
                    unit: 0,
                    range: out.stderr.trim().into(),
                });
            }
            if self.checkout_head(path, branch)? != expected {
                return Err(conflict());
            }
            lock.write_all(format!("{target}\n").as_bytes())
                .map_err(anyhow::Error::from)?;
            lock.sync_all().map_err(anyhow::Error::from)?;
            fs::rename(&lock_path, destination).map_err(anyhow::Error::from)?;
            published = true;
            File::open(&parent)
                .and_then(|directory| directory.sync_all())
                .map_err(anyhow::Error::from)?;
            Ok(())
        })();
        if !published {
            let _ = fs::remove_file(lock_path);
        }
        result
    }

    fn import_fetched_objects(&self) -> Result<(), ShepherdError> {
        // A fetched merge may exist only in this owned repository. Import its
        // immutable object files before cleanup can remove any worktree or
        // fast-forward the source checkout. No source Git config is loaded.
        let source = self.root.join("objects");
        let target = self
            .trust
            .common_git_dir
            .join("objects")
            .canonicalize()
            .map_err(anyhow::Error::from)?;
        if !target.starts_with(&self.trust.common_git_dir) {
            return Err(denied(
                "source object directory left the trusted repository",
            ));
        }
        let mut files = Vec::new();
        for entry in fs::read_dir(&source).map_err(anyhow::Error::from)? {
            let entry = entry.map_err(anyhow::Error::from)?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.len() == 2 && name.bytes().all(|b| b.is_ascii_hexdigit()) {
                if !entry.file_type().map_err(anyhow::Error::from)?.is_dir() {
                    return Err(denied("unexpected loose object directory"));
                }
                for file in fs::read_dir(entry.path()).map_err(anyhow::Error::from)? {
                    let file = file.map_err(anyhow::Error::from)?;
                    let tail = file.file_name().to_string_lossy().into_owned();
                    if matches!(tail.len(), 38 | 62) && tail.bytes().all(|b| b.is_ascii_hexdigit())
                    {
                        files.push((file.path(), target.join(&name).join(tail)));
                    }
                }
            } else if name == "pack" {
                if !entry.file_type().map_err(anyhow::Error::from)?.is_dir() {
                    return Err(denied("unexpected object pack directory"));
                }
                for file in fs::read_dir(entry.path()).map_err(anyhow::Error::from)? {
                    let file = file.map_err(anyhow::Error::from)?;
                    let name = file.file_name().to_string_lossy().into_owned();
                    if let Some((stem, extension)) = name.rsplit_once('.')
                        && matches!(extension, "pack" | "idx" | "rev")
                        && stem.strip_prefix("pack-").is_some_and(oid)
                    {
                        files.push((file.path(), target.join("pack").join(name)));
                    }
                }
            }
        }
        // Publish packs before their indexes can advertise them to readers.
        files.sort_by_key(|(_, target)| target.extension().is_some_and(|ext| ext != "pack"));
        for (source, destination) in files {
            copy_object_file(&source, &destination, &target)?;
        }
        Ok(())
    }
    pub fn is_ancestor(&self, source: &str, base: &str) -> Result<bool, ShepherdError> {
        let out = proc::capture(
            self.command()?
                .args(["merge-base", "--is-ancestor", source, base]),
            Duration::from_secs(20),
            None,
        )?;
        match out.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(denied("cannot prove commit containment")),
        }
    }
    pub fn patch_equivalent(&self, branch_tip: &str, base: &str) -> Result<bool, ShepherdError> {
        Ok(self
            .execute(&["cherry", base, branch_tip])?
            .lines()
            .all(|line| line.starts_with("- ")))
    }
    fn check_destination(&self, branch: &str) -> Result<(), ShepherdError> {
        if crate::gate::BranchPolicy::new(&[], Some(&self.trust.default_branch), None)?
            .is_protected(branch)
        {
            return Err(denied("transport destination is protected"));
        }
        Ok(())
    }
    pub fn delete_branch(&self, branch: &str, expected: &str) -> Result<(), ShepherdError> {
        self.check_destination(branch)?;
        if !oid(expected) {
            return Err(denied("invalid deletion lease"));
        }
        let data = Store::open(&self.state_dir, &self.trust.project_root)?.read()?;
        if !data.tickets.iter().any(|t| t.branch == branch) {
            return Err(denied("remote deletion requires a stored ticket"));
        }
        if self.remote_tip(branch)?.as_deref() != Some(expected) {
            return Err(denied("remote branch changed since merge proof"));
        }
        self.execute(&[
            "push",
            &format!("--force-with-lease=refs/heads/{branch}:{expected}"),
            "--",
            self.url()?,
            &format!(":refs/heads/{branch}"),
        ])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn transport_helpers_restore_owned_auth_sentinels() {
        use std::os::unix::fs::PermissionsExt;
        if std::env::var("SHEPHERD_TEST_PROVIDER_CHILD").as_deref() != Ok("fixture") {
            let dir = tempfile::tempdir_in("/tmp").unwrap();
            let fake = dir.path().join("fake");
            let shim = dir.path().join("shim");
            fs::create_dir(&fake).unwrap();
            fs::create_dir(&shim).unwrap();
            let marker = dir.path().join("provider-env");
            let script = format!(
                "#!/bin/sh\nprintf '%s\n' \"${{GH_TOKEN-unset}}\" \"${{GITHUB_TOKEN-unset}}\" \"${{GH_CONFIG_DIR-unset}}\" > {}\nprintf 'username=fixture\npassword=fixture-password\n'\n",
                crate::gate::shim::shell_quote(&marker)
            );
            fs::write(fake.join("gh"), script).unwrap();
            fs::set_permissions(fake.join("gh"), fs::Permissions::from_mode(0o700)).unwrap();
            fs::write(shim.join("gh"), "#!/bin/sh\nexit 70\n").unwrap();
            fs::set_permissions(shim.join("gh"), fs::Permissions::from_mode(0o700)).unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "gate::transport::tests::transport_helpers_restore_owned_auth_sentinels",
                    "--nocapture",
                ])
                .env("SHEPHERD_TEST_PROVIDER_CHILD", "fixture")
                .env("SHEPHERD_TEST_PROVIDER_ROOT", dir.path())
                .env(
                    "PATH",
                    std::env::join_paths([
                        shim,
                        fake,
                        std::path::PathBuf::from("/usr/bin"),
                        std::path::PathBuf::from("/bin"),
                    ])
                    .unwrap(),
                )
                .env("GH_TOKEN", crate::gate::env::NO_TOKEN)
                .env("GITHUB_TOKEN", crate::gate::env::NO_TOKEN)
                .env("GH_ENTERPRISE_TOKEN", crate::gate::env::NO_TOKEN)
                .env("GITHUB_ENTERPRISE_TOKEN", crate::gate::env::NO_TOKEN)
                .env("GH_CONFIG_DIR", dir.path().join("agent-gh"))
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{} {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(fs::read_to_string(marker).unwrap(), "unset\nunset\nunset\n");
            return;
        }
        let root =
            std::path::PathBuf::from(std::env::var_os("SHEPHERD_TEST_PROVIDER_ROOT").unwrap());
        let global = root.join("provider-global");
        fs::write(&global, "[credential]\nhelper = !gh auth git-credential\n").unwrap();
        let trust = ProjectTrust {
            project_root: root.clone(),
            common_git_dir: root.join("source.git"),
            origin_url: "https://github.com/o/r".into(),
            push_url: "https://github.com/o/r".into(),
            default_branch: "main".into(),
            config_commit: None,
            config_blob: None,
            config_text: None,
            trusted_at: "fixture".into(),
            global_config: Some(global),
            object_format: "sha1".into(),
        };
        let global_copy = flatten_global_config(&trust, &root).unwrap();
        let transport = PushTransport {
            trust,
            root: root.clone(),
            state_dir: root.clone(),
            worktree: root.clone(),
            _lock: File::create(root.join("lock")).unwrap(),
            allow_local_for_tests: false,
            global_copy,
        };
        fs::create_dir(root.join("objects")).unwrap();
        fs::create_dir(root.join("refs")).unwrap();
        fs::write(root.join("HEAD"), "ref: refs/heads/fixture\n").unwrap();
        fs::write(root.join("config"), "[core]\nbare = true\n").unwrap();
        let result = crate::proc::capture(
            transport.command().unwrap().args(["credential", "fill"]),
            std::time::Duration::from_secs(5),
            Some(b"protocol=https\nhost=github.com\n\n"),
        )
        .unwrap();
        assert!(result.status.success(), "{}", result.stderr);
        assert!(result.stdout.contains("username=fixture"));
    }

    #[test]
    fn transport_restores_only_global_matching_helpers_and_ssh() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("real-global.gitconfig");
        fs::write(&global,"[credential]\nhelper = global-default\n[credential \"https://github.com\"]\nhelper = !fixture-global-helper\n[core]\nsshCommand = fixture-global-ssh\n").unwrap();
        let trust = ProjectTrust {
            project_root: dir.path().into(),
            common_git_dir: dir.path().join("source.git"),
            origin_url: "https://github.com/o/r".into(),
            push_url: "https://github.com/o/r".into(),
            default_branch: "main".into(),
            config_commit: None,
            config_blob: None,
            config_text: None,
            trusted_at: "fixture".into(),
            global_config: Some(global.clone()),
            object_format: "sha1".into(),
        };
        let global_copy = flatten_global_config(&trust, dir.path()).unwrap();
        assert_ne!(global_copy, global);
        let transport = PushTransport {
            trust,
            root: dir.path().into(),
            state_dir: dir.path().into(),
            worktree: dir.path().into(),
            _lock: File::create(dir.path().join("lock")).unwrap(),
            allow_local_for_tests: false,
            global_copy,
        };
        fs::create_dir_all(dir.path().join("objects")).unwrap();
        fs::create_dir_all(dir.path().join("refs")).unwrap();
        fs::write(dir.path().join("HEAD"), "ref: refs/heads/fixture\n").unwrap();
        fs::write(dir.path().join("config"), "[core]\nbare = true\n").unwrap();
        let mut command = transport.command().unwrap();
        let args: Vec<_> = command
            .get_args()
            .map(|s| s.to_string_lossy().into_owned())
            .collect();
        assert!(args.contains(&"credential.helper=".to_owned()));
        assert!(
            args.contains(&"--config-env=credential.helper=SHEPHERD_TRANSPORT_HELPER_0".to_owned())
        );
        assert!(
            args.contains(
                &"--config-env=credential.https://github.com.helper=SHEPHERD_TRANSPORT_HELPER_1"
                    .to_owned()
            )
        );
        let env: std::collections::HashMap<_, _> = command
            .get_envs()
            .filter_map(|(key, value)| {
                value.map(|value| {
                    (
                        key.to_string_lossy().into_owned(),
                        value.to_string_lossy().into_owned(),
                    )
                })
            })
            .collect();
        assert_eq!(env["SHEPHERD_TRANSPORT_HELPER_0"], "global-default");
        assert_eq!(env["SHEPHERD_TRANSPORT_HELPER_1"], "!fixture-global-helper");
        assert_eq!(env["SHEPHERD_TRANSPORT_SSH"], "fixture-global-ssh");
        let output = command
            .args(["config", "--get-all", "credential.helper"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
