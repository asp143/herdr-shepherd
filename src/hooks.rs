use std::{
    cell::RefCell,
    io::Write,
    marker::PhantomData,
    os::unix::process::CommandExt,
    process::{Command, Stdio},
    rc::Rc,
    thread,
    time::{Duration, Instant},
};

use crate::config::Config;

struct PendingHook {
    hook: toml::Value,
    event: String,
    payload: String,
}

#[derive(Default)]
struct DeferredHooks {
    depth: usize,
    pending: Vec<PendingHook>,
}

thread_local! {
    static DEFERRED: RefCell<DeferredHooks> = RefCell::default();
}

/// Deliver hooks synchronously after the outermost ownership scope releases
/// its locks. Declare this guard before locks, which drop in reverse order.
/// Nested launch/placement/revision scopes must not flush each other's hooks.
pub fn defer() -> DeferredHookGuard {
    DEFERRED.with_borrow_mut(|deferred| deferred.depth += 1);
    DeferredHookGuard(PhantomData)
}

// The queue belongs to this thread; its scope must not move to another one.
#[must_use = "keep the guard alive until the ownership locks have been released"]
pub struct DeferredHookGuard(PhantomData<Rc<()>>);

impl Drop for DeferredHookGuard {
    fn drop(&mut self) {
        let pending = DEFERRED.with_borrow_mut(|deferred| {
            deferred.depth -= 1;
            if deferred.depth == 0 {
                std::mem::take(&mut deferred.pending)
            } else {
                Vec::new()
            }
        });
        for pending in pending {
            run(&pending.hook, &pending.event, &pending.payload);
        }
    }
}

pub fn known_event(event: &str) -> bool {
    crate::config::known_hook_event(event)
}

/// Run configured lifecycle hooks without making their success a prerequisite
/// for the operation that emitted the event.
pub fn emit(cfg: &Config, event: &str, payload: &serde_json::Value) {
    if !known_event(event) {
        eprintln!("shepherd: unknown lifecycle event: {event}");
        return;
    }
    let Some(hooks) = cfg.raw.get("hooks").and_then(toml::Value::as_array) else {
        return;
    };
    let payload = payload.to_string();
    for hook in hooks {
        if hook.get("on").and_then(toml::Value::as_str) != Some(event) {
            continue;
        }
        let deferred = DEFERRED.with_borrow_mut(|deferred| {
            if deferred.depth == 0 {
                return false;
            }
            deferred.pending.push(PendingHook {
                hook: hook.clone(),
                event: event.into(),
                payload: payload.clone(),
            });
            true
        });
        if !deferred {
            run(hook, event, &payload);
        }
    }
}

