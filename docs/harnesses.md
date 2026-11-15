# Harnesses

What each agent harness reports to the service and where it falls short. [design.md](design.md) covers what the service does with the reports.

The service reads every harness's own files as they change. Claude Code's and Junie's files say all it needs, so it registers no hooks with them. With Codex, Grok and Cursor, `agent-leds install` registers `agent-leds hook <harness>` for the one or two events listed below that their files do not record; OpenCode and Pi load a plugin of ours that runs it for what their files leave out. The hook passes the event to the daemon with the processes it runs under, prints nothing and always succeeds. So it never holds up a turn and never answers a permission request. Where a harness takes a hook's silence for an answer, that event is not hooked. Whatever the hooks say, a session leaves when the process running it ends.

[coverage.md](coverage.md) lists every signal each harness gives, how each is known, and the moves each cannot make.

## Claude Code

Each running Claude Code process, in a terminal or the desktop app, keeps a file in `~/.claude/sessions/` with its session and whether it is busy, idle or waiting on the user, and each session writes its transcript to `~/.claude/projects/`. The daemon reads both as they change.

| What Claude Code writes | Session becomes |
| --- | --- |
| A prompt in the transcript | working |
| The session file waiting on a permission | question |
| The session file waiting for input needed | question |
| The end of a turn whose last line ends in a question mark | question, as a weak signal |
| The end of a turn while the session file stays busy | working, in the background |
| The end of a turn otherwise, or a background turn going idle | stopped |
| An API error in the transcript | error |
| An interrupt in the transcript | removed |
| The session file gone or naming another session, or its process gone | removed |

A turn that ends with a subagent or a workflow running in the background is not the session's last word: Claude Code keeps the session file busy until that work is done, so the session stays green until it goes idle, and then it shows as stopped. The files are undocumented; the fixtures in `tests/fixtures/claude/`, recorded from real sessions, pin what the daemon expects of them. A session never seen running, such as one run with `claude -p`, is not shown.

## Codex

A Codex process holds a lock on `~/.codex/thread-writer-locks/<thread>.lock` for each thread it runs, and appends the thread's rollout, `~/.codex/sessions/YYYY/MM/DD/rollout-<time>-<thread>.jsonl`, a record at a time. The daemon follows a thread while its lock is held: the lock deleted is a quit, and a lock left unlocked is a process that was killed. The date folders are listed, never computed, since a resumed thread goes on writing to the rollout in the folder of the day it began. Compressed rollouts and `~/.codex/archived_sessions` are never appended to, and are not read.

| What Codex writes | Session becomes |
| --- | --- |
| `task_started` | working |
| A `request_user_input` call with no output yet | question |
| The question's output | working |
| `task_complete` whose last line ends in a question mark, or that holds a `<proposed_plan>` | question, as a weak signal |
| `task_complete` while a subagent the turn started, or gave a `followup_task`, is still at work | working, in the background |
| `task_complete` otherwise, or the last such subagent completed after the turn ended | stopped |
| `task_complete` with an error | error |
| `turn_aborted`, which a cancelled approval writes too | removed |
| The lock deleted or left unlocked | removed |

Nothing is written while an approval waits, so `agent-leds install` registers the hook for `PermissionRequest` alone, and removes the others an earlier version registered with their trust records. The hook makes the session a question only while its rollout has that turn open; the next tool output or completed item in the rollout answers it. Codex runs a hook only once its hash is recorded under `[hooks.state]`, so the install asks `codex app-server` for the hash and records it, which needs `codex` on the `PATH`. Codex keys that record by the hook's place among the groups of its event, so a group of ours keeps its place, and if another tool reorders the groups, run `agent-leds install` again.

A thread whose `session_meta` names a subagent, by its `source` or its `thread_source`, is never followed: its parent's rollout says when it starts and completes, and the copy of the parent's history a forked subagent's rollout opens with is never read. That leaves out the agent that consolidates Codex's memories too. A thread the desktop app opened for an agent (`thread_source` `agent_created_thread`) is followed like the user's own. When the daemon starts, and when it first sees a thread's lock, it reads the thread's rollout back to its last `task_started`, at most 16 MiB, and only what was written since the lock was taken, so a resumed thread does not replay its history and a prompt written before the lock is seen still counts. The files are undocumented; the fixtures in `tests/fixtures/codex/`, recorded from Codex 0.153.2 and the desktop app's 0.162 rollouts, pin what the daemon expects of them. The gaps:

