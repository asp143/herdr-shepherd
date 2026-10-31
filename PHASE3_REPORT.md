# Phase 3 orchestration and supervision report

Phase 3 Tasks 4–10 and both security follow-ups are complete on
`feat/shepherd-phase3`. The core merge and implementation are committed; this
report accompanies the Task 10 smoke-script commit. No real remote was pushed
and no real PR was opened or commented on.

## Merge and branch

- Worktree: `/home/asp143/.herdr/worktrees/herdr-shepherd/feat-shepherd-phase3`.
- Branch: `feat/shepherd-phase3`; all new commits remain on this branch.
- Core merged with `--no-ff` as `91e53bd` (`merge(core): integrate ticket and
  role core`), parents `42d4562` and `67b2d66`.
- Additive conflicts were resolved by retaining both branches' functionality;
  Cargo regenerated the lockfile. README also needed an additive merge beyond
  the four conflict files anticipated by the task.
- Standard-library `File::lock`, `lock_shared`, and `try_lock` are the single
  file-lock mechanism throughout the project. `fs2` was removed.
- Before new task implementation, the merged tree passed all 246 tests, format
  checking, and clippy. Logs: `/tmp/shepherd-phase3-merge-tests.log` and
  `/tmp/shepherd-phase3-merge-clippy.log`.

The external plans and specification were read at their supplied absolute
paths. They were not copied into this worktree or committed.

## Task commits

Each completed Phase 3 task has one dedicated conventional commit. Separately
requested security and recovery fixes have their own commits, listed below;
they are additional to the task commits.

| Task | Commit | Result |
| --- | --- | --- |
| 4: harness adapters and launch | `101e283` | Codex/omp/Claude/pi adapters, durable launch intent, fallback, environment preparation, model/stage tokens |
| 5: flock and reconciliation | `fa3ca83` | Spawn/report/list/status/cancel and crash recovery |
| 6: intake and scheduler | `5640ade` | Ticket intake, worktree setup, revision-priority scheduling |
| 7: child worktrees | `ea210b2` | Ordered integration, full conflict rollback, owned-child cleanup |
| 8: prompts and lead | `bda4f6f` | Role/lead prompts, init/start/resume/status actions and manifest events |
| 9: supervision | `6364b08` | Policy-first unblocking, report/stall nudges, escalation, native resume, provider fallback, independent sweeps |
| 10: scratch smoke | This commit: `test(smoke): add end-to-end smoke script` | PASS: native default-profile pipeline and scratch cleanup |

## Security and recovery batch 1

These fixes were kept separate from orchestration task commits so each risk
and its regression coverage can be reviewed independently.

| Fix | Commit | Regression evidence |
| --- | --- | --- |
| Isolate agent credential lookup; sanitize Git environment and stop implicit gh keyring fallback | `aae3f47` | `agent_git_config_removes_credential_and_redirects`, `agent_gh_token_does_not_fall_back_to_keyring`, `pane_environment_removes_inherited_git_config_injection`, `agent_git_config_removes_http_auth_sources` in `tests/agent_env_security.rs` |
| Restrict repository mutation APIs and CLI mutation paths | `db73645` | `shim_blocks_repository_ref_api_writes`, `shim_blocks_contents_branch_and_workflow_api_writes`, `shim_blocks_repository_and_workflow_cli_mutations`, `shim_blocks_repository_settings_and_ruleset_writes`; positive read/reply coverage in `shim_preserves_read_only_repo_commands_and_comment_replies` |
| Retry the detached watcher startup lock and rotate oversized logs | `00eb418` | `detached_child_retries_transient_parent_lock`, `detach_rotates_watch_log_past_five_mib` in `tests/watch_lock.rs` |
| Bind pushes, ticket identity, hooks, and project policy to trusted project state | `e8efa04` | `config_redirect_without_explicit_ticket_is_blocked`, `config_redirect_cannot_rewrite_main_with_explicit_branch`, `missing_ticket_marker_fails_closed`, `linked_config_redirect_without_marker_is_blocked`, `worktree_remote_and_url_redirects_are_ignored`, `installed_hook_keeps_custom_config_and_state`, `committed_base_blocks_ticket_config_hook_injection`, `poisoned_remote_tracking_config_cannot_inject_hooks` |
| Show ignored files before a cleanup removal | `4b864d2` | `dry_run_lists_ignored_files_before_removal` in `tests/cleanup.rs` |

Fixture follow-up `9ba1668` (`test(gate): run the token fixture via shell`)
corrected the existing keyring-blocking test fixture invocation; it did not
change production behavior.

