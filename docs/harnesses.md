# Harnesses

What each agent harness reports to the service and where it falls short. [design.md](design.md) covers what the service does with the reports.

The service reads Claude Code's own files, and registers no hooks with it, and reads OpenCode's database beside the plugin of ours that tells what the database leaves out. With each other harness, `agent-leds install` registers `agent-leds hook <harness>` for the events listed below; OpenCode and Pi load a plugin of ours that runs it. The hook passes the event to the daemon with the processes it runs under, prints nothing and always succeeds. So it never holds up a turn and never answers a permission request. Where a harness takes a hook's silence for an answer, that event is not hooked. Whatever the hooks say, a session leaves when the process running it ends.

The harnesses took their hook events from Claude Code, so most of the names agree. Each harness reads only its own spelling. [coverage.md](coverage.md) lists every signal each harness gives, how each is known, and the moves each cannot make.

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

Hooks are `[[hooks.<Event>]]` groups in `~/.codex/config.toml`, after the groups that are there. Codex runs a hook only once its hash is recorded under `[hooks.state]`, so the install asks `codex app-server` for the hash of each and records it. That needs `codex` on the `PATH`. Codex finds a hook's hash by the hook's place among the groups of its event, so if another tool reorders the groups, run `agent-leds install` again.

| Hook event | Session becomes |
| --- | --- |
| `UserPromptSubmit`, `PreToolUse`, `PostToolUse` | working |
| `PermissionRequest`, `PreToolUse` for `request_user_input` or `request_user_input_async` | question |
| `Stop` whose last line ends in a question mark, or that is a `<proposed_plan>` to review | question, as a weak signal |
| `Stop` otherwise | stopped |
| `SessionEnd`, `Interrupt` | removed |

A Codex subagent runs no hooks. The session that starts one reports its `spawn_agent` and `wait_agent` calls, so it shows as working while it waits; one it leaves running in the background after its turn ends goes unseen. Codex also runs agents of its own, such as the one that consolidates its memories, as sessions it keeps no transcript of. Those are ignored, as is any session Codex keeps no transcript of. The gaps:

- Codex has no event for a turn that fails, so there is no error state. A session whose request fails stays green until its next prompt, or until its process ends.
- `request_user_input_async` does not stop the agent. The session is orange until the agent's next tool call, not until you answer.
- A plan is recognized only when Codex passes it as the turn's last message.

## Grok

Hooks are a file of their own, `~/.grok/hooks/agent-leds.json`.

