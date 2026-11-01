# herdr-shepherd

Shepherd is a herdr plugin that delegates tickets through planning, isolated coding, review, and QA. The PR loop pushes a ready-for-review PR, waits for CI, routes review feedback through the flock, and cleans up after the user merges. It also provides gated GitHub operations, credential controls, workspace upkeep, supervision, cleanup, and lifecycle hooks.

## Install

Requires Rust 1.89 or newer, herdr 0.9.5 or newer, and Git. `gh` supplies GitHub PR operations and `du` supplies optional sizes. Detached watching uses `setsid`. Linux is tested; the plugin also declares macOS support.

From this checkout:

```sh
sh scripts/build.sh && herdr plugin link "$PWD"
```

The binary is `bin/herdr-shepherd`. Add that directory to your PATH to use the shorter commands below, or invoke the binary by its path. Linking registers upkeep actions plus `herdr-shepherd.init`, `herdr-shepherd.start`, `herdr-shepherd.resume`, and `herdr-shepherd.status`. Ticket dispatch groups the new ticket workspace immediately when global upkeep permits it; worktree creation and removal events also trigger regrouping.

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
| `pr push --worktree PATH [--branch BRANCH]` | Push only a stored ticket branch through the isolated transport; an explicit branch must match the store. |
| `pr create --worktree PATH --base BRANCH --title TITLE --body-file FILE` | Create a ready PR and request missing CODEOWNERS reviewers. |
| `pr owners --worktree PATH --base BRANCH` | Print changed-path owners, excluding the authenticated author. |
| `pr reply --worktree PATH --number N [--comment ID] --body-file FILE` | Reply to an inline review comment, or add a general PR comment. |
| `hook install --worktree PATH --branch BRANCH` | Opt a linked worktree into a per-worktree pre-push guard. |
| `hook pre-push [REMOTE URL]` | Validate Git pre-push stdin using that worktree's ticket branch. |
| `shim gh -- ARGS...` | Check agent GitHub calls and exec the real gh outside the shim PATH. |
| `watch [--once] [--interval SECONDS] [--detach]` | Drive ticket CI, comment pickup, merge cleanup, and sidebar PR status. |
| `cleanup --worktree PATH [--dry-run] [--force-discard]` | Remove an owned, verified merged ticket workspace. |
| `hooks emit EVENT --json-file FILE` | Emit a configured lifecycle event with a JSON payload. |

| `ticket list [--project-root PATH]` | List tickets in the invoking project's store. |
| `ticket show <id> [--project-root PATH]` | Show a stored ticket and its history. |
| `ticket fetch <id> [--project-root PATH]` | Fetch ticket JSON from the configured source without storing it. |
| `ticket pr <id> --summary-file FILE --changes-file FILE --testing-file FILE` | Push the stored ticket branch and open a ready PR after QA. |
| `ticket comments <id> [--new]` | Read review comments, issue comments, and changes-requested review bodies. |
| `ticket reply <id> --comment ID [--kind KIND] --body-file FILE` | Decline a comment with evidence; kind distinguishes review, issue, and review-body IDs. |
| `ticket revise <id> --comments ID,ID [--kind KIND] --task-file FILE` | Accept feedback and queue coder, reviewer, and QA. |
| `ticket ack <id> --comment ID [--kind KIND] --body-file FILE` | Describe a pushed revision and its commit SHA. |
| `init [--project-root PATH]` | Write defaults and snapshot project trust without changing herdr configuration. |
| `start`, `resume`, `status` | Launch or focus the lead, recover launches, and report ticket state. |
| `ask <text...>` or `ask --file PATH` | Send a request to this project's lead, starting it if needed; `ask -` reads stdin. |
| `ticket add [--id ID] --title TITLE --body-file FILE` | Add a queued ticket; optional acceptance, type, and base flags are supported. |
| `ticket dispatch` | Schedule revisions before queued tickets and launch planners or revision coders. |
| `ticket transition ID --to STATE [--reason REASON]` | Apply a legal transition with the configured revision cap. |
| `ticket integrate ID` | Integrate child commits, roll back conflicts, and remove proven integrated children. |
| `ticket cancel ID` | Close a ticket while preserving its worktree and commits. |
| `agent spawn --ticket ID --role ROLE --task-file FILE [--unit N]` | Launch a ticket tab; `--unit N` gives a coder a private child checkout. |
| `agent report --agent NAME --report-file FILE` | Validate and store the report; an inherited caller pane must own that agent. |
| `agent list`, `agent status NAME`, `agent cancel NAME [--force]` | Inspect or retire owned flock agents. |
| `config trust [--project-root PATH]` | Explicitly replace the project trust snapshot and print its diff. |

