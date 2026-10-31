#!/usr/bin/env bash
# Manual only: real default-profile agents, isolated Herdr, no GitHub or push.
# Usage: scripts/smoke.sh [--prebuilt] [--timeout-seconds 1200]
set -euo pipefail
script_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
if [[ "$PWD" != "$script_root" ]]; then
  printf 'Run this script from its worktree root: %s\n' "$script_root" >&2
  exit 2
fi
if [[ "$(git branch --show-current)" != feat/shepherd-phase3 ]]; then
  printf 'The smoke must run on feat/shepherd-phase3.\n' >&2
  exit 2
fi
if [[ "${HERDR_ENV:-}" != 1 ]]; then
  printf 'Run this script inside a Herdr-managed pane.\n' >&2
  exit 2
fi
prebuilt=0
timeout_seconds=1200
while (($#)); do
  case "$1" in
    --prebuilt) prebuilt=1; shift ;;
    --timeout-seconds) timeout_seconds=${2:?missing timeout}; shift 2 ;;
    *) printf 'Unknown argument: %s\n' "$1" >&2; exit 2 ;;
  esac
done
[[ "$timeout_seconds" =~ ^[1-9][0-9]*$ ]] || exit 2
for required in herdr git python3 codex omp fuser; do
  command -v "$required" >/dev/null || { printf 'Missing executable: %s\n' "$required" >&2; exit 1; }
done
if (( ! prebuilt )); then
  sh scripts/build.sh
fi
[[ -x bin/herdr-shepherd ]] || { printf 'Build bin/herdr-shepherd first.\n' >&2; exit 1; }
exec python3 - "$script_root" "$timeout_seconds" <<'PY'
import json
import os
from pathlib import Path
import shlex
import shutil
import signal
import socket
import stat
import subprocess
import sys
import tempfile
import time
import tomllib

worktree = Path(sys.argv[1]).resolve()
limit = int(sys.argv[2])
root = Path(tempfile.mkdtemp(prefix="shepherd-smoke-", dir="/tmp")).resolve()
repo = root / "repo"
origin = root / "origin.git"
herdr = shutil.which("herdr")
binary = worktree / "bin/herdr-shepherd"
session = "smoke-" + root.name.rsplit("-", 1)[-1]
state = root / "state/herdr/plugins/herdr-shepherd"
plugin_config = root / "config/herdr/plugins/config/herdr-shepherd"
server = None
sock = None
owned_ids = set()
closed_ids = set()
last_status = {}
result = {"scratch": str(root), "session": session, "ok": False,
          "cleanup_errors": [], "observed_states": [], "pane_evidence": {},
          "roles": [], "origin_unchanged": False}
print(f"Smoke evidence: {root}", flush=True)
def interrupted(signum, _frame):
    raise InterruptedError(f"Smoke interrupted by signal {signum}")
signal.signal(signal.SIGTERM, interrupted)

env = dict(os.environ)
# The fresh server must never inherit a route into the user's live session.
for key in list(env):
    if key.startswith(("HERDR_", "SHEPHERD_", "GIT_")):
        del env[key]
env.update({"XDG_CONFIG_HOME": str(root / "config"),
            "XDG_STATE_HOME": str(root / "state"),
            "XDG_RUNTIME_DIR": str(root / "runtime"),
            "XDG_CACHE_HOME": str(root / "cache"),
            "HERDR_CONFIG_PATH": str(root / "config/herdr/config.toml"),
            "HERDR_STARTUP_CWD": str(repo), "SHELL": "/bin/sh",
            "GIT_CONFIG_GLOBAL": "/dev/null", "GIT_CONFIG_NOSYSTEM": "1"})
# HOME and native harness authentication are retained; no credentials are read
# or copied by this script. Agent runtime files follow each harness's defaults.

def save(name, value):
    (root / name).write_text(json.dumps(value, indent=2) + "\n")

def run(args, *, cwd=None, timeout=45, check=True, command_env=None):
    out = subprocess.run([str(a) for a in args], cwd=cwd or repo,
                         env=command_env or env, text=True,
                         stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                         timeout=timeout)
    if check and out.returncode:
        raise RuntimeError(f"{shlex.join([str(a) for a in args])}: "
                           f"exit {out.returncode}: {out.stderr.strip()[:1500]}")
    return out

