# herdr-shepherd

Shepherd is a herdr plugin intended to take a ticket through planning, parallel coding, review, QA, and a pull request, then handle feedback and cleanup after the user merges. Phase 2 adds gated GitHub operations, credential-stripped agent environments, worktree push guards, PR sidebar status, verified cleanup, and lifecycle hooks. Ticket orchestration and automatic PR revision loops belong to later phases.

## Install

Requires Rust, herdr 0.9.5 or newer, and Git. `gh` supplies GitHub PR operations and `du` supplies optional sizes. Detached watching uses `setsid`. Linux is tested; the plugin also declares macOS support.

From this checkout:

```sh
sh scripts/build.sh && herdr plugin link "$PWD"
```

The binary is `bin/herdr-shepherd`. Add that directory to your PATH to use the shorter commands below, or invoke the binary by its path. Linking registers three actions: `herdr-shepherd.regroup`, `herdr-shepherd.prune`, and `herdr-shepherd.validate-config`. Creating or removing a herdr worktree automatically regroups workspaces when global upkeep permits it.

## Commands

| Command | Behavior |
| --- | --- |
| `version` | Print the package version. |
| `validate-config [--project-root PATH]` | Validate and print resolved settings, source, and warnings. |
| `regroup` | Strip generated prefixes from linked worktree labels and pack children after their parent workspace. |
| `prune [--root PATH] [--no-size] [--all]` | Report stale worktrees; OPEN entries are hidden unless `--all` is passed. |
| `event` | Handle the event name and JSON supplied by herdr. |
| `keys [--herdr-config PATH]` | Print a keybinding snippet without changing herdr configuration. |
| `pr view --worktree PATH` | Read the current branch PR; return null if none exists. |
| `pr push --worktree PATH` | Push only the declared ticket branch through the gate. |
| `pr create --worktree PATH --base BRANCH --title TITLE --body-file FILE` | Create a ready PR and request missing CODEOWNERS reviewers. |
| `pr owners --worktree PATH --base BRANCH` | Print changed-path owners, excluding the authenticated author. |
| `pr reply --worktree PATH --number N [--comment ID] --body-file FILE` | Reply to an inline review comment, or add a general PR comment. |
| `hook install --worktree PATH --branch BRANCH` | Opt a linked worktree into a per-worktree pre-push guard. |
| `hook pre-push [REMOTE URL]` | Validate Git pre-push stdin using that worktree's ticket branch. |
| `shim gh -- ARGS...` | Check agent GitHub calls and exec the real gh outside the shim PATH. |
| `watch [--once] [--interval SECONDS] [--detach]` | Poll linked-worktree PRs and report sidebar status. |
| `cleanup --worktree PATH [--dry-run] [--force-discard]` | Remove an owned, verified merged ticket workspace. |
| `hooks emit EVENT --json-file FILE` | Emit a configured lifecycle event with a JSON payload. |

Global options are `--json`, `--config-dir PATH`, and `--state-dir PATH`. Ticket state arrives in Phase 3; the watcher persists only its own reported token state.

Regroup preserves user labels such as `fix/foo`, skips children whose parent workspace is absent, and is idempotent. Nested generated prefixes are stripped in one run so another invocation does not rename again. Explicit `regroup` always runs; `[upkeep] regroup` controls automatic event handling.

Prune defaults to `~/.herdr/worktrees` and reports `MERGED`, `CLOSED`, `ORPHAN`, `STRAY`, `NO-PR`, and `OPEN`, plus optional size, `DIRTY`, `AGENTS=n`, and `BRANCH-SWITCHED` flags. Missing `gh`, unsupported remotes, failed lookups, or timeouts yield `NO-PR`. Probe calls time out after 20 seconds, except size probes after 120 seconds; at most eight entries are probed concurrently. If herdr is unavailable, the report still runs with warnings and no workspace or agent annotations. Prune never deletes, moves, or writes anything.

## Configuration

Global settings come from `shepherd.toml` inside the first available location:

1. `--config-dir PATH`
2. `HERDR_PLUGIN_CONFIG_DIR`
3. `~/.config/herdr/plugins/config/herdr-shepherd`

A project `.shepherd.toml` at the Git root replaces the global settings in full, except `[upkeep]`, which always comes from the global file. A project `[upkeep]` is ignored with a warning. Missing files use defaults; malformed files and unsupported schema versions are errors. Unknown sections are preserved for later phases.

