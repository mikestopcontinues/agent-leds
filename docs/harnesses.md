# Harnesses

What each agent harness reports to the service and where it falls short. [design.md](design.md) covers what the service does with the reports.

The service reads Claude Code's and Codex's own files, and registers no hooks with Claude Code and one with Codex, for the one moment its files do not record. With each other harness, `agent-leds install` registers `agent-leds hook <harness>` for the events listed below; OpenCode and Pi load a plugin of ours that runs it. The hook passes the event to the daemon with the processes it runs under, prints nothing and always succeeds. So it never holds up a turn and never answers a permission request. Where a harness takes a hook's silence for an answer, that event is not hooked. Whatever the hooks say, a session leaves when the process running it ends.

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