- A thread opened in the terminal shows only once its first prompt creates its rollout.
- An approval a subagent asks for is not seen: it is not known whether its hook names the subagent's thread or its parent's, and the subagent's thread is not followed.
- `request_user_input_async`, the desktop app's question, does not stop the agent, so the session stays working while it waits for the answer.
- A failing request that Codex retries leaves no record, so the session stays working until the turn ends.
- A subagent that never reports completing keeps its parent in the background until the next prompt.
- A subagent started or given its task in an earlier turn does not keep its parent in the background: when the turn ends the session shows stopped while the subagent works. A `send_message` to a subagent starts no turn of its own, so it does not count either.
- A plan proposal is recognised by its `<proposed_plan>` block, which no rollout has shown yet.
- Codex's desktop app was not running during the probe, so whether it holds the thread locks the same way is not confirmed.

## Grok

`~/.grok/active_sessions.json` lists each running Grok session with its process's pid and the microsecond that process started. Each session keeps a folder, `~/.grok/sessions/<cwd folder>/<session>/`, with two logs: `events.jsonl` says when a turn starts, waits and ends, and `updates.jsonl` holds what the agent wrote and the subagents it started. The daemon reads all three as they change. The events decide every move; the updates are read to their end when a turn ends, for its last message and the subagents still running. One hook remains, `StopFailure` in `~/.grok/hooks/agent-leds.json`, because no file was seen to record a turn that ends on an API error.

| What Grok writes | Session becomes |
| --- | --- |
| A row in `active_sessions.json` whose process runs | tracked |
| `turn_started` in the events, typed or after a background subagent finishes | working |
| `permission_requested` still unresolved 250 ms after it was written | question |
| `tool_started` for `ask_user_question` | question |
| `permission_resolved` after such a prompt, or `tool_completed` for `ask_user_question` | working |
| `turn_ended` completed, whose last message's last line ends in a question mark | question, as a weak signal |
| `turn_ended` while the updates hold a subagent spawned and not finished | working, in the background |
| `turn_ended` otherwise, including a cancel Grok made itself such as `max_turns` | stopped |
| The `StopFailure` hook, for the newest turn | error |
| `turn_ended` cancelled by Ctrl-C, the stop button or the dashboard, or by rejecting or cancelling a permission | removed |
| The row gone from `active_sessions.json`, as `/quit` leaves it even mid-turn, or its process gone | removed |

Every tool, read-only ones too, writes a permission request, and Grok writes each request and its answer as separate lines. One Grok approves by itself is answered within 7 ms; one the user answers waits seconds. So a request counts only once it has gone 250 ms unanswered, and the light turns to question that much later. The question tool's own approval is never a wait. A row left by a process that was killed stays in the registry until another session starts, so a row counts only while its pid runs with the start it records. A subagent runs in a session folder of its own, beside its parent's, under an id the registry never lists, so it is never followed; the parent shows its work. The files are undocumented; the fixtures in `tests/fixtures/grok/`, recorded from Grok 1.0.77, pin what the daemon expects of them. The gaps:

- A turn that fails is known only from the hook. A daemon restarted after it keeps showing the error until the turn's end is written, and then shows stopped; one that was not running when the hook came shows what the files say. A request that cannot connect retries without end and never ends the turn, so the session stays green.
- Grok sends `StopFailure` off the turn, so the hook can arrive after the turn's end is written, which shows stopped and then error, or after the next turn has started. The hook names its turn's `promptId`, and so do the agent's records in `updates.jsonl`, so a report on a turn the next one has replaced is dropped. A turn that fails before the agent writes anything has no such record: if the next turn has not written anything either when the report arrives, the next turn shows error until it writes something, and its own end still counts.
- A run with `grok -p` gets no row, so it is not shown.
- A subagent that finishes after its parent's turn without waking it should show the session as stopped. That was never seen: the one that finished after its turn woke its parent, which started a turn, and the ones that did not wake it finished inside the turn.
- Plan approval, the leader process (`use_leader`) and several windows on one session have not been tried.

## Cursor