Example global settings:

```toml
schema_version = 1
enabled = true

[upkeep]
regroup = true
keybindings = true
pr_status = true
notifications = true
model_badge = true
token_prefix = "shepherd_"
```

All booleans default to `true`. `pr_status`, `notifications`, and `token_prefix` control the watcher. `model_badge` is reserved for agent orchestration. Unknown upkeep fields are errors, and the global schema version is validated even when a project config replaces it. Set `regroup = false` to disable auto-regroup, or `keybindings = false` to emit no key suggestions.

Herdr access uses `HERDR_BIN_PATH` or `herdr` on PATH, and `HERDR_SOCKET_PATH` or `~/.config/herdr/herdr.sock`. Event dispatch reads `HERDR_PLUGIN_EVENT` and `HERDR_PLUGIN_EVENT_JSON`. Config validation uses `HERDR_PLUGIN_CONTEXT_JSON` to find the invoking checkout, falling back to the current directory's Git root; `--project-root` overrides both. `--state-dir` takes precedence over `HERDR_PLUGIN_STATE_DIR`; Watcher state and locks, hook scripts, and agent shim files live in that state directory (fallback `~/.local/state/herdr/plugins/herdr-shepherd`). Concurrent automatic regroup events share an exclusive state-directory lock.

## JSON interface

Every command accepts `--json` before or after the subcommand. Successful output is one JSON envelope on stdout:

```json
{"schema_version":1,"ok":true,"data":{"version":"0.1.0"}}
```

Errors use the same versioned envelope:

```json
{"schema_version":1,"ok":false,"error":{"code":"herdr_unavailable","message":"herdr unavailable at /path/to/herdr.sock"}}
```

| Exit | Meaning | Error code |
| --- | --- | --- |
| 0 | Success | — |
| 1 | Runtime or GitHub failure | `runtime` or `github_unavailable` |
| 2 | Usage or configuration error | `usage` or `config_invalid` |
| 3 | Required herdr CLI or server unavailable | `herdr_unavailable` |
| 4 | Safety policy refusal | `policy_denied` |

Prune treats unavailable herdr as a warning and returns its report successfully. Config errors include the offending path. Consumers should use this CLI or herdr actions rather than reading plugin state files.

## Safety and agent environment

The protected-branch floor is `main`, `master`, `development`, `develop`, `production`, `release-*`, and `sync/*`, plus the repository default and PR base. `[shepherd] protected_branches` adds glob patterns; it cannot remove the floor. Ordinary pushes require an explicit remote and destination equal to the ticket branch. Force flags, deletion, `--no-verify`, wildcards, mirror/all/tag pushes, merging, approval, and auto-merge are refused. Configured implicit tag propagation and remote mirroring are disabled for gated pushes. Opaque GraphQL and review mutation calls are refused conservatively. Literal PR body text does not affect policy parsing.

Hook installation enables worktree config and records `shepherd.ticketBranch` and `core.hooksPath` only in the linked worktree. Installation in the main checkout is refused. The shared hook script reads the invoking worktree's marker each time. A missing marker allows non-shepherd worktrees; a marked worktree rejects other refs, deletion, and non-fast-forward updates.

The pure `gate::env::agent_env` helper removes `GH_TOKEN`, `GITHUB_TOKEN`, `GH_ENTERPRISE_TOKEN`, `GITHUB_ENTERPRISE_TOKEN`, and `SSH_AUTH_SOCK`. It sets `GH_CONFIG_DIR=<state>/agent-gh`, `GIT_TERMINAL_PROMPT=0`, `GIT_SSH_COMMAND="ssh -o BatchMode=yes -o IdentitiesOnly=yes -i /dev/null"`, and prepends `<state>/shim` to PATH. `install_shim` creates that executable shim and empty gh config directory. Phase 3 will apply this environment when launching agents. The shim preserves the stripped environment when forwarding allowed operations. Body and JSON payload readers refuse secret-file paths.

CODEOWNERS search order is `.github/CODEOWNERS`, `CODEOWNERS`, then `docs/CODEOWNERS`. Last matching rule wins for each changed path. Owners are combined, sorted, and deduplicated; emails and the PR author are excluded. Teams stay `org/team`. Existing GitHub reviewer requests are retained without duplicates. No labels or assignees are added.

## PR watcher

