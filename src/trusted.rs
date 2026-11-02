//! Project trust is captured only by an authorized start or explicit refresh.
//! Ordinary callers consume stored bytes and never re-read mutable Git refs.
use crate::{
    config::{self, Config},
    error::ShepherdError,
    proc,
    store::{ProjectTrust, Store, StoreData, project_hash},
};
use serde::Serialize;
use std::{
    fs,
    io::Read,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

pub fn default_state_dir() -> PathBuf {
    std::env::var_os("HERDR_PLUGIN_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| ".".into())
                .join(".local/state/herdr/plugins/herdr-shepherd")
        })
}

fn denied(message: &str) -> ShepherdError {
    ShepherdError::PolicyDenied(message.into())
}
fn safe_path(path: &Path) -> Result<PathBuf, ShepherdError> {
    let actual = path.canonicalize().map_err(anyhow::Error::from)?;
    if actual.components().any(|part| {
        let name = part.as_os_str().to_string_lossy();
        name.starts_with(".env")
            || name == "Credentials"
            || name.ends_with(".pem")
            || name.ends_with(".key")
            || matches!(name.as_ref(), "credentials" | "id_rsa" | "id_ed25519")
    }) {
        return Err(denied("secret paths cannot supply project trust"));
    }
    Ok(actual)
}

struct Repository {
    git_dir: PathBuf,
    main: PathBuf,
    common: PathBuf,
}
fn repository(path: &Path) -> Result<Option<Repository>, ShepherdError> {
    let root = safe_path(path)?;
    let Some(checkout) = root
        .ancestors()
        .find(|p| fs::symlink_metadata(p.join(".git")).is_ok())
    else {
        return Ok(None);
    };
    let marker = safe_path(&checkout.join(".git"))?;
    let git_dir = if marker.is_dir() {
        marker
    } else {
        let text = fs::read_to_string(&marker).map_err(anyhow::Error::from)?;
        let target = text
            .trim()
            .strip_prefix("gitdir: ")
            .ok_or_else(|| denied("invalid worktree Git marker"))?;
        safe_path(&checkout.join(target))?
    };
    let common = if git_dir.join("commondir").exists() {
        let marker = safe_path(&git_dir.join("commondir"))?;
        let text = fs::read_to_string(marker).map_err(anyhow::Error::from)?;
        safe_path(&git_dir.join(text.trim()))?
    } else {
        git_dir.clone()
    };
    let main = if common == git_dir {
        checkout.to_path_buf()
    } else {
        if common.file_name().is_none_or(|s| s != ".git") {
            return Err(denied(
                "a linked worktree requires its stored main checkout",
            ));
        }
        common
            .parent()
            .ok_or_else(|| denied("main checkout is missing"))?
            .to_path_buf()
    };
    Ok(Some(Repository {
        git_dir,
        main: safe_path(&main)?,
        common,
    }))
}

pub fn main_checkout(worktree: &Path) -> Result<PathBuf, ShepherdError> {
    repository(worktree)?
        .map(|r| r.main)
        .ok_or_else(|| ShepherdError::Usage("not a Git checkout".into()))
}