def git(*args, cwd=None, **kwargs):
    return run(["git", *args], cwd=cwd, **kwargs).stdout.strip()

def local(path):
    actual = Path(path).resolve()
    if not actual.is_relative_to(root):
        raise RuntimeError(f"Refuse resource outside scratch: {actual}")
    return actual

def client_env():
    if sock is None:
        raise RuntimeError("Scratch socket is not verified")
    local(sock)
    value = dict(env)
    value["HERDR_SOCKET_PATH"] = str(sock)
    value["HERDR_SESSION"] = session
    return value

def h(*args, timeout=45, check=True):
    out = run([herdr, *args], timeout=timeout, check=check,
              command_env=client_env())
    if not check and out.returncode:
        return None
    # Mutating acknowledgements such as `pane run` can succeed silently.
    # Commands whose response supplies IDs still validate their required keys.
    if not out.stdout.strip():
        return {}
    try:
        value = json.loads(out.stdout)
    except json.JSONDecodeError as error:
        raise RuntimeError(f"Invalid JSON from herdr {' '.join(args)}: "
                           f"{out.stdout[:500]}") from error
    if value.get("error"):
        raise RuntimeError(str(value["error"]))
    return value.get("result", value)

def shepherd(*args):
    command_env = client_env()
    command_env["HERDR_PLUGIN_STATE_DIR"] = str(state)
    command_env["HERDR_PLUGIN_CONFIG_DIR"] = str(plugin_config)
    value = json.loads(run([binary, "--json", "--state-dir", state,
                           "--config-dir", plugin_config, *args],
                          command_env=command_env).stdout)
    if not value.get("ok"):
        raise RuntimeError(str(value.get("error")))
    return value["data"]

def status():
    global last_status
    last_status = shepherd("status", "--project-root", repo)
    save("last-status.json", last_status)
    for ticket in last_status.get("tickets", []):
        for child in ticket.get("children", []):
            local(child["path"])
            owned_ids.add(child["workspace_id"])
        if ticket.get("worktree"):
            local(ticket["worktree"])
        if ticket.get("workspace_id"):
            owned_ids.add(ticket["workspace_id"])
    return last_status

def remember_panes(data):
    for agent in data.get("agents", []):
        pane_id = agent.get("pane_id")
        if not pane_id:
            continue
        pane_result = h("pane", "get", pane_id, check=False)
        if not pane_result:
            continue
        pane = pane_result.get("pane", pane_result)
        # Deliberately capture no environment or terminal transcript.
        evidence = {key: pane.get(key) for key in
                    ("pane_id", "workspace_id", "tab_id", "agent", "cwd",
                     "foreground_cwd", "agent_status", "agent_session", "tokens")}
        if agent.get("tab_id"):
            tab = h("tab", "get", agent["tab_id"], check=False)
            if tab:
                evidence["tab"] = tab.get("tab", tab)
        result["pane_evidence"][agent["name"]] = evidence

def stop_watchers():
    # fuser is restricted to our unique scratch lock; validate every PID again.
    locks = list((root / "state").rglob("watch.lock"))
    locks.extend((root / "state").rglob("watch-pr.lock"))
    for lock in locks:
        local(lock)
        output = run(["fuser", lock], check=False)
        for pid_text in output.stdout.split():
            if not pid_text.isdecimal():
                continue
            pid = int(pid_text)
            try:
                argv = Path(f"/proc/{pid}/cmdline").read_bytes().split(b"\0")
                text = [a.decode(errors="replace") for a in argv if a]
                if "watch" not in text or "--state-dir" not in text:
                    raise RuntimeError(f"Refuse unfamiliar lock holder PID {pid}")
                local(text[text.index("--state-dir") + 1])
                if Path(text[0]).resolve() != binary.resolve():
                    raise RuntimeError(f"Refuse unfamiliar watcher executable PID {pid}")
                os.kill(pid, signal.SIGTERM)
                result.setdefault("stopped_watcher_pids", []).append(pid)
            except ProcessLookupError:
                pass
            except FileNotFoundError:
                pass