Global options are `--json`, `--config-dir PATH`, and `--state-dir PATH`.
Ticket inspection requires `--state-dir` or `HERDR_PLUGIN_STATE_DIR`. State lives
in `projects/<sha256 of canonical project path>/state.json`, with a per-project
lock covering each update and a synced atomic replacement. Ticket inspection
does not change stored ticket data. Reads use shared locks; opening and reading
a missing store returns empty data without creating its state directory.
Writers create the directory on the first update. Corrupt-file errors name
`state.json` and recommend preserving it and restoring a known-good backup.

The library's ticket state machine includes `ci_wait`, which consumes a ticket
slot. Review, QA, and red CI bounces share a revision counter; exceeding the cap
blocks the ticket with `loop_cap`. `transition` uses the default cap of three;
`transition_with_limit` accepts the project's resolved cap. Blocked tickets may
resume only their recorded prior state, or be cancelled.
PR-ready, open-PR, and merged tickets can also block for escalation or cleanup
failure. Blocked tickets can fail; resuming a loop-cap block grants a fresh set
of revision rounds.

`ticket::pr_body` builds PR titles and the ordered Summary, Changes, Testing,
optional Open and deferred findings, optional Finding history, and optional
Ticket sections. The Ticket link uses `Ticket.url`. Stored findings and agent
reports are append-only evidence, not an unresolved-issue list. Without explicit
publication dispositions, every finding appears in Finding history as
"current disposition unclassified". No severity or prose prefix implies resolution.

The existing command remains usable:

```sh
herdr-shepherd ticket pr T-1 --summary-file summary.txt --changes-file changes.txt --testing-file testing.txt
```

Summary is text; changes and testing accept JSON string arrays or nonempty lines.
To classify findings for this PR, inspect `ticket show T-1` and supply optional
`--finding-dispositions-file dispositions.json` using this strict JSON contract:

```json
{
  "ticket_id": "T-1",
  "findings": [
    {"index": 0, "status": "resolved", "reason": "Covered by the boundary test"},
    {"index": 1, "status": "open"},
    {"index": 2, "status": "deferred", "reason": "Separate follow-up"}
  ]
}
```

`index` is the zero-based position in the append-only `findings` array from
`ticket show`. Status must be `open`, `deferred`, or `resolved`; `reason` is
optional text. Open and deferred entries of every severity appear in the current
section; resolved and unclassified entries appear in history. Each section
retains finding order, text, severity, and location, with the original index.
Omitted entries, an empty array, and findings appended after metadata preparation
remain unclassified and visible. This metadata classifies publication text only;
it never changes stored evidence. Raw findings remain available through
`ticket show`, and original reports through `agent status <name>`.

Wrong ticket IDs, duplicate or out-of-range indices, unknown fields/statuses,
and malformed JSON are rejected before backend calls. References are validated
again against the publication snapshot under the existing dispatch lock.
All publication state, QA commit, branch, transport, identity and reviewer gates
still apply. The final body, including reasons and history, must pass the
60000-character limit and attribution check.

`check_attribution` rejects specified attribution phrases with `attribution`
(exit 2) while allowing technical references such as "Claude Code hook fix".
Future GitHub writers must call that check before sending human-facing text.
Attribution matching accepts Unicode whitespace between words, including NBSP
and repeated spaces.

Regroup preserves user labels such as `fix/foo`, skips children whose parent workspace is absent, and is idempotent. Nested generated prefixes are stripped in one run so another invocation does not rename again. Explicit `regroup` always runs; `[upkeep] regroup` controls initial ticket placement and automatic event handling. A regroup failure is logged and leaves the created ticket available.

