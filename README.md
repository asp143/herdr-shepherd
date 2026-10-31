# herdr-shepherd

Shepherd is a herdr plugin intended to take a ticket through planning, parallel coding, review, QA, and a pull request, then handle feedback and cleanup after the user merges. Phase 1 supplies the foundation and upkeep: workspace regrouping, a read-only stale-worktree report, configuration, a JSON CLI, worktree event handling, and keybinding suggestions. Agent orchestration and PR workflows belong to later phases.

## Install

Requires Rust, herdr 0.9.5 or newer, and Git. `gh` supplies optional PR classifications and `du` supplies optional sizes. Linux is tested; the plugin also declares macOS support.

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

Global options are `--json`, `--config-dir PATH`, and `--state-dir PATH`. Phase 1 has no persistent ticket state.

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

All booleans default to `true`. `pr_status`, `notifications`, `model_badge`, and `token_prefix` are reserved for later phases; Phase 1 writes no metadata tokens. Set `regroup = false` to disable auto-regroup, or `keybindings = false` to emit no key suggestions.

Herdr access uses `HERDR_BIN_PATH` or `herdr` on PATH, and `HERDR_SOCKET_PATH` or `~/.config/herdr/herdr.sock`. Event dispatch reads `HERDR_PLUGIN_EVENT` and `HERDR_PLUGIN_EVENT_JSON`. Config validation uses `HERDR_PLUGIN_CONTEXT_JSON` to find the invoking checkout, falling back to the current directory's Git root; `--project-root` overrides both. `--state-dir` takes precedence over `HERDR_PLUGIN_STATE_DIR`; Phase 1 accepts these for future stateful commands and creates no state files.

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
| 1 | Runtime failure | `runtime` |
| 2 | Usage or configuration error | `usage` or `config_invalid` |
| 3 | Required herdr CLI or server unavailable | `herdr_unavailable` |

Prune treats unavailable herdr as a warning and returns its report successfully. Config errors include the offending path. Consumers should use this CLI or herdr actions rather than reading plugin state files.

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

Tests use temporary directories, fake herdr/probe implementations, and sanitized snapshot fixtures. No cadence code is included. Licensed under MIT.
