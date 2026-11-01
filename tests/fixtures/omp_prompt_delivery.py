"""Synthetic PTY probe, invoked only inside the Rust test's network sandbox."""
import fcntl
import http.server
import json
import os
from pathlib import Path
import pty
import re
import select
import struct
import subprocess
import sys
import termios
import threading
import time

ROOT = Path(sys.argv[1])
OMP = sys.argv[2]
MANAGED_ARGS = json.loads(Path(sys.argv[3]).read_text())
TASK = Path(sys.argv[4]).read_bytes()
requests = []


class Provider(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_GET(self):
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(b'{"object":"list","data":[]}')

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", 0))))
        requests.append({"path": self.path, "body": body})
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()
        for delta, finish in [({"role": "assistant", "content": "fixture acknowledged"}, None), ({}, "stop")]:
            chunk = {
                "id": "fixture", "object": "chat.completion.chunk", "created": 1,
                "model": "probe", "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
            }
            self.wfile.write(("data: " + json.dumps(chunk) + "\n\n").encode())
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()


server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Provider)
threading.Thread(target=server.serve_forever, daemon=True).start()
port = server.server_address[1]


def run(name, phases):
    base = ROOT / name
    base.mkdir()
    for directory in ["home", "config", "data", "cache", "agent", "project"]:
        (base / directory).mkdir()
    (base / "agent/models.yml").write_text(json.dumps({"providers": {"fixture": {
        "baseUrl": f"http://127.0.0.1:{port}/v1", "api": "openai-completions",
        "apiKey": "synthetic-fixture", "models": [{
            "id": "probe", "name": "Probe", "reasoning": False, "input": ["text"],
            "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
            "contextWindow": 128000, "maxTokens": 128,
        }],
    }}}))
    (base / "agent/config.yml").write_text(json.dumps({"startup": {
        "quiet": True, "setupWizard": False, "showSplash": False, "checkUpdate": False,
    }}))
    env = {
        "PATH": "/usr/bin:/bin", "HOME": str(base / "home"),
        "XDG_CONFIG_HOME": str(base / "config"), "XDG_DATA_HOME": str(base / "data"),
        "XDG_CACHE_HOME": str(base / "cache"), "PI_CODING_AGENT_DIR": str(base / "agent"),
        "TERM": "xterm-256color", "LANG": "C.UTF-8", "NO_COLOR": "1",
        "HERDR_SOCKET_PATH": str(base / "nonexistent-herdr.sock"),
    }
    version = subprocess.run([OMP, "--version"], cwd=base / "project", env=env,
                             capture_output=True, check=True, timeout=10).stdout.decode().strip()
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 140, 0, 0))
    proc = subprocess.Popen([OMP, *MANAGED_ARGS], cwd=base / "project", env=env,
                            stdin=slave, stdout=slave, stderr=slave, start_new_session=True)
    os.close(slave)
    out = bytearray()
    events = []
    before = len(requests)

    def pump(seconds, until=None):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            if until and until():
                return True
            readable, _, _ = select.select([master], [], [], min(.05, max(0, deadline - time.monotonic())))
            if not readable:
                continue
            try:
                chunk = os.read(master, 65536)
            except OSError:
                return False
            if not chunk:
                return False
            out.extend(chunk)
            # Only answer terminal capability queries from this disposable PTY.
            for query, reply in [(b"\x1b[6n", b"\x1b[1;1R"), (b"\x1b[c", b"\x1b[?1;2c"),
                                 (b"\x1b[?u", b"\x1b[?0u")]:
                for _ in range(chunk.count(query)):
                    os.write(master, reply)
        return bool(until and until())

    def observation(event, **extra):
        states = re.findall(rb"\x1b\]7501;state=(\w+)", out)
        return {
            "event": event, "requests": len(requests) - before,
            "working_osc": b"state=working:app=omp" in out,
            "last_state": states[-1].decode() if states else None, **extra,
        }

    try:
        ready = pump(40, lambda: b"change thinking effort" in out)
        events.append(observation("ready", ready=ready, exited=proc.poll()))
        for label, data, staged in phases:
            if not ready:
                break
            phase_start = len(out)
            written = 0
            while written < len(data):
                written += os.write(master, data[written:])
            if staged:
                observed = pump(3, lambda: any(marker in out[phase_start:]
                                               for marker in [b"Paste", b"attachment", b"#1"]))
            else:
                observed = pump(3, lambda: len(requests) > before)
            events.append(observation(label, hex=data.hex(), observed=observed))
        # Observe completion and any unexpected duplicate provider turn.
        pump(1)
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=3)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()
        os.close(master)
    (base / "terminal.bin").write_bytes(out)
    (base / "terminal.txt").write_text(re.sub(r"\x1b\[[0-?]*[ -/]*[@-~]", "", out.decode(errors="replace")))
    result = {"name": name, "version": version, "events": events, "requests": requests[before:]}
    (base / "result.json").write_text(json.dumps(result, indent=2))
    print(json.dumps({"name": name, "version": version, "events": [
        {key: value for key, value in event.items() if key != "hex"} for event in events
    ], "requests": len(result["requests"])}), flush=True)
    return result


results = []
try:
    # One submission key per fresh fixture terminal. Never repair a failed case.
    for name, key, split in [("split-kitty", b"\x1b[13u", True),
                              ("combined-kitty-event", b"\x1b[13;1:1u", False)]:
        paste = b"\x1b[200~" + TASK + b"\x1b[201~"
        phases = [("paste", paste, True), ("enter", key, False)] if split else [("combined", paste + key, False)]
        results.append(run(name, phases))
finally:
    server.shutdown()
(ROOT / "summary.json").write_text(json.dumps(results, indent=2))
for result in results:
    assert result["events"][0]["ready"], result["name"]
    assert len(result["requests"]) == 1, (result["name"], len(result["requests"]))
    body = result["requests"][0]["body"]
    assert body["model"] == "probe", body
    assert "Synthetic terminal transport fixture." in json.dumps(body)
    users = [message for message in body["messages"] if message["role"] == "user"]
    assert len(users) == 1, users
    content = users[0]["content"]
    if isinstance(content, list):
        content = "\n".join(part.get("text", "") for part in content)
    assert TASK.decode().strip() in content, "full generated flock task was not delivered"