Prune defaults to `~/.herdr/worktrees` and reports `MERGED`, `CLOSED`, `ORPHAN`, `STRAY`, `NO-PR`, and `OPEN`, plus optional size, `DIRTY`, `AGENTS=n`, and `BRANCH-SWITCHED` flags. Missing `gh`, unsupported remotes, failed lookups, or timeouts yield `NO-PR`. Probe calls time out after 20 seconds, except size probes after 120 seconds; at most eight entries are probed concurrently. If herdr is unavailable, the report still runs with warnings and no workspace or agent annotations. Prune never deletes, moves, or writes anything.

## Configuration

Global settings come from `shepherd.toml` inside the first available location:

1. `--config-dir PATH`
2. `HERDR_PLUGIN_CONFIG_DIR`
3. `~/.config/herdr/plugins/config/herdr-shepherd`

`validate-config` reads `.shepherd.toml` at the selected Git root. Runtime commands use the project configuration snapshot captured by `init`, `start`, or ticket startup; `config trust` explicitly replaces it. When a snapshot exists, `validate-config` warns if the on-disk file differs from it, including when either copy is missing. Validation never creates or refreshes a snapshot. To apply project config changes to runtime commands, commit them to the default branch, then run `config trust`.

`config trust` captures only `.shepherd.toml` committed on the default branch. If a local file exists but that branch has no committed config, trust fails and preserves the stored snapshot. If local edits differ from the committed file, trust succeeds using the committed bytes and warns in both human and `--json` output that those edits were not captured. If neither copy exists, trust succeeds without project config, so a committed config can be removed. The `init` flow still snapshots the configuration it initializes.

Project settings replace the global settings in full, except `[upkeep]`, which always comes from the global file. A project `[upkeep]` is ignored with a warning. Missing files use defaults; malformed files and unsupported schema versions are errors. Unknown sections are preserved for later phases.

`validate-config` also resolves `[shepherd]`, `[roles.*]`, `[runners.*]`, and
`[supervision]` from the selected file. Its existing output fields are retained,
with `shepherd`, `roles`, `runners`, and `supervision` added alongside them.
Defaults include planner, researcher, coder, reviewer, and QA roles, with the
`claude-opus`, `codex-sol-high`, `omp-astra`, and `omp-glm-infra` runner catalog,
in that order. The lead defaults to `claude-opus`, which runs Claude Code with
`--model claude-opus-5-5 --dangerously-skip-permissions`. Planner, coder, and QA
use `codex-sol-high`; researcher and reviewer use `omp-astra`. Named role and
runner entries override built-in fields; new roles default to the ticket
worktree and `codex-sol-high`. No fields are merged between config files.
Configured roles and runners retain file order, followed by unspecified
built-ins in catalog order. The library uses `IndexMap<String, Runner>` for
`Resolved.runners`. Unknown fields within orchestration sections and rules are
configuration errors; each role must have at least one runner.

Shepherd defaults are two concurrent tickets, six parallel agents, three
revision rounds, and PR polling every 180 seconds. Supervision defaults are a
30-second sweep, 120-second report grace, 20-minute stall threshold, and
15-minute escalation threshold. User regex rules run before built-in
`codex-trust`, `press-enter`, `codex-update-notice`, `claude-trust`, and
`claude-bypass-permissions` rules. The Claude rules recognize the startup dialogs
verified in Claude Code 2.1.316, require the selected `No, exit` option, and send
`down`, then `enter`. Other text, menu layouts, and selections require manual
handling. Rule keys are
limited to `enter`, `esc`, `y`, `n`, `1` through `9`, `down`, and `up`. The supervisor executes these rules for owned panes inside their assigned worktrees.
Rules require a nonempty pattern and keys, unique names (including built-ins),
and either no harness filter or one of `codex`, `omp`, `claude`, and `pi`.

An optional ticket source is an argv array in the selected configuration:

```toml
[ticket_source]
command = ["your-ticket-command", "--json"]
```

`ticket fetch <id>` appends the ID as one literal argument and allows 30 seconds
for execution and stdout capture. The command must print JSON with string
`id`, `title`, and `description` fields, an `acceptance_criteria` string array,
and optional `url`. The returned ID must match the request, and the title must
not be blank. Missing or empty configuration returns `no_ticket_source`
(exit 2); paste the ticket text instead. Invalid JSON, invalid ticket content,
execution failures, and timeouts are runtime errors (exit 1). Timeout errors
and nonzero exit statuses have distinct messages. Sources run in an owned
process group; timeouts kill the group, including descendants retaining
stdout. Fetching requires no state directory and does not add a ticket to the
store.

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