fn matches_snapshot(
    data: &StoreData,
    trust: &ProjectTrust,
    path: &Path,
    common: Option<&Path>,
) -> bool {
    data.project_root == trust.project_root
        && (trust.project_root == path
            || common == Some(trust.common_git_dir.as_path())
            || data.tickets.iter().any(|t| {
                t.worktree.as_ref().is_some_and(|p| p == path)
                    || t.children.iter().any(|c| c.path == path)
            }))
}
fn find_snapshot(state: &Path, project: &Path) -> Result<Option<ProjectTrust>, ShepherdError> {
    let path = safe_path(project)?;
    let direct = Store::open(state, &path)?.read_only()?;
    if let Some(trust) = direct.trust
        && trust.project_root == path
    {
        return Ok(Some(trust));
    }
    let projects = match fs::read_dir(state.join("projects")) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(anyhow::Error::from(e).into()),
    };
    let mut headers = Vec::new();
    for entry in projects.flatten() {
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let Ok(bytes) = fs::read(entry.path().join("state.json")) else {
            continue;
        };
        let Ok(header) = serde_json::from_slice::<StoreData>(&bytes) else {
            continue;
        };
        if entry.file_name() != project_hash(&header.project_root).as_str() {
            continue;
        }
        if let Some(trust) = &header.trust
            && matches_snapshot(&header, trust, &path, None)
        {
            let data = Store::open(state, &header.project_root)?.read_only()?;
            if let Some(trust) = &data.trust
                && matches_snapshot(&data, trust, &path, None)
            {
                return Ok(Some(trust.clone()));
            }
        }
        headers.push(header);
    }
    // Only an unregistered worktree needs filesystem metadata. Registered
    // worktrees retain their own project's snapshot even if .git is changed.
    let Some(repo) = repository(&path)? else {
        return Ok(None);
    };
    let own = Store::open(state, &repo.main)?.read_only()?;
    if let Some(trust) = own.trust
        && trust.common_git_dir == repo.common
    {
        return Ok(Some(trust));
    }
    for header in headers {
        let Some(trust) = &header.trust else {
            continue;
        };
        if trust.common_git_dir != repo.common || header.project_root != trust.project_root {
            continue;
        }
        let data = Store::open(state, &header.project_root)?.read_only()?;
        if let Some(trust) = data.trust
            && trust.common_git_dir == repo.common
        {
            return Ok(Some(trust));
        }
    }
    Ok(None)
}

pub fn load_snapshot(state: &Path, project: &Path) -> Result<ProjectTrust, ShepherdError> {
    find_snapshot(state, project)?
        .ok_or_else(|| denied("project has no trust snapshot; run start or config trust"))
}

enum LocalConfig {
    Missing,
    Bytes(Vec<u8>),
    Unavailable,
}

impl LocalConfig {
    fn matches(&self, text: Option<&str>) -> bool {
        match (self, text) {
            (Self::Missing, None) => true,
            (Self::Bytes(bytes), Some(text)) => bytes == text.as_bytes(),
            _ => false,
        }
    }
}

/// Local bytes are used only for comparison, never as a source of trust.
fn local_config(project: &Path) -> LocalConfig {
    let path = project.join(".shepherd.toml");
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => return LocalConfig::Unavailable,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return LocalConfig::Missing;
        }
        Err(_) => return LocalConfig::Unavailable,
    }
    // Prevent a replacement symlink from being followed between metadata and
    // open, and avoid blocking if the file is replaced by a FIFO.
    let Ok(mut file) = fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(path)
    else {
        return LocalConfig::Unavailable;
    };
    if !file.metadata().is_ok_and(|metadata| metadata.is_file()) {
        return LocalConfig::Unavailable;
    }
    let mut bytes = Vec::new();
    match file.read_to_end(&mut bytes) {
        Ok(_) => LocalConfig::Bytes(bytes),
        Err(_) => LocalConfig::Unavailable,
    }
}

/// Compare the selected checkout to existing runtime trust without creating state.
pub fn config_drift_warning(state: &Path, project: &Path) -> Result<Option<String>, ShepherdError> {
    let Some(snapshot) = find_snapshot(state, project)? else {
        return Ok(None);
    };
    let local = local_config(&safe_path(project)?);
    if local.matches(snapshot.config_text.as_deref()) {
        return Ok(None);
    }
    let reason = match local {
        LocalConfig::Missing => "On-disk .shepherd.toml is missing",
        LocalConfig::Bytes(_) if snapshot.config_text.is_none() => {
            "On-disk .shepherd.toml exists, but the trust snapshot has no project config"
        }
        LocalConfig::Bytes(_) => "On-disk .shepherd.toml differs from the trust snapshot",
        LocalConfig::Unavailable => {
            "On-disk .shepherd.toml is symlinked, unreadable, or not a regular file"
        }
    };
    Ok(Some(format!(
        "{reason}. Runtime commands use the stored trust snapshot; commit the desired configuration (or its removal) on default branch `{}`, then run `config trust`.",
        snapshot.default_branch
    )))
}