fn run(hook: &toml::Value, event: &str, payload: &str) {
    let result = (|| -> anyhow::Result<()> {
        let argv = hook
            .get("command")
            .and_then(toml::Value::as_array)
            .ok_or_else(|| anyhow::anyhow!("hook command must be an argv array"))?;
        let argv: Vec<_> = argv
            .iter()
            .map(|arg| {
                arg.as_str()
                    .ok_or_else(|| anyhow::anyhow!("hook argv must contain strings"))
            })
            .collect::<Result<_, _>>()?;
        let (program, args) = argv
            .split_first()
            .filter(|(program, _)| !program.is_empty())
            .ok_or_else(|| anyhow::anyhow!("hook command cannot be empty"))?;
        let timeout = match hook.get("timeout_seconds") {
            None => Duration::from_secs(10),
            Some(value) => Duration::from_secs(
                value
                    .as_integer()
                    .and_then(|value| u64::try_from(value).ok())
                    .filter(|value| *value > 0)
                    .ok_or_else(|| anyhow::anyhow!("hook timeout_seconds must be positive"))?,
            ),
        };
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| anyhow::anyhow!("hook timeout_seconds is too large"))?;
        let mut child = Command::new(program)
            .args(args)
            .env("SHEPHERD_EVENT", event)
            .env("SHEPHERD_EVENT_JSON", payload)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .process_group(0)
            .spawn()?;
        let mut stdin = child.stdin.take().expect("stdin is piped");
        let bytes = payload.as_bytes().to_vec();
        // Feed concurrently: hooks that do not read stdin must still time out
        // even when a lifecycle payload exceeds the pipe buffer.
        thread::spawn(move || {
            let _ = stdin.write_all(&bytes);
        });
        loop {
            if let Some(status) = child.try_wait()? {
                anyhow::ensure!(status.success(), "hook exited with {status}");
                return Ok(());
            }
            if Instant::now() >= deadline {
                // The hook gets its own process group so shell descendants
                // cannot outlive the timeout and continue side effects.
                let _ = Command::new("/bin/kill")
                    .args(["-KILL", "--", &format!("-{}", child.id())])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!("hook timed out after {}s", timeout.as_secs());
            }
            thread::sleep(Duration::from_millis(5));
        }
    })();
    if let Err(error) = result {
        eprintln!("shepherd: hook {event}: {error:#}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{
        fs,
        time::{Duration, Instant},
    };

    fn config(dir: &std::path::Path, event: &str, command: &[&str], timeout: u64) -> Config {
        let command = toml::Value::Array(
            command
                .iter()
                .map(|s| toml::Value::String((*s).into()))
                .collect(),
        );
        let text = format!(
            "[[hooks]]\non = {event:?}\ncommand = {command}\ntimeout_seconds = {timeout}\n"
        );
        fs::write(dir.join("shepherd.toml"), text).unwrap();
        crate::config::load(dir, None).unwrap()
    }

    #[test]
    fn nested_ownership_scopes_deliver_hooks_once_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("output");
        let cfg = config(
            dir.path(),
            "agent.spawned",
            &["sh", "-c", "cat >> \"$1\"", "hook", out.to_str().unwrap()],
            1,
        );
        {
            let _placement = defer();
            emit(&cfg, "agent.spawned", &json!({"agent":"first"}));
            {
                let _launch = defer();
                emit(&cfg, "agent.spawned", &json!({"agent":"second"}));
            }
            assert!(
                !out.exists(),
                "inner scope must not dispatch hooks under outer locks"
            );
        }
        assert_eq!(
            fs::read_to_string(out).unwrap(),
            r#"{"agent":"first"}{"agent":"second"}"#
        );
    }

    #[test]
    fn runs_matching_hook() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("output");
        let cfg = config(
            dir.path(),
            "ticket.cleaned_up",
            &[
                "sh",
                "-c",
                "printf '%s\\n%s\\n' \"$SHEPHERD_EVENT\" \"$SHEPHERD_EVENT_JSON\" > \"$1\"; cat >> \"$1\"",
                "hook",
                out.to_str().unwrap(),
            ],
            10,
        );
        let payload = json!({"branch": "feat/example", "pr": 12});
        emit(&cfg, "ticket.cleaned_up", &payload);
        let text = fs::read_to_string(out).unwrap();
        assert_eq!(text, format!("ticket.cleaned_up\n{payload}\n{payload}"));
    }

    #[test]
    fn skips_other_events() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("output");
        let cfg = config(
            dir.path(),
            "ticket.cleaned_up",
            &["sh", "-c", "touch \"$1\"", "hook", out.to_str().unwrap()],
            10,
        );
        emit(&cfg, "ticket.started", &json!({}));
        assert!(!out.exists());
    }

    #[test]
    fn timeout_kills_hook() {
        let dir = tempfile::tempdir().unwrap();
        let pid = dir.path().join("pid");
        let cfg = config(
            dir.path(),
            "ticket.started",
            &[
                "sh",
                "-c",
                "echo $$ > \"$1\"; exec sleep 30",
                "hook",
                pid.to_str().unwrap(),
            ],
            1,
        );
        let start = Instant::now();
        emit(&cfg, "ticket.started", &json!({}));
        assert!(start.elapsed() < Duration::from_secs(2));
        let pid = fs::read_to_string(pid).unwrap();
        assert!(!std::path::Path::new("/proc").join(pid.trim()).exists());
    }
}