Herdr access uses `HERDR_BIN_PATH` or `herdr` on PATH, and `HERDR_SOCKET_PATH` or `~/.config/herdr/herdr.sock`. Event dispatch reads `HERDR_PLUGIN_EVENT` and `HERDR_PLUGIN_EVENT_JSON`. Config validation and ticket commands use `HERDR_PLUGIN_CONTEXT_JSON` to find the invoking checkout, falling back to the current directory's Git root; `--project-root` overrides both. `--state-dir` takes precedence over `HERDR_PLUGIN_STATE_DIR`; Watcher state and locks, hook scripts, and agent shim files live in that state directory (fallback `~/.local/state/herdr/plugins/herdr-shepherd`). Concurrent automatic regroup events share an exclusive state-directory lock.

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
| 2 | Rejected human-facing attribution | `attribution` |
| 2 | Ticket source is not configured | `no_ticket_source` |
| 3 | Required herdr CLI or server unavailable | `herdr_unavailable` |
| 4 | Safety policy refusal | `policy_denied` |

Prune treats unavailable herdr as a warning and returns its report successfully. Config errors include the offending path. Consumers should use this CLI or herdr actions rather than reading plugin state files.

## Safety and agent environment

The protected-branch floor is `main`, `master`, `development`, `develop`, `production`, `release-*`, and `sync/*`, plus the repository default and PR base. `[shepherd] protected_branches` adds glob patterns; it cannot remove the floor. Ordinary pushes require an explicit remote and destination equal to the ticket branch. Force flags, deletion, `--no-verify`, wildcards, mirror/all/tag pushes, merging, approval, and auto-merge are refused. Configured implicit tag propagation and remote mirroring are disabled for gated pushes. Opaque GraphQL and review mutation calls are refused conservatively. Literal PR body text does not affect policy parsing.

Hook installation enables worktree config and records `shepherd.ticketBranch` and `core.hooksPath` only in the linked worktree. Installation in the main checkout is refused. The shared hook script reads the invoking worktree's marker each time. A missing marker refuses pushes when the shepherd hooks directory is installed. A marked worktree rejects other refs, deletion, and non-fast-forward updates.

The `gate::env::agent_env` helper replaces `GH_TOKEN` and `GITHUB_TOKEN` with an invalid placeholder and removes enterprise tokens and `SSH_AUTH_SOCK`. It uses a cleaned global Git configuration, disables system configuration, and sets both ask-pass programs to `/bin/false`. It sets `GH_CONFIG_DIR=<state>/agent-gh`, `GIT_TERMINAL_PROMPT=0`, `GIT_SSH_COMMAND="ssh -o BatchMode=yes -o IdentitiesOnly=yes -i /dev/null"`, and prepends `<state>/shim` to PATH. `install_shim` creates that executable shim and empty gh config directory. Agent launch applies this environment before starting a harness. The shim preserves the stripped environment when forwarding allowed operations. Body and JSON payload readers refuse secret-file paths.

CODEOWNERS and the changed-path base are read from the verified base commit through the trusted transport, so ticket edits cannot choose their own reviewers. CODEOWNERS search order is `.github/CODEOWNERS`, `CODEOWNERS`, then `docs/CODEOWNERS`. Last matching rule wins for each changed path. Owners are combined, sorted, and deduplicated; emails and the PR author are excluded. Teams stay `org/team`. Existing GitHub reviewer requests are retained without duplicates. No labels or assignees are added.

## PR watcher

The plugin startup command launches `watch --detach`. One poller holds `watch.lock`; duplicate starts return `already_running` with exit 0. Detached launch uses a second lock and a readiness handshake. `--once` performs one poll. Set `[shepherd] pr_poll_seconds` to change the default 180-second interval.