## Security and recovery batch 2

| Fix | Commit | Regression evidence |
| --- | --- | --- |
| Allow only explicit issue/run read commands | `186c668` | `shim_blocks_issue_and_run_mutations` in `tests/shim_api_security.rs` |
| Preserve structured herdr prompt error codes and distinguish accepted-but-timed-out prompts | `0debe76` | `only_structured_prompt_timeouts_imply_submission` in `src/error.rs`, `prompt_errors_preserve_server_codes_only` in `src/herdr/cli.rs` |
| Serialize worktree placement, bind reports to the caller pane, and recover pending startup tasks | `4feb49f` | `concurrent_initial_coders_use_distinct_worktrees`, `report_requires_calling_pane_identity`, `reconcile_waits_for_readiness_before_pending_delivery`, `reconcile_delivers_pending_startup_prompt_once` |
| Configure child hooks with the selected config on the first install | `a615d47` | `child_hook_uses_explicit_config_on_first_install` in `tests/children.rs` |
| Snapshot trusted config bytes, URLs, and default-branch identity; refresh only through an explicit trust action | `3c51a00` | `stored_config_blocks_local_default_ref_hook_injection`, `snapshot_survives_shared_origin_and_main_head_edits`, `configuration_reads_do_not_create_or_refresh_trust`, `explicit_refresh_reports_diff_and_replaces_config_bytes`, `bootstrap_never_follows_shared_git_config_includes`, `cli_hooks_use_frozen_text_after_shared_local_ref_poisoning`, `pr_push_requires_ticket_even_with_explicit_branch` |
| Isolate privileged Git transport from mutable shared repository config | `124ac50` | `shared_pushurl_cannot_redirect_shepherd_to_evil_remote`, `shared_ssh_helper_include_and_url_rewrite_never_execute`, `stored_branch_must_match_the_ticket_head`, `packed_ticket_ref_is_verified_and_pushed_from_owned_bare_repo`, `global_include_of_shared_config_cannot_bypass_url_guard`, `shared_origin_change_cannot_redirect_cleanup`, `remote_only_merge_objects_exist_before_cleanup_mutates_worktrees` |
| Defer capacity-limited dispatch, block failed launches, and continue independent queue actions | `7dbe8c7` | `dispatch_defers_queued_and_revising_before_capacity_transition`, `dispatch_blocks_launch_failed_tickets_and_continues_queue`, `dispatch_blocks_runtime_failure_and_starts_later_ticket`, `dispatch_blocks_capacity_race_and_reports_later_action` |

## Live-smoke and final-review corrections

| Fix | Commit | Regression evidence |
| --- | --- | --- |
| Accept silent acknowledgements and plain terminal text from installed Herdr | `f647906` | `start_agent_accepts_silent_run_and_text_wait`, `read_visible_accepts_plain_terminal_text`, `read_visible_accepts_json_envelope`, `send_keys_accepts_empty_acknowledgement`, `structured_request_rejects_malformed_nonempty_response` |
| Explicitly unlock completed agent operations despite concurrent fork inheritance | `c0edd1a` | `agent_operation_unlocks_before_inherited_fd_closes`; deterministic child held before exec, failing before the guard fix |
| Preserve per-agent errors in periodic sweep reports | `f5fcab2` | `periodic_sweep_preserves_agent_errors`; failing before aggregation was restored |

The successful native-agent pipeline used the release binary built from
`f647906`. The subsequent lock-release and error-report aggregation corrections
were verified by deterministic regressions and the complete final suite.

## Validation

At `f5fcab2`:

| Check | Result | Evidence |
| --- | --- | --- |
| `cargo fmt -- --check` | PASS | successful final format check (empty output) |
| `cargo clippy --all-targets -- -D warnings` | PASS | `/tmp/shepherd-final-clippy2.log` |
| `cargo test --locked` | PASS, 396 tests | `/tmp/shepherd-final-tests3.log`; zero failed |
| Tests with `test-support` enabled | PASS, 397 tests | `/tmp/shepherd-final-test-support2.log` |
| All 49 explicitly named Phase 3 tests | PASS | Every required name occurs with `... ok` in the full test log |

Tests were developed through failing-test/implementation cycles. Task 9's
retained focused evidence includes the red/green cadence regression logs
`/tmp/shepherd-task9-red-cadence.log` and
`/tmp/shepherd-task9-green-cadence.log`, plus red/green cases for policy denial,
pane ownership, foreground cwd, pending prompts, provider failures, and
isolation between agents/projects. Final checks supersede intermediate failing
development runs.

