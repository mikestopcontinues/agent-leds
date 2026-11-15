# Harnesses

What each agent harness reports to the service and where it falls short. [design.md](design.md) covers what the service does with the reports.

The service reads Claude Code's and Cursor's own files. It registers no hooks with Claude Code, and one with Cursor. With each other harness, `agent-leds install` registers `agent-leds hook <harness>` for the events listed below; OpenCode and Pi load a plugin of ours that runs it. The hook passes the event to the daemon with the processes it runs under, prints nothing and always succeeds. So it never holds up a turn and never answers a permission request. Where a harness takes a hook's silence for an answer, that event is not hooked. Whatever the hooks say, a session leaves when the process running it ends.

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
- A daemon that restarts during a text-only turn shows the previous turn's end, because such a turn writes nothing until it ends.
- After `/new`, the old chat stays bound to the process until it exits.
- A process killed with `kill -9` leaves its marker. The daemon sees that the process is gone at its next liveness check, within seconds.

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