The watcher reports `shepherd_pr` (or the configured prefix plus `pr`) as `open`, `ci-pending`, `ci-failing`, `changes-requested`, `merged`, or `closed`. Metadata goes through Herdr's socket API because the installed 0.9.5 CLI rejects its advertised report-metadata arguments. Only successful observations update the atomic `watch-state.json`; GitHub failures keep the last good token and are logged. A missing PR clears only a token owned by this watcher. Notifications fire on merged, closed, failing CI, and requested-changes transitions when enabled. Stored tickets are polled even when their workspace is absent. Green CI hands the PR to the user; red CI queues a coder with failing check names and URLs and consumes a revision round. Missing checks hand off after ten continuous minutes; GitHub outages leave the ticket and revision counter unchanged. Comments received during CI or building remain pending until `pr_open`, then wake or resume the lead once. Replies are recorded so they cannot feed back into triage.

The lead checks each comment against the code, replies with evidence when declining it, and delegates legitimate feedback through coder, reviewer, and QA. Revision preparation fetches the snapshotted URL into owned refs and fast-forwards with owned Git configuration, preserving the pre-sync review base. Divergence or a moving remote blocks with `conflict`; ordinary pushes never force. Shepherd never resolves review threads, approves, requests changes, or merges. A merge during a revision retires its flock and preserves unpushed work for the user; a closed PR retains its resources.

To run an isolated manual watcher:

```sh
bin/herdr-shepherd --state-dir /tmp/shepherd-check --json watch --detach
```

Stop only the PID returned by that invocation. Logs are in the same state directory. A live slow Herdr server is a runtime error; a missing socket or EOF before a response is `herdr_unavailable`.

## Verified cleanup

Cleanup requires a ticket linked worktree with its matching ticket-branch marker and a unique Herdr workspace, no active agents, and a GitHub PR verified merged for that branch. Private unit checkouts have no workspace: cleanup removes them through Git before removing the ticket workspace. Stored children from earlier versions retain their verified workspace cleanup. Every child named `<branch>--<number>` must pass ownership and containment checks before any removal. Execution rechecks workspace IDs, repository/remote identity, tips, agents, and the complete child set. Running inside the workspace being removed is refused.

Squash cleanup requires the PR head commit ID to match the local tip when ancestry or patch equivalence cannot prove containment. Local and remote branches are retained unless ancestry proves containment in the base. Local deletion uses an atomic expected-tip ref update after that proof; branch configuration is retained. The specialized remote-delete gate requires a bound merge proof and uses an expected-tip lease on a deletion-only refspec, so an advanced remote branch is retained. Ordinary pushes continue to refuse all force/lease flags and cannot use this path to rewrite commits.

`--dry-run` performs read-only checks, skips fetch, and sends no notifications. Normal preflight fetches the verified base through the snapshotted URL and the shepherd-owned transport. Worktree removal never uses force unless `--force-discard` was explicit; unrelated nested worktrees are refused even then. The main checkout is fast-forwarded only when clean and on the base; otherwise the report includes `main_checkout_dirty` or `main_checkout_not_on_base`. No recursive shell deletion is used. Cleanup emits `ticket.cleaned_up` after a successful non-dry workspace removal.

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

Tests use temporary repositories and bare remotes, fake GitHub/Herdr/probe implementations, and sanitized fixtures. Git fixtures ignore host signing configuration. `cargo test --locked --features test-support` additionally verifies the public fake Herdr interface. `proc::capture` exposes exit status and stderr; `proc::run` remains the compatibility wrapper for Phase 1 callers. Cadence and pane-event supervision use owned-agent fixtures. Licensed under MIT.

### Managed OMP prompt delivery

Managed OMP launch, native resume and provider fallback add a final per-run
`--config <state>/omp-paste-v1.yml` overlay containing only
`paste.largeMenuThreshold: 0`. This prevents the large-paste menu from consuming
the submit key for multiline flock tasks. Configured runner arguments, model
selection and other overlays are retained; shepherd does not read or modify
user OMP settings. The overlay is written atomically in shepherd's state directory.

This uses the public repeatable `--config` overlay and paste setting verified
with OMP 18.14.1. Older or other builds must support both features; unsupported
startup errors are surfaced without dropping the overlay or trying another
submission. Upgrade an incompatible OMP build before using managed OMP runners.
Already running agents are unaffected. Ambiguous prompt outcomes remain
quarantined for operator inspection and are never automatically replayed.

