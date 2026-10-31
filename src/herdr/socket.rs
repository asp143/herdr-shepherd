use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::Path,
    time::Duration,
};

use serde_json::Value;

use crate::error::ShepherdError;

pub(super) fn connect(path: &Path) -> Result<UnixStream, ShepherdError> {
    UnixStream::connect(path).map_err(|err| {
        ShepherdError::HerdrUnavailable(format!("herdr unavailable at {}: {err}", path.display()))
    })
}

pub fn request(path: &Path, value: &Value) -> Result<Value, ShepherdError> {
    request_with_timeout(path, value, Duration::from_secs(20))
}

fn request_with_timeout(
    path: &Path,
    value: &Value,
    timeout: Duration,
) -> Result<Value, ShepherdError> {
    let mut stream = connect(path)?;
    let timeout = Some(timeout);
    stream.set_read_timeout(timeout).map_err(runtime)?;
    stream.set_write_timeout(timeout).map_err(runtime)?;
    serde_json::to_writer(&mut stream, value).map_err(|e| ShepherdError::Runtime(e.into()))?;
    stream.write_all(b"\n").map_err(runtime)?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).map_err(|err| {
        if matches!(
            err.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ) {
            runtime(err)
        } else {
            ShepherdError::HerdrUnavailable(format!(
                "herdr unavailable at {}: {err}",
                path.display()
            ))
        }
    })?;
    if !line.ends_with('\n') {
        return Err(ShepherdError::HerdrUnavailable(format!(
            "herdr unavailable at {}: incomplete socket response",
            path.display()
        )));
    }
    let response: Value =
        serde_json::from_str(&line).map_err(|err| ShepherdError::Runtime(err.into()))?;
    if let Some(error) = response.get("error").filter(|error| !error.is_null()) {
        return Err(ShepherdError::Runtime(anyhow::anyhow!(
            "herdr socket error: {error}"
        )));
    }
    response.get("result").cloned().ok_or_else(|| {
        ShepherdError::Runtime(anyhow::anyhow!("herdr socket response has no result"))
    })
}

fn runtime(error: std::io::Error) -> ShepherdError {
    ShepherdError::Runtime(error.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{
        io::{BufRead, BufReader, Write},
        os::unix::net::UnixListener,
        thread,
    };

    #[test]
    fn move_request_uses_newline_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("herdr.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(&conn).read_line(&mut line).unwrap();
            let request: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(
                request,
                json!({"id":"shepherd-move","method":"workspace.move_block","params":{"workspace_ids":["w3"],"before_workspace_id":"w2"}})
            );
            conn.write_all(b"{\"id\":\"shepherd-move\",\"result\":{}}\n")
                .unwrap();
        });
        request(&path, &json!({"id":"shepherd-move","method":"workspace.move_block","params":{"workspace_ids":["w3"],"before_workspace_id":"w2"}})).unwrap();
        server.join().unwrap();
    }

    #[test]
    fn socket_error_is_runtime_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("herdr.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(&conn).read_line(&mut line).unwrap();
            conn.write_all(b"{\"id\":\"test\",\"error\":{\"message\":\"bad workspace\"}}\n")
                .unwrap();
        });
        assert!(matches!(
            request(&path, &json!({"id":"test"})),
            Err(ShepherdError::Runtime(_))
        ));
        server.join().unwrap();
    }

    #[test]
    fn request_returns_parsed_result() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("herdr.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(&conn).read_line(&mut line).unwrap();
            conn.write_all(b"{\"id\":\"test\",\"result\":{\"value\":42}}\n")
                .unwrap();
        });
        assert_eq!(
            request(&path, &json!({"id":"test"})).unwrap(),
            json!({"value":42})
        );
        server.join().unwrap();
    }

    #[test]
    fn stalled_socket_is_runtime_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("herdr.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = thread::spawn(move || {
            let (conn, _) = listener.accept().unwrap();
            thread::sleep(Duration::from_millis(100));
            drop(conn);
        });
        let result = request_with_timeout(&path, &json!({"id":"test"}), Duration::from_millis(20));
        assert!(
            matches!(result, Err(ShepherdError::Runtime(_))),
            "{result:?}"
        );
        server.join().unwrap();
    }

    #[test]
    fn socket_eof_is_herdr_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("herdr.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = thread::spawn(move || {
            let (conn, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(&conn).read_line(&mut line).unwrap();
            drop(conn);
        });
        assert!(matches!(
            request(&path, &json!({"id":"test"})),
            Err(ShepherdError::HerdrUnavailable(_))
        ));
        server.join().unwrap();
    }

    #[test]
    fn missing_socket_reports_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.sock");
        let err = request(&path, &json!({})).unwrap_err();
        assert!(matches!(err, ShepherdError::HerdrUnavailable(_)));
        assert!(err.to_string().contains(path.to_str().unwrap()));
    }
}
