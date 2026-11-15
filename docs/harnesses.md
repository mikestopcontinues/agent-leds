# Harnesses

What each agent harness reports to the service and where it falls short. [design.md](design.md) covers what the service does with the reports.

The service reads Claude Code's and Junie's own files, and registers no hooks with them. With each other harness, `agent-leds install` registers `agent-leds hook <harness>` for the events listed below; OpenCode and Pi load a plugin of ours that runs it. The hook passes the event to the daemon with the processes it runs under, prints nothing and always succeeds. So it never holds up a turn and never answers a permission request. Where a harness takes a hook's silence for an answer, that event is not hooked. Whatever the hooks say, a session leaves when the process running it ends.

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

Junie keeps its folder at `$JUNIE_HOME`, or at `~/.junie` in the account's home directory, which it takes from the system and never from `$HOME`; the daemon finds it the same way. Each Junie process writes a sidecar to `processes/` naming the session it takes, and each session appends its events to `sessions/<session>/events.jsonl`, which its first prompt creates. The daemon reads both as they change, and registers no hooks: an install removes the ones an older agent-leds registered in `~/.junie/config.json`, with the file their hook kept, `~/.local/state/agent-leds/junie-session.json`.

| What Junie writes | Session becomes |
| --- | --- |
| `UserPromptEvent`, or `TaskStartedEvent` when a task resumes | working |
| A block waiting in state `INPUT_REQUIRED` with an `approvalRequest` | question |
| An `AskAsyncRequestUpdatedEvent` or `ChoiceRequestUpdatedEvent` waiting in state `INPUT_REQUIRED` | question |
| The waiting step written again waiting on nothing, or the user's response | working |
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