Every automatic prompt claims its recipient durably before input. Initial tasks
can leave quarantine after independently observed owned Working evidence; a
follow-up cannot, because Working may describe an earlier turn. Uncertain nudges
retain their clock and payload in the agent's `observed.prompt_claim`; queued
notifications move there before input while later enqueues remain ordered.
Only a proven rejection before input restores a notification to the queue.
A real report resolves its recipient's claim. Automatic retirement uses
nonblocking operation ownership and fresh pane/session/worktree/project checks;
deferred retirement keeps resources and stops CI handoff or cleanup. Explicit
`agent cancel` remains available to the operator after inspecting uncertainty.

Automatic lead notices carry their ticket, agent and creation time. The queue
heals during supervision, before delivery and when launching a lead: it removes
self-notices, notices for finished or missing tickets, notices for inactive
agents without an active ticket, and notices older than 24 hours. Capacity
recovery removes the oldest automatic notices first. `status` reports the
remaining pending count; routine healing logs counts without message text or
notifications. A relaunched lead retains notices for active tickets.

Messages from `ask` never expire or get trimmed, including intentional duplicates.
State files migrate from schema 1 to schema 2 automatically when saved. Legacy
queue strings are classified as automatic only when they match a known generated
notice; unrecognized strings remain user requests. A full queue still
refuses new `ask` requests until capacity is available. Supervision sends blocked
lead conditions to the user's notification channel once per episode while safe
rules can still handle trust dialogs; flock notices and escalation are also
limited per episode, even when screen text changes.

An opt-in PTY regression uses the installed OMP with empty fixture HOME/XDG and
agent directories, no tools/extensions/context files, and a loopback fake
provider inside a separate network namespace. It requires Python 3 and bwrap:

```sh
SHEPHERD_TEST_OMP_BIN=/absolute/path/to/omp cargo test --locked --features test-support --test omp_prompt_delivery -- --ignored --nocapture
```

The regression passes the actual managed launch arguments and a generated
multiline flock task to OMP. It checks separate paste/CSI 13u and combined
paste/CSI 13;1:1u, with exactly one provider turn per intended submission.
These are causal fixture controls; historical Herdr encoding and chunk
boundaries and live OMP delivery remain unverified. The test creates its own
bwrap sandbox; a surrounding sandbox must allow nested user/network namespaces.

## Credential controls and their limits

Agents use a generated global Git config that omits every credential helper,
SSH command, URL rewrite, and include. System Git configuration is disabled,
ask-pass programs are disabled, and GitHub token variables contain an invalid
placeholder so `gh` does not fall back to keyring credentials. The pane handoff
also removes inherited Git configuration overrides before exporting these
settings. Safe user settings, including Git author and signing preferences,
are retained without following includes.

These controls are defense in depth. An agent running as the same OS user can
still reach credentials by reading files directly, accessing the keyring,
reading `~/.ssh`, or finding an ssh-agent socket. GitHub branch protection is
needed for guarantees against those actions. The known same-user credential limit is deliberate: v1 does not add an OS sandbox.
Branch protection on GitHub supplies the guarantee. Shepherd reads protection for
the ticket base and snapshotted default branch at startup, warns without blocking
when either is unprotected, and shows warnings in status and the lead summary.
Successful protection reads are cached for 24 hours; unknown results are retried.
Shepherd never changes repository protection settings.


## Ad-hoc tasks

Start the lead from the project workspace and give it a plain request in chat:

```sh
herdr plugin action invoke herdr-shepherd.start
```

For example, say "fix the typo in the login banner". No ticket ID or prepared
acceptance criteria are needed. The lead derives a title and testable criteria,
reports the generated ID and criteria, and dispatches immediately. It asks first
only when the request is too ambiguous to derive any acceptance criteria.
Distinct tasks in one message become separate tickets.

You can also send the request from a terminal:

```sh
herdr-shepherd ask "fix the typo in the login banner"
```

`ask` starts the project's lead when needed and durably queues the message while
the lead is busy or blocked. Use `--file PATH` or `-` for file or stdin input,
and `--project-root PATH` to target an explicit project. Managed flock agent
panes cannot use `ask`.

## Ticket orchestration and supervision

Run `init`, review the generated settings, then invoke the start action from the
project workspace. Give the lead the ticket text and acceptance criteria. The
lead never edits repository code: it uses `ticket add`, `ticket dispatch`,
`ticket transition`, and `agent spawn` to delegate the work. Every flock agent
submits a JSON report. A report preserves tests, changed paths, findings, and
its commit SHA. Only the lead talks to the user.