def cleanup():
    if sock is not None:
        try:
            status()
        except Exception as error:
            result["cleanup_errors"].append(f"Final status: {error}")
        # This entire named server was created here. Discover only its resources.
        try:
            snapshot = h("api", "snapshot").get("snapshot", {})
            for workspace in snapshot.get("workspaces", []):
                ident = workspace["workspace_id"]
                wt = workspace.get("worktree") or {}
                path = wt.get("checkout_path")
                if wt.get("is_linked_worktree") and path:
                    local(path)
                    owned_ids.add(ident)
                    try:
                        h("worktree", "remove", "--workspace", ident, "--force")
                        closed_ids.add(ident)
                    except Exception as error:
                        result["cleanup_errors"].append(f"Remove scratch worktree {ident}: {error}")
            # Linked children are gone before their parent is closed.
            snapshot = h("api", "snapshot").get("snapshot", {})
            for workspace in snapshot.get("workspaces", []):
                ident = workspace["workspace_id"]
                # A pristine isolated server's initial workspace also belongs
                # to this script; never issue a close on another server.
                owned_ids.add(ident)
                try:
                    h("workspace", "close", ident)
                    closed_ids.add(ident)
                except Exception as error:
                    result["cleanup_errors"].append(f"Close scratch workspace {ident}: {error}")
            snapshot = h("api", "snapshot").get("snapshot", {})
            remaining = {w["workspace_id"] for w in snapshot.get("workspaces", [])}
            result["remaining_owned_workspace_ids"] = sorted(remaining & owned_ids)
            if remaining & owned_ids:
                result["cleanup_errors"].append("Owned scratch workspaces remain open")
        except Exception as error:
            result["cleanup_errors"].append(f"Close scratch workspaces: {error}")
        try:
            stop_watchers()
        except Exception as error:
            result["cleanup_errors"].append(f"Stop scratch watcher: {error}")
        try:
            # Explicit verified /tmp socket, never the main server endpoint.
            run([herdr, "server", "stop"], command_env=client_env(), timeout=15)
        except Exception as error:
            result["cleanup_errors"].append(f"Stop owned server: {error}")
    if server is not None:
        try:
            server.wait(timeout=15)
        except subprocess.TimeoutExpired:
            # Popen's child identity belongs to us even when startup failed.
            server.terminate()
            try:
                server.wait(timeout=10)
            except subprocess.TimeoutExpired:
                server.kill()
                server.wait(timeout=10)
    if origin.exists() and repo.exists():
        try:
            after = git("ls-remote", "origin")
            result["origin_after"] = after
            result["origin_unchanged"] = after == result.get("origin_before")
            if not result["origin_unchanged"]:
                result["cleanup_errors"].append("Local origin refs changed")
        except Exception as error:
            result["cleanup_errors"].append(f"Check origin: {error}")
    result["owned_workspace_ids"] = sorted(owned_ids)
    result["closed_workspace_ids"] = sorted(closed_ids)
    result["server_exited"] = server is None or server.poll() is not None
    if result["cleanup_errors"]:
        result["ok"] = False
    save("result.json", result)
    print(json.dumps({"ok": result["ok"], "result": str(root / "result.json"),
                      "error": result.get("error"),
                      "cleanup_errors": result["cleanup_errors"]}), flush=True)