The daemon reads two things Cursor's CLI writes. Each chat's transcript, `~/.cursor/projects/<slug>/agent-transcripts/<chat>/<chat>.jsonl`, is written at the checkpoints of a turn. Each running process marks itself in `~/.local/share/cursor-agent/versions/<version>/.running/<pid>` with the name it was run by and the second it started, such as `cursor-agent|Sat Nov 14 21:54:52 2026`, or `agent|…` when run as `agent`. The CLI unlinks its marker when it quits. Its node worker, a child of the CLI, writes a `node|…` marker that outlives it. A marker names no chat, so a hook stays: `beforeSubmitPrompt`, registered in `~/.cursor/hooks.json`, binds the chat to the CLI's process. Of the hook's ancestors whose marker matches the second they started, that is the nearest whose parent has no such marker, so the worker is passed over whatever the processes are called. The hook also says that the turn started. A second hook, `stop`, is read only for its status `aborted`, because some interrupts write nothing.

| What Cursor writes | Session becomes |
| --- | --- |
| `beforeSubmitPrompt`, under a marked process | working |
| A transcript ending in a user or assistant record, after the turn's end or with more user records | working |
| A transcript ending in `turn_ended` with status `success` | stopped, or question as a weak signal when the last assistant text ends in a question mark |
| `turn_ended` with status `error` | error |
| `turn_ended` with status `aborted` | removed |
| `stop` with status `aborted`, for a bound chat | removed |
| The bound process's marker unlinked, or the process gone | removed |

The transcript is not appended to only. The first checkpoint of every turn truncates the file in place and writes it again whole, without the earlier turns' `turn_ended` records. So the daemon reads the whole file each time it changes and takes a read only if it is unchanged across the read, ends in a newline, and is not the start of the read taken before. A read caught in the middle of a rewrite is the start of that earlier read, so it is skipped. Only the last record and the count of user records matter, so rewritten history changes nothing. An interrupt before the turn's first checkpoint rewrites the previous turn's `success` as `aborted`; that reads as an interrupt. When the previous turn had already ended `aborted`, the rewrite leaves the file as it was, so only the `stop` hook tells of the interrupt. A `stop` that completes or fails is ignored, since the transcript says the same. Subagent transcripts, in the chat's `subagents/` folder, are never read. The files are undocumented; the fixtures in `tests/fixtures/cursor/`, recorded from Cursor 2026.11.13, pin what the daemon expects of them.

The gaps:

- No question state. Nothing is written while a tool waits for approval, and the hooks that run before a tool fire for approved tools too, so a session waiting for approval stays green. The `AskQuestion` dialog was never offered to the model, so what it writes is unknown.
- An error ending a turn is read from Cursor's code, not from a real session, as is the `stop` hook's status `aborted`.
- Cursor writes a reply's reasoning after its text, in the same block. With a reasoning model, the question test reads the reasoning's last line instead of the reply's, so a question can show as stopped and a statement as a question. The probe used the Auto model, which wrote no reasoning.
- A chat no hook bound names no process, so it does not show. That includes every chat already running when the daemon first starts with this version: it shows from its next prompt.
- After `/new`, the old chat stays bound to the process until it exits.
- A process killed with `kill -9` leaves its marker. The daemon sees that the process is gone at its next liveness check, within seconds.

## Junie

Junie keeps its folder at `$JUNIE_HOME`, or at `~/.junie` in the account's home directory, which it takes from the system and never from `$HOME`; the daemon finds it the same way. Each Junie process writes a sidecar to `processes/` naming the session it takes, and each session appends its events to `sessions/<session>/events.jsonl`, which its first prompt creates. The daemon reads both as they change, and registers no hooks: an install removes the ones an older agent-leds registered in `~/.junie/config.json`, with the file their hook kept, `~/.local/state/agent-leds/junie-session.json`.

| What Junie writes | Session becomes |
| --- | --- |
| `UserPromptEvent`, or `TaskStartedEvent` when a task resumes | working |
| A block waiting in state `INPUT_REQUIRED` with an `approvalRequest` | question |
| An `AskAsyncRequestUpdatedEvent` or `ChoiceRequestUpdatedEvent` waiting in state `INPUT_REQUIRED` | question |
| The waiting step written again as `COMPLETED` or outside `INPUT_REQUIRED`, or the user's response | working |
| `TaskState FAILED`, or `TaskState` after an `AgentFailureEvent` or `AgentTaskFailedEvent` | error |
| `TaskState` after a `CancelAgentEvent` | removed |
| `TaskState` whose result's last line ends in a question mark | question, as a weak signal |
| `TaskState` otherwise | stopped |
| A newer sidecar for the pid naming another session, the sidecar deleted, or its process gone | removed |