fn isolated_git(cwd: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1");
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("GIT_")
            && !matches!(
                name.to_str(),
                Some("GIT_CONFIG_GLOBAL" | "GIT_CONFIG_SYSTEM" | "GIT_CONFIG_NOSYSTEM")
            )
        {
            command.env_remove(name);
        }
    }
    command.env("GIT_DIR", cwd.join("trust-probe.git"));
    command
}
fn output(cwd: &Path, args: &[&str]) -> Result<Option<String>, ShepherdError> {
    let out = proc::capture(isolated_git(cwd).args(args), Duration::from_secs(20), None)?;
    Ok(out.status.success().then_some(out.stdout))
}
fn config_values(cwd: &Path, common: &Path, key: &str) -> Result<Vec<String>, ShepherdError> {
    let file = safe_path(&common.join("config"))?;
    if !file.starts_with(common) {
        return Err(denied("repository config escapes its common directory"));
    }
    let out = proc::capture(
        isolated_git(cwd)
            .args(["config", "--file"])
            .arg(file)
            .args(["--no-includes", "--get-all", key]),
        Duration::from_secs(20),
        None,
    )?;
    if out.status.code() == Some(1) {
        return Ok(vec![]);
    }
    if !out.status.success() {
        return Err(denied(
            "cannot read initial repository settings without includes",
        ));
    }
    Ok(out.stdout.lines().map(str::to_owned).collect())
}
fn valid_oid(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|b| b.is_ascii_hexdigit())
}
fn read_ref(common: &Path, name: &str, depth: usize) -> Result<Option<String>, ShepherdError> {
    if depth > 8
        || !name.starts_with("refs/")
        || name.contains(['\\', '\n', '\r', '\0'])
        || name
            .split('/')
            .any(|s| s.is_empty() || s == "." || s == "..")
    {
        return Err(denied("invalid initial Git reference"));
    }
    let file = common.join(name);
    if file.exists() {
        let actual = safe_path(&file)?;
        if !actual.starts_with(common) {
            return Err(denied("Git reference escapes its common directory"));
        }
        let text = fs::read_to_string(actual).map_err(anyhow::Error::from)?;
        let text = text.trim();
        if let Some(target) = text.strip_prefix("ref: ") {
            return read_ref(common, target, depth + 1);
        }
        if !valid_oid(text) {
            return Err(denied("invalid initial Git object id"));
        }
        return Ok(Some(text.into()));
    }
    let packed = common.join("packed-refs");
    if packed.exists() {
        let actual = safe_path(&packed)?;
        if !actual.starts_with(common) {
            return Err(denied("packed references escape the repository"));
        }
        for line in fs::read_to_string(actual)
            .map_err(anyhow::Error::from)?
            .lines()
        {
            if let Some((oid, reference)) = line.split_once(' ')
                && reference == name
            {
                if !valid_oid(oid) {
                    return Err(denied("invalid packed Git object id"));
                }
                return Ok(Some(oid.into()));
            }
        }
    }
    Ok(None)
}
fn validate_url(url: &str) -> Result<(), ShepherdError> {
    if url.is_empty()
        || url.starts_with('-')
        || url.contains(['\r', '\n', '\0'])
        || url.starts_with("ext::")
    {
        return Err(denied("origin URL is invalid"));
    }
    if let Some((scheme, rest)) = url.split_once("://") {
        let authority = rest.split('/').next().unwrap_or_default();
        if matches!(scheme, "http" | "https") && authority.contains('@')
            || authority
                .split_once('@')
                .is_some_and(|(user, _)| user.contains(':'))
        {
            return Err(denied("credential-bearing origin URLs cannot be trusted"));
        }
    }
    Ok(())
}
fn github_origin(origin: &str) -> bool {
    origin.starts_with("git@github.com:")
        || origin.starts_with("ssh://git@github.com/")
        || origin.starts_with("https://github.com/")
        || origin.starts_with("http://github.com/")
}
fn main_head(common: &Path) -> Result<(Option<String>, Option<String>), ShepherdError> {
    let actual = safe_path(&common.join("HEAD"))?;
    if !actual.starts_with(common) {
        return Err(denied("main HEAD escapes its common directory"));
    }
    let text = fs::read_to_string(actual).map_err(anyhow::Error::from)?;
    if let Some(reference) = text.trim().strip_prefix("ref: ") {
        return Ok((
            reference.strip_prefix("refs/heads/").map(str::to_owned),
            read_ref(common, reference, 0)?,
        ));
    }
    Ok((None, valid_oid(text.trim()).then(|| text.trim().into())))
}
/// Observe verification SHAs without creating or changing project trust. These
/// are task inputs, never authorization for a push or configuration change.
/// Filesystem ref reads preserve legacy direct-spawn support and cannot execute
/// checkout hooks, helpers, or Git configuration supplied by an earlier agent.
pub fn verification_heads(
    worktree: &Path,
    branch: &str,
    base: &str,
) -> Result<(String, String), ShepherdError> {
    let repo =
        repository(worktree)?.ok_or_else(|| denied("verification requires a Git checkout"))?;
    if repo.git_dir != repo.common && !repo.git_dir.starts_with(repo.common.join("worktrees")) {
        return Err(denied("verification worktree metadata left the repository"));
    }
    let head_path = safe_path(&repo.git_dir.join("HEAD"))?;
    if !head_path.starts_with(&repo.git_dir) {
        return Err(denied("verification HEAD escapes its Git directory"));
    }
    let head_text = fs::read_to_string(&head_path).map_err(anyhow::Error::from)?;
    let reference = format!("refs/heads/{branch}");
    if head_text.trim() != format!("ref: {reference}") {
        return Err(denied(
            "verification checkout differs from its assigned branch",
        ));
    }
    let head = read_ref(&repo.common, &reference, 0)?
        .ok_or_else(|| denied("verification HEAD is missing"))?;
    let base = read_ref(&repo.common, &format!("refs/remotes/origin/{base}"), 0)?
        .or(read_ref(&repo.common, &format!("refs/heads/{base}"), 0)?)
        .ok_or_else(|| denied("verification base is missing"))?;
    if fs::read_to_string(head_path).map_err(anyhow::Error::from)? != head_text
        || read_ref(&repo.common, &reference, 0)?.as_ref() != Some(&head)
    {
        return Err(denied("verification HEAD changed while pinning the task"));
    }
    Ok((base, head))
}