### Required named tests

| Task | Source | Names |
| --- | --- | --- |
| 1 | `tests/store_concurrency.rs` | `roundtrip_store`, `atomic_write_survives_partial`, `two_threads_update_no_lost_write`, `project_hash_stable` |
| 2 | `src/ticket/state.rs` | `happy_path_edges`, `illegal_edge_rejected`, `loop_cap_blocks`, `unblock_returns_to_prior`, `cancel_from_any_nonterminal` |
| 3 | `src/roles.rs` | `defaults_match_spec`, `unknown_runner_rejected`, `bad_rule_key_rejected`, `user_role_added`, `builtin_rules_after_user_rules` |
| 4 | `src/harness/mod.rs`, `src/launch.rs` | `codex_args_verbatim`, `fallback_on_provider_failure`, `all_runners_fail_launch_failed`, `model_badge_tokens`, `starting_recorded_before_start` |
| 5 | `src/flock.rs` | `spawn_refuses_at_capacity`, `report_validates_schema`, `report_stores_findings`, `reconcile_adopts_existing`, `reconcile_marks_missing_starting_failed`, `no_duplicate_after_reconcile` |
| 6 | `src/ticket/scheduler.rs`, `src/ticket/intake.rs` | `revising_before_queued`, `long_queue_never_blocks_revising`, `respects_max_concurrent`, `pr_open_frees_slot`, `slug_and_branch`, `integration_branch_auto` |
| 7 | `tests/children.rs` | `integrates_two_children_in_order`, `conflict_aborts_and_keeps_branch_clean`, `refuses_dirty_ticket_worktree`, `child_hook_blocks_ticket_branch_push` |
| 8 | `src/prompts.rs`, `src/lead.rs` | `lead_prompt_contains_rules`, `role_prompt_contains_report_command_and_secrets_rule`, `start_refuses_with_active_ticket`, `init_does_not_touch_herdr_config`, `ping_queued_when_lead_blocked` |
| 9 | `src/supervise/decide.rs` | `codex_trust_auto_answered`, `rule_skipped_outside_worktree`, `policy_screen_denied_even_if_rule_matches`, `unknown_dialog_asks_lead`, `escalates_after_threshold`, `idle_no_report_nudges_once_then_asks_lead`, `stall_nudge_then_lead`, `exit_resumes_once`, `provider_failure_respawns` |

The manifest test `manifest_parses_and_matches_crate` also verifies
`init/start/resume/status` and all three pane event registrations.

### Five Review Focus checks

| Focus | Passing coverage |
| --- | --- |
| Crash between agent launch and saved state | `reconcile_adopts_existing`, `reconcile_marks_missing_starting_failed`, `no_duplicate_after_reconcile`, plus pending-prompt recovery tests |
| Concurrent store/report writes | `two_threads_update_no_lost_write` |
| Child integration conflict | `conflict_aborts_and_keeps_branch_clean`, `cli_conflict_persists_both_ranges_and_blocks_ticket`; clean status and original HEAD restored |
| Policy dialog also matching a safe rule | `policy_screen_denied_even_if_rule_matches`; effect coverage in `policy_denial_blocks_ticket_and_stops_owned_flock` |
| Revising priority without queue starvation | `revising_before_queued`, `long_queue_never_blocks_revising` |

## Task 10 live smoke

PASS: `/tmp/shepherd-smoke-bdds339n/result.json` (`ok: true`).

The script uses a disposable `/tmp` repository, a local bare `origin`, and its
own named herdr server/socket/config/state. It verifies the spec's unchanged
default codex and omp profiles, invokes the plugin start action from the
scratch pane, hands the lead a tiny hello-shell ticket, and checks real role
reports, tabs, badges, final code, and unchanged origin refs. Cleanup is limited
to resources created by that isolated run.

First attempt: `/tmp/shepherd-smoke-dyk4hig0/result.json`.

- Result: FAIL before submitting a ticket. The helper expected JSON from a
  successful `herdr pane run`, which returned empty output.
- Observed ticket states: none; this attempt provides no pipeline evidence.
- Cleanup: PASS, no cleanup errors; scratch workspaces `w1` and `w2` closed,
  owned server exited, origin refs unchanged.
- This first attempt exposed a smoke-helper output-parser failure. Subsequent
  investigation also found that the production Herdr adapter must accept
  successful silent/text acknowledgements, rather than require JSON for every
  command. That runtime compatibility fix was developed test-first and committed
  separately as `f647906`.

