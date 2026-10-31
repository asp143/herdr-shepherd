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