try:
    for directory in (repo, root / "runtime", root / "cache",
                      root / "config/herdr", state, plugin_config):
        directory.mkdir(parents=True, exist_ok=True)
    (root / "runtime").chmod(0o700)
    (root / "config/herdr/config.toml").write_text('''onboarding = false
[terminal]
default_shell = "/bin/sh"
shell_mode = "non_login"
[update]
version_check = false
manifest_check = false
[session]
resume_agents_on_restore = false
[worktrees]
directory = "''' + str(root / "worktrees") + '''"
''')
    git("init", "--initial-branch=main")
    git("config", "user.name", "Shepherd Smoke")
    git("config", "user.email", "shepherd-smoke@example.invalid")
    (repo / "README.md").write_text("# Shepherd smoke\nA disposable shell project.\n")
    (repo / ".gitignore").write_text(".shepherd-report-*\n.shepherd-task-*\n")
    git("add", "README.md", ".gitignore")
    git("commit", "-m", "chore(smoke): seed scratch project",
        "-m", "Give the orchestration smoke an isolated baseline.")
    git("init", "--bare", "--initial-branch=main", origin)
    git("remote", "add", "origin", origin)
    # No push, even during setup: populate the local bare fixture by fetching.
    git("fetch", repo, "main:refs/heads/main", cwd=origin)
    git("fetch", "origin")
    git("remote", "set-head", "origin", "main")
    baseline = run(["sh", "hello.sh"], check=False)
    assert baseline.returncode != 0, "The initial hello check must fail"
    result["baseline_hello_exit"] = baseline.returncode
    result["origin_before"] = git("ls-remote", "origin")
    (root / "origin-before.txt").write_text(result["origin_before"] + "\n")

    with (root / "server.log").open("w") as log:
        server = subprocess.Popen([herdr, "--session", session, "server"],
                                  cwd=repo, env=env, stdin=subprocess.DEVNULL,
                                  stdout=log, stderr=subprocess.STDOUT,
                                  start_new_session=True)
    result["server_pid"] = server.pid
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        if server.poll() is not None:
            raise RuntimeError(f"Scratch server exited: inspect {root / 'server.log'}")
        for path in (root / "config").rglob("*.sock"):
            if stat.S_ISSOCK(path.stat().st_mode):
                try:
                    with socket.socket(socket.AF_UNIX) as probe:
                        probe.settimeout(0.5)
                        probe.connect(str(path))
                        probe.sendall(b'{"id":"smoke-ping","method":"ping","params":{}}\n')
                        reply = json.loads(probe.recv(65536))
                    if "result" in reply:
                        sock = local(path)
                        break
                except (OSError, ValueError):
                    pass
        if sock is not None:
            break
        time.sleep(0.25)
    if sock is None:
        raise RuntimeError("No verified scratch API socket became ready")
    result["socket"] = str(sock)
    save("server-status.json", h("api", "snapshot"))
    created = h("workspace", "create", "--cwd", str(repo),
                "--label", "shepherd-smoke", "--no-focus")
    workspace_id = created["workspace"]["workspace_id"]
    pane_id = created["root_pane"]["pane_id"]
    owned_ids.add(workspace_id)
    result["workspace_id"] = workspace_id
    result["invocation_pane_id"] = pane_id
    # Linking has no skip-build option; the plugin runs its normal build.
    save("plugin-link.json", h("plugin", "link", str(worktree), "--enabled", timeout=300))
    shepherd("init", "--project-root", repo)
    defaults = tomllib.loads((repo / ".shepherd.toml").read_text())
    expected_codex = ["--yolo", "-m", "gpt-6.1-sol", "-c", "model_reasoning_effort=high"]
    expected_omp = ["--auto-approve", "--model", "openai-codex/gpt-6-astra:medium",
                    "--smol", "openai-codex/gpt-6-luna:max"]
    assert defaults["runners"]["codex-sol-high"]["args"] == expected_codex
    assert defaults["runners"]["omp-astra"]["args"] == expected_omp
    result["runner_profiles"] = defaults["runners"]
    # Linking after server startup does not run [[startup]] retroactively.
    # Start the same public watcher explicitly, restricted to scratch state.
    save("watch-start.json", shepherd("watch", "--detach"))
    assert (state / "watch.log").exists(), "Scratch watcher log is missing"
    holders = run(["fuser", state / "watch.lock"])
    assert holders.stdout.strip(), "Scratch watcher does not hold its lock"
    git("add", ".shepherd.toml")
    git("commit", "-m", "chore(smoke): configure default profiles",
        "-m", "Exercise the production runner defaults in the scratch repository.")
    # Execute the required plugin CLI action in our scratch shell, preserving
    # real caller context. Completion is a file, never a matching echoed line.
    invocation = (f"{shlex.quote(herdr)} plugin action invoke herdr-shepherd.start "
                  f">{shlex.quote(str(root / 'action-result.json'))} "
                  f"2>{shlex.quote(str(root / 'action-error.log'))}; "
                  f"printf '%s' \"$?\" >{shlex.quote(str(root / 'action-exit'))}")
    h("pane", "run", pane_id, invocation)
    deadline = time.monotonic() + limit
    action_deadline = min(deadline, time.monotonic() + 120)
    while not (root / "action-exit").exists():
        if time.monotonic() >= action_deadline:
            raise RuntimeError("Plugin action invocation timed out")
        time.sleep(1)
    if (root / "action-exit").read_text().strip() != "0":
        raise RuntimeError("Plugin action failed: " + (root / "action-error.log").read_text()[:1500])

    lead_name = None
    while time.monotonic() < deadline:
        data = status()
        lead = data.get("lead")
        if lead:
            agent = h("agent", "get", lead["agent_name"], check=False)
            live = (agent or {}).get("agent", agent or {})
            if live.get("agent_status") in ("idle", "done"):
                lead_name = lead["agent_name"]
                break
        time.sleep(2)
    if lead_name is None:
        raise RuntimeError("Lead did not become ready for its tiny ticket")
    task = '''Run ticket SMOKE-1: add hello.sh that prints exactly hello plus a newline, and test_hello.sh that checks it. This is a tiny local scratch shell project. Acceptance: sh hello.sh outputs hello; sh test_hello.sh exits 0; both files are committed on the ticket branch. Use planner, a single coder, reviewer, and QA with the configured default profiles. Research and parallel coders are unnecessary. The coder must demonstrate the missing hello check fails before implementing, then run the completed test. Drive all stages yourself using the public shepherd CLI and actual flock reports. Never edit repo code yourself. Never skip review or QA, never manually fabricate reports or edit state files. Never push, open PRs, invoke GitHub, merge, or touch projects outside this scratch root. Keep all ticket, task, and report files inside the assigned scratch checkout/worktree. Delegate the tiny work and stop at ready_for_pr after QA passes. Report status by SMOKE-1.'''
    (root / "ticket-prompt.txt").write_text(task + "\n")
    save("ticket-submission.json", h("agent", "prompt", lead_name, task,
                                      "--wait", "--until", "working", "--timeout", "15000"))
    while time.monotonic() < deadline:
        data = status()
        remember_panes(data)
        ticket = next((t for t in data.get("tickets", []) if t["id"] == "SMOKE-1"), None)
        if ticket:
            state_name = ticket["state"]
            if not result["observed_states"] or result["observed_states"][-1] != state_name:
                result["observed_states"].append(state_name)
                print(f"SMOKE-1: {state_name}", flush=True)
                save("result-progress.json", result)
            if state_name in ("failed", "closed", "cleaned_up", "merged", "pr_open"):
                raise RuntimeError(f"Unexpected ticket state: {state_name}")
            if state_name == "ready_for_pr":
                break
        time.sleep(3)
    else:
        raise RuntimeError("Ticket did not reach ready_for_pr before its deadline")

    ticket_wt = local(ticket["worktree"])
    assert git("branch", "--show-current", cwd=ticket_wt) == ticket["branch"]
    hello = run(["sh", "hello.sh"], cwd=ticket_wt)
    assert hello.stdout == "hello\n", repr(hello.stdout)
    check = run(["sh", "test_hello.sh"], cwd=ticket_wt)
    for name in ("hello.sh", "test_hello.sh"):
        git("ls-files", "--error-unmatch", name, cwd=ticket_wt)
        assert not git("diff", "HEAD", "--", name, cwd=ticket_wt), name
    result["hello_stdout"] = hello.stdout
    result["test_exit"] = check.returncode
    result["ticket"] = ticket
    result["final_status"] = data
    agents = [a for a in data["agents"] if a["ticket"] == "SMOKE-1"]
    result["roles"] = [{key: a.get(key) for key in
                        ("name", "role", "runner", "status", "report", "pane_id", "tab_id")}
                       for a in agents]
    for role in ("planner", "coder", "reviewer", "qa"):
        records = [a for a in agents if a["role"] == role]
        assert records, f"Missing actual {role} agent"
        record = records[-1]
        expected = "omp-astra" if role == "reviewer" else "codex-sol-high"
        assert record["runner"] == expected, record
        assert record["status"] == "reported", record
        assert record.get("report", {}).get("status") == "completed", record
        evidence = result["pane_evidence"].get(record["name"], {})
        tokens = evidence.get("tokens") or {}
        expected_model = "gpt-6-astra/medium" if role == "reviewer" else "gpt-6.1-sol/high"
        assert tokens.get("shepherd_model") == expected_model, (role, tokens)
        assert tokens.get("shepherd_stage") == role, (role, tokens)
        assert (evidence.get("tab") or {}).get("label") == role, (role, evidence)
    assert git("ls-remote", "origin") == result["origin_before"], "Origin refs changed"
    result["ok"] = True
except BaseException as error:
    result["error"] = f"{type(error).__name__}: {error}"
finally:
    cleanup()
sys.exit(0 if result["ok"] else 1)
PY
