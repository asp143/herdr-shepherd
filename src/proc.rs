use std::{
    io::{Read, Write},
    os::unix::process::CommandExt,
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

/// A process or one of its inherited pipes outlived the shared deadline.
#[derive(Debug)]
pub struct DeadlineExceeded;

impl std::fmt::Display for DeadlineExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("subprocess deadline exceeded")
    }
}
impl std::error::Error for DeadlineExceeded {}

/// Preserve the Phase 1 stdout-only API and its nonzero/timeout fallback.
pub fn run(cmd: &mut Command, timeout: Duration) -> anyhow::Result<Option<String>> {
    match capture(cmd, timeout, None) {
        Ok(result) => Ok(result.status.success().then_some(result.stdout)),
        Err(error) if error.is::<DeadlineExceeded>() => Ok(None),
        Err(error) => Err(error),
    }
}

/// Captured subprocess result. Both pipes and optional stdin share the deadline.
pub struct Captured {
    pub status: std::process::ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Clone, Copy)]
enum Pipe {
    Stdout,
    Stderr,
    Stdin,
}

fn stop(child: &mut Child) {
    // Descendants can retain a pipe after the immediate child exits. Each
    // capture owns a process group, so timeout cleanup reaches those too.
    let _ = Command::new("/bin/kill")
        .args(["-KILL", "--", &format!("-{}", child.id())])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = child.kill();
    let _ = child.wait();
}

pub fn capture(
    cmd: &mut Command,
    timeout: Duration,
    input: Option<&[u8]>,
) -> anyhow::Result<Captured> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| anyhow::anyhow!("subprocess timeout is too large"))?;
    let mut child = cmd
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()?;
    let (tx, rx) = mpsc::channel();
    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");
    for (kind, pipe) in [
        (Pipe::Stdout, Box::new(stdout) as Box<dyn Read + Send>),
        (Pipe::Stderr, Box::new(stderr)),
    ] {
        let tx = tx.clone();
        thread::spawn(move || {
            let mut bytes = Vec::new();
            let result = std::io::BufReader::new(pipe)
                .read_to_end(&mut bytes)
                .map(|_| bytes);
            let _ = tx.send((kind, result));
        });
    }
    if let Some(input) = input {
        let data = input.to_vec();
        let mut stdin = child.stdin.take().expect("stdin piped");
        let tx = tx.clone();
        thread::spawn(move || {
            let result = stdin.write_all(&data).map(|_| Vec::new());
            drop(stdin);
            let _ = tx.send((Pipe::Stdin, result));
        });
    }
    drop(tx);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            stop(&mut child);
            return Err(DeadlineExceeded.into());
        }
        thread::sleep(Duration::from_millis(5));
    };
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    for _ in 0..(2 + usize::from(input.is_some())) {
        let (pipe, result) =
            match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(result) => result,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    stop(&mut child);
                    return Err(DeadlineExceeded.into());
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    anyhow::bail!("subprocess pipe reader disconnected")
                }
            };
        match pipe {
            Pipe::Stdout => stdout = result?,
            Pipe::Stderr => stderr = result?,
            // A command may exit early without consuming its complete input.
            // Preserve its status and diagnostic rather than mask a rejection.
            Pipe::Stdin => {
                if let Err(error) = result
                    && status.success()
                    && error.kind() != std::io::ErrorKind::BrokenPipe
                {
                    return Err(error.into());
                }
            }
        }
    }
    Ok(Captured {
        status,
        // The old API rejected invalid UTF-8 on successful stdout, but returned
        // None on every nonzero exit. Keep both contracts while retaining logs.
        stdout: if status.success() {
            String::from_utf8(stdout)?
        } else {
            String::from_utf8_lossy(&stdout).into_owned()
        },
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn captures_large_stdout_without_deadlock() {
        let out = run(
            Command::new("sh").args(["-c", "head -c 262144 /dev/zero | tr '\\0' x"]),
            Duration::from_secs(2),
        )
        .unwrap()
        .unwrap();
        assert_eq!(out.len(), 262_144);
    }

    #[test]
    fn returns_none_on_nonzero_exit() {
        assert_eq!(
            run(
                Command::new("sh").args(["-c", "exit 1"]),
                Duration::from_secs(1)
            )
            .unwrap(),
            None
        );
    }

    #[test]
    fn timeout_kills_child_without_waiting_for_descendant_pipe() {
        let started = Instant::now();
        assert_eq!(
            run(
                Command::new("sh").args(["-c", "sleep 1"]),
                Duration::from_millis(40)
            )
            .unwrap(),
            None
        );
        assert!(started.elapsed() < Duration::from_millis(500));
    }
    #[test]
    fn capture_preserves_nonzero_status_stderr_and_stdin() {
        let out = capture(
            Command::new("sh").args(["-c", "cat; printf diagnostic >&2; exit 7"]),
            Duration::from_secs(2),
            Some(b"literal input\nsecond line"),
        )
        .unwrap();
        assert_eq!(out.status.code(), Some(7));
        assert_eq!(out.stdout, "literal input\nsecond line");
        assert_eq!(out.stderr, "diagnostic");
    }

    #[test]
    fn capture_drains_large_stderr_without_deadlock() {
        let out = capture(
            Command::new("sh").args([
                "-c",
                "head -c 262144 /dev/zero | tr '\\0' e >&2; printf done",
            ]),
            Duration::from_secs(2),
            None,
        )
        .unwrap();
        assert!(out.status.success());
        assert_eq!(out.stdout, "done");
        assert_eq!(out.stderr, "e".repeat(262144));
    }

    #[test]
    fn capture_deadline_covers_nonreading_stdin() {
        let started = Instant::now();
        let result = capture(
            Command::new("sh").args(["-c", "exec sleep 1"]),
            Duration::from_millis(40),
            Some(&vec![b'x'; 1_048_576]),
        );
        let error = result.err().expect("nonreading process must time out");
        assert!(error.is::<DeadlineExceeded>(), "{error:#}");
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn capture_deadline_covers_descendant_pipe() {
        let started = Instant::now();
        let result = capture(
            Command::new("sh").args(["-c", "sleep 1 & exit 0"]),
            Duration::from_millis(40),
            None,
        );
        let error = result.err().expect("descendant-held pipes must time out");
        assert!(error.is::<DeadlineExceeded>(), "{error:#}");
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn run_preserves_spawn_errors() {
        let dir = tempfile::tempdir().unwrap();
        let error = run(
            &mut Command::new(dir.path().join("missing")),
            Duration::from_secs(1),
        )
        .unwrap_err();
        assert_eq!(
            error.downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::NotFound
        );
    }

    #[test]
    fn run_nonzero_binary_output_still_returns_none() {
        assert_eq!(
            run(
                Command::new("sh").args(["-c", "printf '\\377'; exit 3"]),
                Duration::from_secs(1)
            )
            .unwrap(),
            None
        );
    }

    #[test]
    fn capture_deadline_covers_stdin_held_after_child_exit() {
        let started = Instant::now();
        let result = capture(
            Command::new("sh").args(["-c", "sleep 1 <&0 >/dev/null 2>&1 & exit 0"]),
            Duration::from_millis(40),
            Some(&vec![b'x'; 1_048_576]),
        );
        assert!(result.is_err(), "stdin writer must complete before success");
        assert!(started.elapsed() < Duration::from_millis(500));
    }
}