fn global_config() -> Result<Option<PathBuf>, ShepherdError> {
    let path = std::env::var_os("GIT_CONFIG_GLOBAL")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".gitconfig")));
    path.filter(|p| p.exists())
        .map(|p| safe_path(&p))
        .transpose()
}
fn reader(store: &Store, common: &Path, object_format: &str) -> Result<PathBuf, ShepherdError> {
    let root = store.root().join("trust-objects.git");
    fs::create_dir_all(root.join("objects/info")).map_err(anyhow::Error::from)?;
    let config = if object_format == "sha256" {
        "[core]\nbare = true\nrepositoryformatversion = 1\n[extensions]\nobjectformat = sha256\n"
    } else {
        "[core]\nbare = true\nrepositoryformatversion = 0\n"
    };
    fs::write(root.join("config"), config).map_err(anyhow::Error::from)?;
    fs::write(root.join("HEAD"), "ref: refs/heads/trust-reader\n").map_err(anyhow::Error::from)?;
    fs::create_dir_all(root.join("refs")).map_err(anyhow::Error::from)?;
    let objects = safe_path(&common.join("objects"))?;
    if objects.as_os_str().to_string_lossy().contains(['\n', '\r']) {
        return Err(denied("object directory contains a newline"));
    }
    fs::write(
        root.join("objects/info/alternates"),
        format!("{}\n", objects.display()),
    )
    .map_err(anyhow::Error::from)?;
    Ok(root)
}
fn object_output(
    cwd: &Path,
    reader: &Path,
    args: &[&str],
) -> Result<Option<String>, ShepherdError> {
    let out = proc::capture(
        isolated_git(cwd).arg("--git-dir").arg(reader).args(args),
        Duration::from_secs(20),
        None,
    )?;
    Ok(out.status.success().then_some(out.stdout))
}
fn capture(
    state: &Path,
    project: &Path,
    gh: Option<&Path>,
    initial_text: Option<&str>,
) -> Result<ProjectTrust, ShepherdError> {
    let repo = repository(project)?
        .ok_or_else(|| ShepherdError::Usage("trust requires a Git checkout".into()))?;
    let store = Store::open(state, &repo.main)?;
    fs::create_dir_all(store.root()).map_err(anyhow::Error::from)?;
    // Force Git setup through our own empty repository even when the user
    // deliberately places the state directory inside their project checkout.
    let probe = store.root().join("trust-probe.git");
    fs::create_dir_all(probe.join("objects")).map_err(anyhow::Error::from)?;
    fs::create_dir_all(probe.join("refs")).map_err(anyhow::Error::from)?;
    fs::write(probe.join("HEAD"), "ref: refs/heads/trust-probe\n").map_err(anyhow::Error::from)?;
    fs::write(
        probe.join("config"),
        "[core]\nbare = true\nrepositoryformatversion = 0\n",
    )
    .map_err(anyhow::Error::from)?;
    let fetch = config_values(store.root(), &repo.common, "remote.origin.url")?;
    let pushes = config_values(store.root(), &repo.common, "remote.origin.pushurl")?;
    if fetch.len() > 1 || pushes.len() > 1 {
        return Err(denied("origin must have a single fetch and push URL"));
    }
    let origin_url = fetch.first().cloned().unwrap_or_default();
    let push_url = pushes
        .first()
        .cloned()
        .unwrap_or_else(|| origin_url.clone());
    // Repositories without an origin can still use local orchestration; every
    // transport operation fails closed on the empty stored URL.
    if !origin_url.is_empty() {
        validate_url(&origin_url)?;
    }
    if !push_url.is_empty() {
        validate_url(&push_url)?;
    }
    let formats = config_values(store.root(), &repo.common, "extensions.objectformat")?;
    let object_format = formats.first().map(String::as_str).unwrap_or("sha1");
    if !matches!(object_format, "sha1" | "sha256") {
        return Err(denied("unsupported Git object format"));
    }
    let (head_branch, head_oid) = main_head(&repo.common)?;
    let provider = gh.or_else(|| github_origin(&origin_url).then_some(Path::new("gh")));
    let mut default_branch = None;
    if let Some(gh) = provider {
        let result = proc::capture(
            Command::new(gh)
                .env("GIT_DIR", &probe)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env_remove("GIT_CONFIG_PARAMETERS")
                .env_remove("GIT_CONFIG_COUNT")
                .current_dir(store.root())
                .env_remove("GH_REPO")
                .env_remove("GH_HOST")
                .args(["repo", "view", &origin_url, "--json", "defaultBranchRef"]),
            Duration::from_secs(20),
            None,
        );
        if let Ok(out) = result
            && out.status.success()
            && let Ok(value) = serde_json::from_str::<serde_json::Value>(&out.stdout)
        {
            default_branch = value
                .pointer("/defaultBranchRef/name")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned);
        }
    }
    let default_branch = default_branch
        .or(head_branch)
        .ok_or_else(|| denied("cannot snapshot a default branch from detached MAIN"))?;
    if output(
        store.root(),
        &["check-ref-format", "--branch", &default_branch],
    )?
    .is_none()
    {
        return Err(denied("invalid initial default branch"));
    }
    let commit = read_ref(&repo.common, &format!("refs/heads/{default_branch}"), 0)?
        .or(read_ref(
            &repo.common,
            &format!("refs/remotes/origin/{default_branch}"),
            0,
        )?)
        .or(head_oid);
    let reader = reader(&store, &repo.common, object_format)?;
    let mut config_blob = None;
    let mut config_text = None;
    if let Some(oid) = &commit {
        let entry = object_output(
            store.root(),
            &reader,
            &["ls-tree", oid, "--", ".shepherd.toml"],
        )?
        .ok_or_else(|| denied("initial base commit is missing"))?;
        if !entry.trim().is_empty() {
            let (metadata, _) = entry
                .trim_end()
                .split_once('\t')
                .ok_or_else(|| denied("invalid project config tree entry"))?;
            let fields: Vec<_> = metadata.split_whitespace().collect();
            if fields.len() != 3 || !matches!(fields[0], "100644" | "100755") || fields[1] != "blob"
            {
                return Err(denied("trusted project config must be a regular file"));
            }
            let text = object_output(store.root(), &reader, &["cat-file", "blob", fields[2]])?
                .ok_or_else(|| denied("initial project config blob is missing"))?;
            config_blob = Some(fields[2].into());
            config_text = Some(text);
        }
    }
    if let Some(text) = initial_text {
        let out = proc::capture(
            isolated_git(store.root())
                .arg("--git-dir")
                .arg(&reader)
                .args(["hash-object", "--stdin"]),
            Duration::from_secs(20),
            Some(text.as_bytes()),
        )?;
        if !out.status.success() {
            return Err(denied("cannot hash initialized project config"));
        }
        config_blob = Some(out.stdout.trim().into());
        config_text = Some(text.into());
    }
    // Validate the selected bytes before retaining them; no hooks are executed.
    if let Some(text) = &config_text {
        text.parse::<toml::Table>()
            .map_err(|e| ShepherdError::Config {
                path: repo.main.join(".shepherd.toml"),
                message: e.to_string(),
            })?;
    }
    Ok(ProjectTrust {
        project_root: repo.main,
        common_git_dir: repo.common,
        origin_url,
        push_url,
        default_branch,
        config_commit: commit,
        config_blob,
        config_text,
        trusted_at: crate::launch::now(),
        global_config: global_config()?,
        object_format: object_format.into(),
    })
}
fn trust_lock(store: &Store) -> Result<fs::File, ShepherdError> {
    fs::create_dir_all(store.root()).map_err(anyhow::Error::from)?;
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(store.root().join("trust.lock"))
        .map_err(anyhow::Error::from)?;
    file.lock().map_err(anyhow::Error::from)?;
    Ok(file)
}
fn ensure_using(
    state: &Path,
    project: &Path,
    gh: Option<&Path>,
    initial_text: Option<&str>,
) -> Result<ProjectTrust, ShepherdError> {
    if let Some(trust) = find_snapshot(state, project)? {
        return Ok(trust);
    }
    let repo = repository(project)?
        .ok_or_else(|| ShepherdError::Usage("trust requires a Git checkout".into()))?;
    let store = Store::open(state, &repo.main)?;
    let _lock = trust_lock(&store)?;
    if let Some(trust) = store.read()?.trust {
        return Ok(trust);
    }
    let trust = capture(state, &repo.main, gh, initial_text)?;
    store.update(|data| {
        data.trust = Some(trust.clone());
        Ok(())
    })?;
    Ok(trust)
}
pub fn ensure_snapshot(state: &Path, project: &Path) -> Result<ProjectTrust, ShepherdError> {
    ensure_using(state, project, None, None)
}
pub fn snapshot_initialized_config(
    state: &Path,
    project: &Path,
    text: &str,
) -> Result<ProjectTrust, ShepherdError> {
    ensure_using(state, project, None, Some(text))
}