Junie never removes a sidecar, and one process takes session after session (the session changes once the trust screen is answered), so the sidecar with the newest `startedAt` for a pid names the session it runs, and a sidecar counts only while a process with its pid started no later than its `startedAt`. Junie writes one `TaskState` for each task, `COMPLETED` even after a failure, so any `TaskState` ends the task, and how it ended is read from what the task wrote before it; and it writes every block of a task again as `COMPLETED` when the task ends, which answers nothing and asks nothing. Steps are told apart by `stepId` and ordered by their place in the file, since `timestampMs` goes backwards. An events log has reached hundreds of gigabytes, as Junie writes terminal output into it, so a log is only ever read from its end, or from its start when it began after its session did. A session a process takes while the daemon runs begins idle, and of a log it resumes only what that process wrote counts: only a daemon starting reads a log back to where its last task began. The session's folder comes from the sidecar's `projectPath`, or from a later `CurrentDirectoryUpdatedEvent` when that is empty.

The gaps:

- Junie was not installed when the files were studied. Every record is taken from captures of Junie CLI 26.8.24 that others published, and the fixtures in `tests/fixtures/junie/` are those captures with the text scrubbed, so every signal is inferred until a real session confirms it.
- A daemon started after a resume, before the session writes again, shows where the log left the session, which may be a wait or a task a killed process never finished.
- What a denied permission writes, whether Esc and Ctrl-C both write `CancelAgentEvent`, whether a resume appends to the same log, and what a subagent writes in `subagents/` and `task-*/` are not known. Those logs are not read.
- No hook is registered for `PermissionRequest` or `PreToolUse`, ever: Junie reads a hook that succeeds there as approving the request or allowing the tool.

## OpenCode

Every OpenCode process, the terminal interface, `opencode serve`, `opencode run` and the desktop app alike, writes its sessions to one SQLite database, `~/.local/share/opencode/opencode.db`. The daemon keeps one read-only connection to it. When the database or its write-ahead log changes and `PRAGMA data_version` says another process committed, it reads, in one short transaction, the last event of each session it follows and of that session's subagents, from the event log OpenCode 1.18 and later write, and, for a session whose events grew, its latest message and that message's parts. It never reads the account or credential tables.

The database names no process and records no permission request, so the install also writes a plugin, `~/.config/opencode/plugins/agent-leds.js`, that runs `agent-leds hook opencode` for what the database leaves out: `busy`, as a session goes busy, with the pid of the OpenCode process; `permission.asked` and `permission.replied`; and `session.error`, only while the session is busy and never for a `ContextOverflowError`, since OpenCode also publishes it for a file it could not read and for an overflow it compacts, and goes on. A session is followed from the first `busy` of a session that is not a subagent's, for as long as that process runs.

| What is read | Session becomes |
| --- | --- |
| The plugin's `busy`, or a new user message as the session's latest | working |
| A `question` tool part running in the turn, in the session or a subagent's | question |
| The plugin's `permission.asked`, for the session or a subagent's | question |
| The question part done, or the plugin's `permission.replied` | working |
| The latest assistant message completed, other than a step that finished on `tool-calls` or `unknown` with no error and no tool the user refused or dismissed | stopped |
| The same, its last text part ending in a question mark | question, as a weak signal |
| An error on that message, or the plugin's `session.error` | error |
| A `MessageAbortedError`, or the message completed with neither finish nor error | removed |
| The session's row deleted or archived, or the OpenCode process gone | removed |

The rule is OpenCode's own: its loop goes on after a step that finished on `tool-calls` or `unknown`, however its tools fared, and writes the next step's message only after it has read the session's messages again, so such a step can be the latest message for a while. A turn the user rejected a permission in, or dismissed a question in, ends on `tool-calls` with that tool errored and nothing after it, which counts as stopped; a permission rejected with feedback lets the turn go on. A subagent runs in a session of its own whose `parent_id` names the session that started it; its questions and permissions hold that session up, and its own runs say nothing more of it. Each turn's end is told once, so the title OpenCode writes after the first turn does not announce it again. The database is undocumented; the fixtures in `tests/fixtures/opencode/`, taken from real sessions, pin what the daemon expects of it.