The plugin startup command launches `watch --detach`. One poller holds `watch.lock`; duplicate starts return `already_running` with exit 0. Detached launch uses a second lock and a readiness handshake. `--once` performs one poll. Set `[shepherd] pr_poll_seconds` to change the default 180-second interval.

The watcher reports `shepherd_pr` (or the configured prefix plus `pr`) as `open`, `ci-pending`, `ci-failing`, `changes-requested`, `merged`, or `closed`. Metadata goes through Herdr's socket API because the installed 0.9.5 CLI rejects its advertised report-metadata arguments. Only successful observations update the atomic `watch-state.json`; GitHub failures keep the last good token and are logged. A missing PR clears only a token owned by this watcher. Notifications fire on merged, closed, failing CI, and requested-changes transitions when enabled. Ticket-specific cleanup/comment orchestration requires Phase 3 ownership state.

To run an isolated manual watcher:

```sh
bin/herdr-shepherd --state-dir /tmp/shepherd-check --json watch --detach
```

Stop only the PID returned by that invocation. Logs are in the same state directory. A live slow Herdr server is a runtime error; a missing socket or EOF before a response is `herdr_unavailable`.

## Verified cleanup

Cleanup requires a linked worktree with its matching ticket-branch marker and a unique Herdr workspace, no active agents, and a GitHub PR verified merged for that branch. Every child named `<branch>--<number>` must pass ownership and containment checks before any removal. Execution rechecks workspace IDs, repository/remote identity, tips, agents, and the complete child set. Running inside the workspace being removed is refused.

Squash cleanup requires the PR head commit ID to match the local tip when ancestry or patch equivalence cannot prove containment. Local and remote branches are retained unless ancestry proves containment in the base. Local deletion uses an atomic expected-tip ref update after that proof; branch configuration is retained. The specialized remote-delete gate requires a bound merge proof and uses an expected-tip lease on a deletion-only refspec, so an advanced remote branch is retained. Ordinary pushes continue to refuse all force/lease flags and cannot use this path to rewrite commits.

`--dry-run` performs read-only checks, skips fetch, and sends no notifications. Normal preflight fetches origin with pruning. Worktree removal never uses force unless `--force-discard` was explicit; unrelated nested worktrees are refused even then. The main checkout is fast-forwarded only when clean and on the base; otherwise the report includes `main_checkout_dirty` or `main_checkout_not_on_base`. No recursive shell deletion is used. Cleanup emits `ticket.cleaned_up` after a successful non-dry workspace removal.

## Lifecycle hooks

```toml
[[hooks]]
on = "ticket.cleaned_up"
command = ["/path/to/your-handler"]
timeout_seconds = 10
```

Each matching argv receives `SHEPHERD_EVENT`, serialized `SHEPHERD_EVENT_JSON`, and the same JSON on stdin. Hooks default to a ten-second timeout. A timeout kills the hook's process group; failures are logged and do not fail the caller. Unknown configured events produce a validate-config warning. Supported events are `ticket.queued`, `ticket.started`, `ticket.stage_changed`, `ticket.blocked`, `ticket.pr_opened`, `ticket.pr_comment`, `ticket.revised`, `ticket.merged`, `ticket.cleaned_up`, `ticket.closed`, `agent.spawned`, `agent.retired`, `agent.unblocked`, and `agent.escalated`.

## Migrate existing upkeep scripts

After linking, run:

```sh
herdr-shepherd keys
```

The default suggestions bind `prefix+alt+g` to regroup and `prefix+alt+p` to prune with `type = "plugin_action"`. Existing user bindings are skipped. A binding that runs `herdr-regroup` gets a marked replacement suggestion. To migrate, paste the emitted snippet into your herdr config, remove the old `[[keys.command]]` binding being replaced, then run:

```sh
herdr server reload-config
```

The command only prints suggestions. Decide when to retire `~/.local/bin/herdr-regroup` and `~/.local/bin/herdr-prune`; installation leaves those scripts and the herdr config untouched. No static keybindings are registered in the manifest.

## Development

```sh
cargo fmt -- --check
cargo clippy --all-targets -- -D warnings
cargo test --locked
```

Tests use temporary repositories and bare remotes, fake GitHub/Herdr/probe implementations, and sanitized fixtures. Git fixtures ignore host signing configuration. `cargo test --locked --features test-support` additionally verifies the public fake Herdr interface. `proc::capture` exposes exit status and stderr; `proc::run` remains the compatibility wrapper for Phase 1 callers. No cadence code is included. Licensed under MIT.