#[derive(Clone, Debug, Serialize)]
pub struct TrustDiff {
    pub old: Option<ProjectTrust>,
    pub new: ProjectTrust,
    pub diff: String,
    pub warnings: Vec<String>,
}
pub fn refresh_snapshot(state: &Path, project: &Path) -> Result<TrustDiff, ShepherdError> {
    let old = find_snapshot(state, project)?;
    let main = match &old {
        Some(trust) => trust.project_root.clone(),
        None => main_checkout(project)?,
    };
    let store = Store::open(state, &main)?;
    let _lock = trust_lock(&store)?;
    let new = capture(state, &main, None, None)?;
    let selected = safe_path(project)?;
    let mut locals = vec![local_config(&selected)];
    if selected != main {
        locals.push(local_config(&main));
    }
    if new.config_text.is_none()
        && locals
            .iter()
            .any(|local| !matches!(local, LocalConfig::Missing))
    {
        return Err(denied(&format!(
            ".shepherd.toml exists on disk but is not committed on default branch `{}`; commit it on that branch, then run `config trust`. The stored trust snapshot was not changed.",
            new.default_branch
        )));
    }
    let warnings = if locals
        .iter()
        .any(|local| !local.matches(new.config_text.as_deref()))
    {
        vec![format!(
            "Local .shepherd.toml edits were not captured; config trust uses only committed bytes from default branch `{}`. Commit the desired configuration (or its removal) on that branch, then run `config trust`.",
            new.default_branch
        )]
    } else {
        Vec::new()
    };
    let mut diff = String::from("--- stored/.shepherd.toml\n+++ main/.shepherd.toml\n");
    for line in old
        .as_ref()
        .and_then(|t| t.config_text.as_deref())
        .unwrap_or("")
        .lines()
    {
        diff.push_str(&format!("-{line}\n"));
    }
    for line in new.config_text.as_deref().unwrap_or("").lines() {
        diff.push_str(&format!("+{line}\n"));
    }
    if old
        .as_ref()
        .is_none_or(|t| t.default_branch != new.default_branch)
    {
        diff.push_str(&format!(
            "default branch: {} -> {}\n",
            old.as_ref()
                .map_or("<unset>", |t| t.default_branch.as_str()),
            new.default_branch
        ));
    }
    if old.as_ref().is_none_or(|t| t.origin_url != new.origin_url) {
        diff.push_str(&format!(
            "origin URL: {} -> {}\n",
            old.as_ref().map_or("<unset>", |t| t.origin_url.as_str()),
            new.origin_url
        ));
    }
    if old.as_ref().is_none_or(|t| t.push_url != new.push_url) {
        diff.push_str(&format!(
            "push URL: {} -> {}\n",
            old.as_ref().map_or("<unset>", |t| t.push_url.as_str()),
            new.push_url
        ));
    }
    store.update(|data| {
        data.trust = Some(new.clone());
        Ok(())
    })?;
    Ok(TrustDiff {
        old,
        new,
        diff,
        warnings,
    })
}

pub fn default_branch(worktree: &Path, state: &Path) -> Result<Option<String>, ShepherdError> {
    Ok(find_snapshot(state, worktree)?.map(|t| t.default_branch))
}
/// Explicit authorized bootstrap seam for provider fixtures and callers that
/// intentionally establish trust. Ordinary default_branch only reads state.
pub fn default_branch_using(
    worktree: &Path,
    state: &Path,
    gh: &Path,
) -> Result<Option<String>, ShepherdError> {
    Ok(Some(
        ensure_using(state, worktree, Some(gh), None)?.default_branch,
    ))
}

pub fn committed_config(
    config_dir: &Path,
    worktree: &Path,
    _base: Option<&str>,
    state: &Path,
) -> Result<Config, ShepherdError> {
    let snapshot = find_snapshot(state, worktree)?;
    let path = snapshot
        .as_ref()
        .map(|t| t.project_root.join(".shepherd.toml"))
        .unwrap_or_else(|| worktree.join(".shepherd.toml"));
    config::load_committed(
        config_dir,
        &path,
        snapshot.as_ref().and_then(|t| t.config_text.as_deref()),
    )
}
