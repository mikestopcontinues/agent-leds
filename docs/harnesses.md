# Harnesses

What each agent harness reports to the service and where it falls short. [design.md](design.md) covers what the service does with the reports.

`agent-leds install` registers `agent-leds hook <harness>` with each harness, for the events listed below; OpenCode and Pi load a plugin of ours that runs it. The hook reads one event, tells the daemon what it means, prints nothing and always succeeds. So it never holds up a turn and never answers a permission request. Where a harness takes a hook's silence for an answer, that event is not hooked.

The harnesses took their hook events from Claude Code, so most of the names agree. Each harness reads only its own spelling. Grok also runs the hooks in `~/.claude/settings.json`, with its own payloads: Grok 1.0.13 sends its own snake_case event names there, which Claude Code's mapping does not follow, so a Grok session is not reported twice.

## Claude Code

Hooks are groups in `~/.claude/settings.json`.

| Hook event | Session becomes |
| --- | --- |
| `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `PostToolUseFailure`, `ElicitationResult` | working |
| `PreToolUse` for `AskUserQuestion`, `PermissionRequest`, `Elicitation` | question |
| `Stop` whose last line ends in a question mark | question, as a weak signal |
| `Stop` with a subagent or a workflow still running | working |
| `Stop` otherwise | stopped |
| `StopFailure` | error |
| `SessionEnd`, or a tool aborted by an interrupt | removed |

A turn that ends with a subagent or a workflow running in the background is not the session's last word: Claude Code starts a new turn when one finishes, so the session stays green until a turn ends with none running. `Stop` lists monitors and background shell commands as running tasks too. Those do not hold the session as working, because one can run for as long as the session does, and the session would never show that it had stopped.

Events from subagents never say what the session needs, but they do say that work is going on. A stopped session whose background subagent runs a tool goes back to working; a session with a question or an error stays as it is. Two gaps follow from what Claude Code reports:

- A subagent's permission request does not turn the session orange, because nothing after it says which agent was answered.
- An approved permission prompt stays orange until its tool finishes. Claude Code has no hook for an approval: after `PermissionRequest`, the next event is the tool's `PostToolUse`.
- Claude Code fires no hook when you interrupt a turn between tool calls. Its process says so instead: each running Claude Code process, in a terminal or the desktop app, keeps a file in `~/.claude/sessions/` with its session and whether it is busy. While a Claude Code light is lit, the daemon reads those files every 3 seconds. A working session whose process has been idle for 5 seconds with no hook since was interrupted, and its light goes. A session whose turn ended with work running in the background is idle too, and stays. The files are undocumented; if they change, an interrupted session stays green until its next prompt, or for 30 minutes of silence.
- A session whose process ends without a `SessionEnd`, as when its terminal is closed or it crashes, leaves once its file is gone or its process is. A session never seen running, such as one run with `claude -p`, is left to its hooks.

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

- Codex has no event for a turn that fails, so there is no error state. A session whose request fails stays green until its next prompt, or for 30 minutes of silence.
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
- A turn that reports nothing, such as one you rewind, stays green until the next prompt, or for 30 minutes of silence.

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