| Hook event | Session becomes |
| --- | --- |
| `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `PostToolUseFailure` | working |
| `Notification` of a `permission_prompt`, `PreToolUse` for `ask_user_question` | question |
| `Stop` whose last line ends in a question mark | question, as a weak signal |
| `Stop` otherwise, or `StopCancelled` when Grok gave the turn up by itself | stopped |
| `StopFailure` | error |
| `SessionEnd`, or `StopCancelled` by the user | removed |

Grok fires `Stop` again when the session closes; only the one with reason `end_turn` counts. A subagent reports as a session of its own, with nothing that names the session that started it, so its events are ignored. The gaps:

- Grok reports a cancelled turn off to the side, so the report can arrive after the next prompt and remove a session that is working again. Its next tool call brings it back.
- A turn that reports nothing, such as one you rewind, stays green until the next prompt, or until its process ends.

## Cursor

Hooks are commands in `~/.cursor/hooks.json`.

| Hook event | Session becomes |
| --- | --- |
| `beforeSubmitPrompt`, `postToolUse`, `postToolUseFailure` | working |
| `stop` with status `completed` | stopped |
| `stop` with status `error` | error |
| `sessionEnd`, `stop` with status `aborted`, or a tool aborted by an interrupt | removed |

The gaps:

- No question state. Cursor expects the hooks that run before a tool to say whether the tool may run, so those are not hooked, and it has no event for a permission request. `stop` does not carry the agent's last message.
- A session waiting for approval of a tool stays green.
- Run without its interface (`cursor-agent -p`), Cursor sends neither `beforeSubmitPrompt` nor `stop`. Such a run shows as working from its first tool and is removed when it ends.

## Junie

Hooks are groups in `~/.junie/config.json`.

| Hook event | Session becomes |
| --- | --- |
| `UserPromptSubmit`, `PostToolUse` | working |
| `Stop` whose last line ends in a question mark | question, as a weak signal |
| `Stop` otherwise | stopped |
| `StopFailure` | error |
| `SessionEnd` | removed |

The gaps:

- No strong question. Junie reads a hook that succeeds on `PermissionRequest` as approval, and one that says nothing on `PreToolUse` as allowing the tool, so neither is hooked.
- Junie names the session only in `UserPromptSubmit`. The hook keeps the last one in `~/.local/state/agent-leds/junie-session.json` and takes every other event to belong to it. With two Junie sessions at once, an event goes to the one that submitted a prompt last.

## OpenCode

Every OpenCode process, the terminal interface, `opencode serve`, `opencode run` and the desktop app alike, writes its sessions to one SQLite database, `~/.local/share/opencode/opencode.db`. The daemon keeps one read-only connection to it. When the database or its write-ahead log changes and `PRAGMA data_version` says another process committed, it reads, in one short transaction, the last event of each session it follows and of that session's subagents, from the event log OpenCode 1.18 and later write, and, for a session whose events grew, its latest message and that message's parts. It never reads the account or credential tables.

The database names no process and records no permission request, so the install also writes a plugin, `~/.config/opencode/plugins/agent-leds.js`, that runs `agent-leds hook opencode` for what the database leaves out: `busy`, as a session goes busy, with the pid of the OpenCode process; `permission.asked` and `permission.replied`; and `session.error`. A session is followed from the first `busy` of a session that is not a subagent's, for as long as that process runs.

| What is read | Session becomes |
| --- | --- |
| The plugin's `busy`, or a new user message as the session's latest | working |
| A `question` tool part running in the turn, in the session or a subagent's | question |
| The plugin's `permission.asked`, for the session or a subagent's | question |
| The question part done, or the plugin's `permission.replied` | working |
| The latest assistant message completed, other than a step that ended on `tool-calls` with no tool errored | stopped |
| The same, its last text part ending in a question mark | question, as a weak signal |
| An error on that message, or the plugin's `session.error` | error |
| A `MessageAbortedError`, or the message completed with neither finish nor error | removed |
| The session's row deleted or archived, or the OpenCode process gone | removed |

A turn the user rejected a permission in, or dismissed a question in, ends on `tool-calls` with the tool errored and nothing after it, which counts as stopped. A subagent runs in a session of its own whose `parent_id` names the session that started it; its questions and permissions hold that session up, and its own runs say nothing more of it. Each turn's end is told once, so the title OpenCode writes after the first turn does not announce it again. The database is undocumented; the fixtures in `tests/fixtures/opencode/`, taken from real sessions, pin what the daemon expects of it.

The gaps:

- A pending permission is in the database only as a running tool, so only the plugin says it waits. A daemon restarted while one waits shows the session working until it moves on.
- An error before a turn's loop starts, such as an unknown model, is in the database at most as a prompt with no reply, and the plugin's `session.error` shows it only on a session already followed: OpenCode 1.17.3 failed that way without ever going busy.
- A turn waiting out a provider's retries shows as working, since nothing is written while it waits.
- A session whose OpenCode process started before the plugin was installed is not shown until that process is restarted.
- A database from OpenCode before 1.18, which has no event log, is not read; the daemon says so in its log.
- Checked against OpenCode 1.19.5 through `opencode serve` and the terminal interface, with permissions, questions, aborts, a subagent, errors, an archive, a deletion and a kill, and 1.18.25 through `opencode run`. The desktop app, background subagents and compaction were not tried.

## Pi

Pi runs extensions rather than hook commands, so the install writes an extension, `~/.pi/agent/extensions/agent-leds.ts`, that passes Pi's events to `agent-leds hook pi`. It reports only a Pi with a UI, in its terminal interface or RPC mode, as an editor integration runs it. Print and JSON modes have no one waiting on them, and Pi's subagents run in them.

| Event | Session becomes |
| --- | --- |
| `agent_start`, `ui_prompt_end` | working |
| `ui_prompt_start` during a run: an extension's dialog holds it up | question |
| `agent_settled` whose last assistant message ends in a question mark | question, as a weak signal |
| `agent_settled` otherwise | stopped |
| `agent_settled` after an error | error |
| `agent_settled` after an abort, or `session_shutdown` other than a reload | removed |

Pi has no permission prompts of its own; extensions that add them ask through dialogs, which show as a question. Checked against Pi 0.84.4 and 1.5.2 in RPC mode. Pi Durable, Pi's library for long-running agent apps, is a framework rather than a harness and is not covered.