Second attempt: `/tmp/shepherd-smoke-uzt7p956/result.json`.

- Result: interrupted with signal 15 while the acknowledgement compatibility
  issue was investigated; no ticket states were observed.
- Cleanup: PASS, no cleanup errors; scratch workspaces `w1` and `w2` closed,
  owned server exited, origin refs unchanged.
- Neither initial attempt is an end-to-end pass.

Runtime acknowledgement compatibility fix: `f647906`.

Final run: `/tmp/shepherd-smoke-bdds339n/result.json`.

- Real ticket `SMOKE-1` advanced through `planning → building → reviewing → qa →
  ready_for_pr`; planner, coder, reviewer, and QA submitted actual completed
  reports. The lead orchestrated all transitions through the public CLI.
- Planner/coder/QA and lead used `codex-sol-high`: `codex --yolo -m gpt-6.1-sol
  -c model_reasoning_effort=high`. Reviewer used `omp-astra`: `omp
  --auto-approve --model openai-codex/gpt-6-astra:medium --smol
  openai-codex/gpt-6-luna:max`. No model/profile substitution was made.
- Verified role tabs and `shepherd_model`/`shepherd_stage` pane badges. The
  supervisor answered the Codex trust dialog and delivered its pending task.
- Coder recorded the required missing-script failure, implemented the tiny
  shell project, and committed `385f541b210de952e40e9ed47c787d6208d5ace3` on
  `feat/smoke-1`. Independent reviewer and QA checked the same commit.
- Final `sh hello.sh` stdout was exactly `hello\n`; `sh test_hello.sh` exited 0.
  Both files were tracked and matched HEAD.
- The local bare origin's HEAD/main stayed at
  `29cebd6e504c1a3b27f885843708a26a96ef3800`. No smoke push was performed.
- Scratch workspaces `w1`, `w2`, and `w3` were confirmed closed;
  `remaining_owned_workspace_ids` was empty. Watcher PID 1035526 was stopped,
  the owned server exited, and `cleanup_errors` was empty.
- All three attempts' evidence remains under their named `/tmp` directories.
  No production Herdr workspace, pane, or agent was modified by the smoke.

Final code validation: formatting, clippy with warnings denied, all 396 default
tests, and all 397 tests with `test-support` passed. During development one
optional run hit `ETXTBSY` in a temporary executable fixture; a fresh complete
run passed. A separate readiness-test failure led to the deterministic inherited
file-lock regression and committed correction above; no assertion was weakened.

## Public names for Phase 4

Use these actual interfaces rather than names inferred from plan Consumes
blocks:

- `roles::{Resolved, ShepherdSettings, Role, Runner, WorktreeMode,
  SupervisionSettings, Rule}`; `roles::resolve(&Config)` returns `Resolved`.
  `Resolved.roles` is `Vec<Role>` and `Resolved.runners` is
  `IndexMap<String, Runner>`.
- `prompts::lead_prompt(bin, project_root, resolved, data) -> String` and
  `prompts::role_prompt(bin, ticket, role, task, agent_name, harness) -> String`.
- `lead::ping_lead(h, store, text)` queues unavailable delivery;
  `lead::flush_pings(h, store) -> Result<usize, ShepherdError>` drains queued
  pings. Lifecycle functions are `lead::{start,resume,status,init,init_with_state}`
  and `LeadContext { bin, state_dir, workspace_id }`.
- `ticket::scheduler::next_actions(&StoreData, &ShepherdSettings) -> Vec<Action>`
  with `Action::{StartTicket(String), ResumeRevising(String)}`. Effectful
  dispatch is in the binary's `cli_ticket::dispatch`; the library scheduler is
  pure.
- `ticket::intake::{add,cancel,integration_branch,start_ticket}`.
  `StartTicketOptions { state_dir, config_dir, bin, settings, now }` supplies
  startup configuration. `start_ticket` returns the prepared `Ticket`; the
  orchestration caller launches the planner.
- `launch::launch_configured(h, store, LaunchRequest, cfg)` honors runtime
  settings. `launch::launch` uses defaults. `LaunchRequest` fields are `ticket`,
  `role`, `runners`, `worktree`, `workspace_id`, `tab_label`, `prompt`, `state_dir`.
  Naming, badges, and timestamps use `launch::{agent_name,model_badge,now}`.
- `harness::{Harness,harness_for}`. `Harness` additionally provides
  `resume_args(runner, session_ref)`.
