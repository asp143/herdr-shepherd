use std::{
    io::Read,
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

/// Capture stdout while enforcing a deadline, including pipe reads.
pub fn run(cmd: &mut Command, timeout: Duration) -> anyhow::Result<Option<String>> {
    let deadline = Instant::now() + timeout;
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let stdout = child.stdout.take().expect("stdout is piped");
    let (tx, rx) = mpsc::channel();
    // Drain concurrently so a full OS pipe cannot block the child before exit.
    // A descendant can retain stdout after the child dies, so do not join this
    // reader on timeout; the deadline must also cover that case.
    thread::spawn(move || {
        let mut out = String::new();
        let result = std::io::BufReader::new(stdout).read_to_string(&mut out);
        let _ = tx.send(result.map(|_| out));
    });
    loop {
        if let Some(status) = child.try_wait()? {
            if !status.success() {
                return Ok(None);
            }
            return match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(result) => Ok(Some(result?)),
                Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    anyhow::bail!("stdout reader disconnected")
                }
            };
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            child.wait()?;
            return Ok(None);
        }
        thread::sleep(Duration::from_millis(5));
    }
}

/// Captured subprocess result. Both pipes and optional stdin share the deadline.
pub struct Captured {
    pub status: std::process::ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

pub fn capture(
    cmd: &mut Command,
    timeout: Duration,
    input: Option<&[u8]>,
) -> anyhow::Result<Captured> {
    use std::io::Write;
    let deadline = Instant::now() + timeout;
    let mut child = cmd
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let (tx, rx) = mpsc::channel();
    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");
    for (kind, pipe) in [
        (true, Box::new(stdout) as Box<dyn Read + Send>),
        (false, Box::new(stderr)),
    ] {
        let tx = tx.clone();
        thread::spawn(move || {
            let mut text = String::new();
            let result = std::io::BufReader::new(pipe)
                .read_to_string(&mut text)
                .map(|_| text);
            let _ = tx.send((kind, result));
        });
    }
    drop(tx);
    if let Some(input) = input {
        let data = input.to_vec();
        let mut stdin = child.stdin.take().expect("stdin piped");
        thread::spawn(move || {
            let _ = stdin.write_all(&data);
        });
    }
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("command timed out");
        }
        thread::sleep(Duration::from_millis(5));
    };
    let mut stdout = String::new();
    let mut stderr = String::new();
    for _ in 0..2 {
        let (is_stdout, result) = rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|_| anyhow::anyhow!("command pipe timed out"))?;
        if is_stdout {
            stdout = result?;
        } else {
            stderr = result?;
        }
    }
    Ok(Captured {
        status,
        stdout,
        stderr,
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
}
