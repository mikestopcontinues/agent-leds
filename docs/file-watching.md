# Plan: sessions from the harnesses' own files

## Outcome and acceptance

Before this change the daemon learned what each session was doing from hooks, and filled the gaps hooks left with two timers: a working session silent for 30 minutes was dropped, and a Claude Code session whose process had been idle for 5 seconds counted as interrupted. The change makes each harness's own files the primary source for all seven harnesses, keeps a hook only for a moment no file records, and drops both timers. Session state lives in one explicit finite state machine whose transition table is data, its rows in priority order: a source reports each fact a record establishes, and the first row that matches one of them decides. Facts told together that share no declared family overlap; they are resolved the same way, counted in `status` and reported by `doctor`.

It is done when:

- each harness's sessions are followed from its files, with only the hooks listed below still installed, and `agent-leds install` removes every other registration of ours;
- the transition table, each harness's declared signals and the channel each comes through (file or hook) are code, and `docs/coverage.md` is generated from them and checked by a test. It lists, per harness, the transitions it cannot make and the phases it can only leave through dismissal or expiry. That document is the evaluation of what is missing;
- every declared signal is backed by a fixture taken from a real session (or marked inferred when no run was possible), and a source that produces an undeclared signal fails its tests and is counted by `doctor`;
- real sessions in each installed harness move through the lights as the fixtures say.

## Constraints and dependencies

- No guessing: no timers or heuristics that infer state. A process's liveness counts as read from files when the harness itself writes the pid, lock or marker. The daemon never scans the process table. A guess may return later as a named signal with its own channel, so the coverage document keeps telling evidence from inference.
- No backward compatibility: the saved queue resets on upgrade, obsolete hook registrations are removed by install, and replaced code is deleted.
- Visible behaviour that is not about detection stays: queue order, weak and blocked questions, reading through the desktop app, the announce animation, `keep_read_minutes`, `abandon_days`, dismiss, status and doctor.
- New dependencies: `notify` (FSEvents) for watching, and `rusqlite` against the system SQLite for OpenCode's database.

What each harness's files record was established with real sessions on 2026-11-14 (Claude Code 2.1.325, Codex 0.153.2, Grok 1.0.77, Cursor 2026.11.13, OpenCode 1.19.5, Pi 0.84.4 and 1.5.2). Junie is not installed here, so its source rests on third-party captures and is marked inferred. The hooks that remain:

| Harness | Kept | Why no file serves |
| --- | --- | --- |
| Claude Code | none | The session file and transcript record every moment. |
| Codex | `PermissionRequest` | Nothing is written while an approval waits. |
| Grok | none | Its event log records ends, failures and cancels, and `plan_mode.json` a plan awaiting approval. |
| Cursor | `beforeSubmitPrompt`, `stop` | Prompts given as arguments and text-only turns write nothing until the turn ends, and some interrupts, and a failure after one the same, leave the transcript as it was. The prompt hook also binds the session to its process. |
| Junie | none | Its event log records waits, ends, failures and cancels. |
| OpenCode | plugin: busy with pid, `permission.asked`, `permission.replied`, `session.error` | A pending permission looks like a running tool; errors before a turn starts and the process are not recorded. |
| Pi | extension: `session_start`, `agent_start`, dialogs, `agent_settled`, `session_shutdown` | No liveness or dialog state is written, and 0.84 writes no file before the first reply. |

## Phases

1. **Core and Claude Code.** The state machine and its table, the queue that applies observations, liveness checks, file reading (tails, whole files, start-up seeding from the end of each file), the FSEvents watcher in the daemon loop, the coverage document, and Claude Code read from its files with its hooks removed. The other harnesses keep their hooks, translated into the same signals, until their own phase.
2. **One phase per other harness:** Codex, Grok, Cursor, Junie, OpenCode, Pi. Each adds its source and fixtures, reduces its hooks to the table above, and regenerates the coverage document. They are independent of each other.
3. **Doctor and documentation.** Doctor reads each harness's files with the daemon's own readers. `docs/design.md`, `docs/harnesses.md` and the README describe the new model.
4. **Evaluation.** Read `docs/coverage.md` and decide, per gap, whether to run an experiment, accept it, or add a deliberate guess.

## Verification

- Unit tests over the table (every row can win, its order decides, complete, `Gone` left only by a start), the queue, the readers and each source's fixtures, including a rescan after every step yielding nothing and a seed giving the same phase as replaying the records.
- One real FSEvents test, since CI runs on macOS.
- Real sessions per harness after install, watching `agent-leds status` through prompt, approval, question, end, error, interrupt, quit and kill.
- Harness configs diffed before and after install and uninstall.

## Unresolved choices

These need a real session to settle, and each would change a declared signal:

- Junie's whole file format, which needs Junie installed.
- Whether turns that retry after a failed request should show as something other than working.

Whether a permission prompt in the Claude desktop app sets the session file's `waiting` status, as the CLI's does, is settled as far as a run without the app can settle it: the stream-json permission prompt the app answers, recorded from `claude -p` with the app's entrypoint, sets it (`tests/fixtures/claude/a_desktop_permission_prompt_shows_a_question.jsonl`, marked inferred), so Claude Code keeps no hook.