The workspace layout is `repo workspace -> ticket worktree workspace -> flock tabs`.
Dispatch creates one linked worktree workspace on the ticket branch, labels it
with the ticket slug, and groups it immediately after the repo workspace when
`[upkeep] regroup` is enabled. Planner, researcher, coder, reviewer, QA, and
revision agents all open as tabs in that ticket workspace.

A coder launched with `--unit N` uses a private Git checkout and branch
`<ticket-branch>--<N>`, including the first unit coder. Its tab stays in the ticket
workspace while its cwd points to that private checkout. Each writable coder
has its own checkout and push guard; unit creation adds no Herdr workspace.
Integration retires only the owning child tabs, removes integrated private
checkouts and branches, and leaves sibling tabs open. Cancellation preserves
worktrees and commits for recovery.

The queue gives waiting revisions priority. Capacity exhaustion leaves queued
and revising tickets unchanged; failed launches block with `launch_failed` and
other dispatch actions continue. Concurrent spawn calls serialize placement so
two coders cannot accidentally share a writable worktree. Child integration is
transactional: any conflict restores the pre-integration HEAD, leaves a clean
branch, and records both ranges in `Ticket.integration_ranges`. A resolver can
merge the child branches locally, resolve conflicts, report, and rerun
`ticket integrate` to recognize the contained commits.

Integration records each picked child commit with Git's `cherry-pick -x`
provenance, including commits that are empty on arrival. Retries recognize
those records in the ticket's first-parent history and pick only missing
commits. Later ticket edits or deliberate reverts do not undo that proof of
integration. An old integrated child without provenance is retained unless
its tip is an ancestor of the ticket. Review its work, merge the child branch
locally (resolving conflicts if needed), then rerun `ticket integrate` to finish
cleanup. A child with outstanding cleanup cannot be reused by another agent;
finish cleanup first.

Pane events and periodic sweeps supervise owned agents. Safe fixed-key dialog
rules run only after policy and foreground-cwd checks, and revalidate the screen
before input. Unknown dialogs go to the lead. Idle or stalled agents receive one
nudge before escalation; crashed agents resume once with the pinned profile and
session; provider failures use the role's next runner. Startup prompts and lead
pings remain durable while blocked. A corrupt project or failed screen read does
not stop healthy agents. PR polling runs in a separate bounded worker so it
cannot hold up the supervision sweep.

## Stored project trust and push transport

`init`, `start`, and ticket startup capture the origin URLs, default branch, and
project config blob/text in `StoreData.trust`. Runtime policy and lifecycle hooks
use this stored text. Shared Git config, refs, and edited project files never
refresh it implicitly. Use `config trust` from a user pane to review the printed
diff and explicitly replace the snapshot; managed agent panes cannot invoke it.
Global upkeep remains selected from the global configuration.

Gated pushes require a stored ticket for the worktree. They run from a
shepherd-owned bare repository under the state directory, use the stored remote
and a verified ticket commit SHA, and never import repository SSH commands,
credential helpers, URL rewrites, includes, or receive-pack settings. Remote
inspection and deletion use the same transport. Local remotes are refused in
release builds; temporary test fixtures have an explicit debug-only allowance.
Direct GLOBAL authentication settings are copied into a private, no-include
transport config. This retains global credentials and SSH configuration while
preventing global includes from reloading mutable repository settings.

Repo-local `credential.helper` and `core.sshCommand` settings in a project's own
shared `.git/config` can still reach agent Git commands. Agents run as the same
OS user and can explicitly reach files, keyrings, or SSH agent sockets. These
layers are defense in depth. GitHub branch protection provides the remote
protection; an OS sandbox is a separate choice.

## Manual smoke

`scripts/smoke.sh --prebuilt` uses real default runner profiles in an isolated
headless herdr server, a `/tmp` project, and a local bare origin. It checks the
actual planner/coder/reviewer/QA reports and badges, local gated pushes,
`ci_wait` to `pr_open`, comment triage, a declined reply, a reviewed
revision and acknowledgement, and cleanup after a simulated user merge. GitHub
operations use the debug-only fake backend; the base is updated only to simulate
the user merging in the disposable bare remote. It leaves evidence under the
printed `/tmp/shepherd-smoke-*` path and never controls the existing server.