- `flock::{spawn,report,validate_report,store_report,cancel,reconcile}`;
  recovered startup delivery uses `flock::deliver_pending_prompt`.
- `ticket::children::{create_child,create_child_configured,integrate,
  conflict_ranges,cleanup_integrated}`. `ChildWorktree` remains a store model;
  `IntegrateReport` fields are `units`, `ranges`, `head`.
- `supervise::{Condition,Decision,decide,observe,ApplyContext,apply,
  supervise_project,sweep_projects,handle_pane_event}`. `observe` returns
  `Result<Option<Condition>, ShepherdError>`. `ApplyContext` fields are `h`,
  `store`, `cfg`, `state_dir`, `now_s`. `SweepReport` fields are `projects`,
  `observed`, `acted`, `errors`.
- `ticket::state::transition_with_limit(ticket, to, reason, now, cap)` accepts
  the configured revision cap. `transition` uses the default cap. The existing
  state machine already includes `TicketState::CiWait`; QA is `TicketState::Qa`.
- `gate::env::agent_env`, `gate::hook::install`, `hooks::emit`, `metadata::token`,
  and `Notifier::new(h, enabled).send(title, body)` retain their Phase 2 names.

Store fields relevant to Phase 4:

| Model | Fields |
| --- | --- |
| `StoreData` | `schema_version`, `project_root`, `trust: Option<ProjectTrust>`, `lead`, `tickets`, `agents`, `pending_pings` |
| `LeadRecord` | `agent_name`, `tab_id`, `pane_id`, `started_at` |
| `Ticket` | `id`, `slug`, `title`, `body`, `acceptance`, `branch`, `base`, `state`, `blocked_reason`, `worktree`, `workspace_id`, `plan`, `research`, `revision_round`, `findings`, `pr_number`, `children`, `integration_ranges`, `history`, `created_at`, `updated_at`, `url` |
| `AgentRecord` | `name`, `ticket`, `role`, `runner`, `status`, `pane_id`, `tab_id`, `worktree`, `task`, `report`, `observed`, `last_progress_at`, `nudges`, `restarts`, `prompt_pending` |
| `ProjectTrust` | `project_root`, `common_git_dir`, `origin_url`, `push_url`, `default_branch`, `config_commit`, `config_blob`, `config_text`, `trusted_at`, `global_config`, `object_format` |

Model types are re-exported by `store`; callers should use
`Store::{open,read,update,root}` instead of manipulating state files. Timestamp
strings produced by `launch::now()` contain Unix seconds. The `observed` field
stores the supervisor's serialized progress observation.

## Deviations and execution boundaries

- The role resolver is in `src/roles.rs`, runners use `IndexMap`, and configured
  caps go through `transition_with_limit`, as required by the actual core API.
- Core included Phase 4 pieces (`ci_wait`, PR text/attribution, ticket source).
  Phase 3 orchestration still stops at `ready_for_pr`.
- The public CLI additionally exposes `ticket dispatch` and `ticket transition`
  so the lead can execute the complete pipeline without editing state files.
- Runtime project config and transport identity use a trusted snapshot;
  `config trust` explicitly refreshes it and reports changes. This extends the
  original config plan to cover shared-ref/config mutation risks.
- The original `observe -> Option<Condition>` plan signature became a fallible
  `Result<Option<Condition>, ShepherdError>` so adapter errors remain visible.
- Child integration rolls back the complete transaction on conflict; cleanup
  happens only after successful integration and proof of ownership/containment.
- The lead never edits code. Planner/researcher report summaries persist into
  ticket plan/research fields. Startup tasks remain durable until delivery.
- Dedicated task commits are supplemented by the separately requested fixes
  above; the history therefore contains more commits than the seven Phase 3
  task commits alone.
- Independent read-only plan/API research and independent module/test work were
  delegated in parallel. Independent checks ran concurrently when their data
  and output directories allowed it.
- Merge resolution and lockfile generation, shared Herdr/CLI wiring, conflict
  integration, final full-tree validation, staging, and commits stayed serial.
  A full-diff review and targeted regression fixes preceded final Task 9 checks.
- No real remote was pushed, no real PR was opened or commented on, and the
  scratch smoke is the only authorized live shepherd project. The legacy
  regroup/prune scripts and the user's herdr config remain outside this change.

Task 10 is the commit containing this report and `scripts/smoke.sh`, titled
`test(smoke): add end-to-end smoke script`. Final branch: `feat/shepherd-phase3`;
all requested work is committed. The final response records its commit hash.
