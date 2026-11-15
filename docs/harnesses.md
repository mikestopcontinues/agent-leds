# Harnesses

What each agent harness reports to the service and where it falls short. [design.md](design.md) covers what the service does with the reports.

The service reads Claude Code's own files, and registers no hooks with it. It reads Grok's files too, and keeps one hook for what they do not record. With each other harness, `agent-leds install` registers `agent-leds hook <harness>` for the events listed below; OpenCode and Pi load a plugin of ours that runs it. The hook passes the event to the daemon with the processes it runs under, prints nothing and always succeeds. So it never holds up a turn and never answers a permission request. Where a harness takes a hook's silence for an answer, that event is not hooked. Whatever the hooks say, a session leaves when the process running it ends.

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
| The `StopFailure` hook | error |
| `turn_ended` cancelled by Ctrl-C, the stop button or the dashboard, or by rejecting or cancelling a permission | removed |
| The row gone from `active_sessions.json`, as `/quit` leaves it even mid-turn, or its process gone | removed |

Every tool, read-only ones too, writes a permission request, and Grok writes each request and its answer as separate lines. One Grok approves by itself is answered within 7 ms; one the user answers waits seconds. So a request counts only once it has gone 250 ms unanswered, and the light turns to question that much later. The question tool's own approval is never a wait. A row left by a process that was killed stays in the registry until another session starts, so a row counts only while its pid runs with the start it records. A subagent runs in a session folder of its own, beside its parent's, under an id the registry never lists, so it is never followed; the parent shows its work. The files are undocumented; the fixtures in `tests/fixtures/grok/`, recorded from Grok 1.0.77, pin what the daemon expects of them. The gaps:

- A turn that fails is known only from the hook, so a daemon started after it shows what the files say: working, or stopped once the turn's end is written. A request that cannot connect retries without end and never ends the turn, so the session stays green.
- A run with `grok -p` gets no row, so it is not shown.
- A subagent that finishes without waking its parent should show the session as stopped; only subagents that wake their parent, which starts a turn, have been seen.
- Plan approval, the leader process (`use_leader`) and several windows on one session have not been tried.

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

OpenCode runs plugins rather than hook commands, so the install writes a plugin, `~/.config/opencode/plugins/agent-leds.js`, that passes OpenCode's events to `agent-leds hook opencode`. The plugin runs inside OpenCode's server, so it covers the terminal interface, the desktop app and `opencode serve` alike.

| Event | Session becomes |
| --- | --- |
| `session.status` going busy, `permission.replied`, `question.replied`, `question.rejected` | working |
| `permission.asked`, `question.asked` | question |
| `session.idle` whose last assistant message ends in a question mark | question, as a weak signal |
| `session.idle` otherwise | stopped |
| `session.idle` after an error | error |
| `session.idle` after `MessageAbortedError`, or `session.deleted` | removed |

A subagent runs in a session of its own whose `parentID` names the session that started it. Its going busy counts as that session's work in the background, and its permission requests and questions put that session's light to question, since they hold it up. Its own runs ending and its deletion are left out.

The gaps:

- Quitting OpenCode ends no session, since sessions outlive it. Its light stays until its next prompt, `agent-leds dismiss`, or `abandon_days`.
- Checked against OpenCode 1.19.5 through `opencode serve`, with a permission, a question, an abort and a subagent. The plugin uses event names that OpenCode's plugin types do not list yet, though its server sends them.

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
