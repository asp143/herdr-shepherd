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

pub(super) fn request(path: &Path, value: &Value) -> Result<(), ShepherdError> {
    let mut stream = connect(path)?;
    let timeout = Some(Duration::from_secs(20));
    stream.set_read_timeout(timeout).map_err(runtime)?;
    stream.set_write_timeout(timeout).map_err(runtime)?;
    serde_json::to_writer(&mut stream, value).map_err(|e| ShepherdError::Runtime(e.into()))?;
    stream.write_all(b"\n").map_err(runtime)?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).map_err(|err| {
        ShepherdError::HerdrUnavailable(format!("herdr unavailable at {}: {err}", path.display()))
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
    Ok(())
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
    fn missing_socket_reports_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.sock");
        let err = request(&path, &json!({})).unwrap_err();
        assert!(matches!(err, ShepherdError::HerdrUnavailable(_)));
        assert!(err.to_string().contains(path.to_str().unwrap()));
    }
}