The gaps:

- A pending permission is in the database only as a running tool, so only the plugin says it waits. A daemon that was not running when it was asked shows the session working until it moves on.
- An error before a turn's first step, such as an unknown model, is in the database at most as a prompt with no reply. OpenCode 1.19.5 goes busy before it looks the model up, so the plugin's `session.error` shows it; 1.17.3 failed that way without ever going busy. A prompt that fails before the session goes busy, for an unknown agent or command, writes nothing and is not shown.
- A turn waiting out a provider's retries shows as working, since nothing is written while it waits.
- A step that overflowed the context and is compacted automatically is written as a step stopped with neither finish nor error, as an abort while retrying is, and its compaction is written just after. A read between the two shows the session idle for that moment.
- With `experimental.continue_loop_on_deny` set, a rejected permission lets the turn go on; the session shows stopped until its next step is written.
- A session whose OpenCode process started before the plugin was installed is not shown until that process is restarted.
- A database from OpenCode before 1.18, which has no event log, is not read; the daemon says so in its log.
- Checked against OpenCode 1.19.5 through `opencode serve` and the terminal interface, with permissions, questions, aborts, a subagent, errors, an archive, a deletion and a kill, and 1.18.25 through `opencode run`. The desktop app, background subagents and compaction were not tried.

## Pi

Pi writes each session to a file, `~/.pi/agent/sessions/--<cwd>--/<time>_<session>.jsonl`, appending an entry as each message ends, and the daemon reads it as it grows. No file says which process runs a session, when an extension's dialog holds a run up, or whether an error is final, since Pi retries some, so the install also writes an extension, `~/.pi/agent/extensions/agent-leds.ts`, that passes those moments to `agent-leds hook pi`. Its `session_start` and `agent_start` name Pi's own pid and the session's file, which binds the session to its process and tells the daemon where to read, a folder set by `sessionDir` or `--session-dir` included. The extension reports only a Pi with a UI, in its terminal interface or RPC mode, as an editor integration runs it. Print and JSON modes have no one waiting on them, and pi-subagents runs its children in them, so their files, a level deeper than a cwd's folder, are never read.

| What Pi writes or the extension tells | Session becomes |
| --- | --- |
| `agent_start`, a prompt in the file, or a reply after the run ended, as when a background subagent's report wakes Pi | working |
| `ui_prompt_start` during a run: an extension's dialog holds it up | question |
| `ui_prompt_end`, or the next entry after the dialog | working |
| An assistant entry that ends the turn on a line ending in a question mark | question, as a weak signal |
| An assistant entry that ends the turn otherwise | stopped |
| `agent_settled` after an error | error |
| An assistant entry the user stopped, or `agent_settled` after a stop | removed |
| `session_shutdown` other than a reload, or the Pi process gone | removed |

An error entry in the file settles nothing, because Pi may retry it; only `agent_settled`, which comes once nothing will run by itself, says the turn failed. Pi 0.84.4 writes a stop during a tool as an error with the abort's text, so an entry the user stopped is told by its text, never by a provider's error that only mentions an abort. 0.84.4 also writes a new session's first prompt to no file until the reply ends, so `agent_start` says the turn runs. A background subagent's report is written whether or not it wakes Pi, so only a reply after it says a run began. A fork or clone copies the turns before it into its new file, and those are never read as new; a session resumed, in a new Pi or with `/resume` in the same one, starts where its file ends.

What it cannot see:

- A dialog answered while the daemon was down still shows as a question when it starts, until that turn ends, since nothing written says the dialog closed.
- A daemon started while a session writes to its own `sessionDir` cannot find the file, so the session shows nothing until its next prompt names the file again.
- The hook does not wait for the daemon, which reads the file before each hook it handles. A reply written before the daemon handles that run's `agent_start` would leave the session working until its next event; no real provider answers that fast.
- Pi has no permission prompts of its own; extensions that add them ask through dialogs, which show as a question. A session Pi keeps no file of, as with `--no-session`, is not shown.

Checked against Pi 0.84.4 and 1.5.2 in RPC mode, with this extension installed. Pi Durable, Pi's library for long-running agent apps, is a framework rather than a harness and is not covered.
