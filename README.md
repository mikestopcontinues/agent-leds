# leds

`leds` shows a queue of agent sessions on a SidePulse Pro, the eight-light card in the MacBook's SD slot. Each session that is doing something or needs something gets one light: red for an error, orange for a question, green for stopped and unread, blue for working. The queue is sorted in that order with its front on the light furthest from you, and the lights are off when there is nothing to show.

It works with Claude Code, Codex, Grok, Cursor and Junie. The colors that reach your eye have not been measured, so the workshop's "true color" view is a datasheet model to tune against the device.

## How it works

A small daemon owns the queue and the device. Each harness reports to it through a hook, and the CLI and the workshop talk to it over a Unix socket.

The device runs whatever program is written to its `LEDS.LED` file, in SidePulse's own animation language, limited to 512 bytes and 20 lines. The daemon writes programs only when the queue changes, so a steady display costs nothing on the Mac:

- **Every state is equally bright.** A state's color sets its hue only. The renderer scales the four colors to the brightness of the dimmest hue at full strength, using the `balance` setting: how bright the red, green and blue dies look relative to each other. The default balance is an estimate and should be tuned by eye in the workshop.
- **A session that comes to want you** (stopped, a question, an error) is announced by a snake. The lights fade out. A head in the color of the state a session just entered crosses the strip toward the front of the queue: up to three lights, then a dark one if wanted. It pulls the new queue in behind it as a dimmed tail and runs off the front end. The queue then rises to full brightness in a wave from its front to its back. One setting, `step_ms`, paces everything after the fade out: the head moves on a light every step, and so does the crest of the wave. Each light, the front one included, goes dark in the step before the wave reaches it, so the crest always stands against a dark light, and then rises at an even pace over seven steps. A queue of n lights therefore rises in n + 7 steps. The whole change takes two to three seconds and spans one to three programs, which the daemon writes one after another, each when the one before it ends.

  To fit more lines in a program, a line names only the lights that change, or gives a row cut off after its last lit light, whichever is shorter. The rise is kept in one program where it fits, so that no write falls in the middle of it.
- **More than eight items** are cut to fit. Every state present keeps at least one light and the rest go to states in queue order. Nothing marks the items that are hidden.
- **Any other change** settles in quietly: a session going back to work, or a light leaving because its session was read, went silent, ended or was dismissed. The wave alone runs from the front of the queue to the back, with no fade out and no snake. Each light holds its old color until the wave reaches it, goes dark for a step, and rises in its new color. A quiet change never interrupts a snake; the snake ends on the queue as it is by then.
- **A played program or shown scene** (`leds play`, or the workshop's "Show on device") holds the lights for a few seconds, then the queue returns.

The daemon saves the queue to `~/.local/state/leds/queue.json` on every change and reloads it when it starts, so restarting it loses nothing.

The daemon reads the card's uptime every 15 seconds. The read keeps the SD reader from powering the card down, and an uptime that went backwards means the card restarted, so the queue is rewritten.

## Harness states

`leds install` registers `leds hook <harness>` with each harness, for the events listed below. The hook reads one event, tells the daemon what it means, prints nothing and always succeeds. So it never holds up a turn and never answers a permission request. Where a harness takes a hook's silence for an answer, that event is not hooked.

The harnesses took their hook events from Claude Code, so most of the names agree. Each harness reads only its own spelling. Grok also runs the hooks in `~/.claude/settings.json`, with its own payloads, and those are not taken for Claude Code sessions.

### Claude Code

Hooks are groups in `~/.claude/settings.json`.

| Hook event | Session becomes |
| --- | --- |
| `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `PostToolUseFailure`, `ElicitationResult` | working |
| `PreToolUse` for `AskUserQuestion`, `PermissionRequest`, `Elicitation` | question |
| `Stop` whose last line ends in a question mark | question, as a weak signal |
| `Stop` otherwise | stopped |
| `StopFailure` | error |
| `SessionEnd`, or a tool aborted by an interrupt | removed |

Events from subagents never say what the session needs, but they do say that work is going on. A stopped session whose background subagent runs a tool goes back to working; a session with a question or an error stays as it is. Two gaps follow from what Claude Code reports:

- A subagent's permission request does not turn the session orange, because nothing after it says which agent was answered.
- Claude Code fires no hook when you interrupt a turn between tool calls, so that session stays blue until its next prompt, or for 30 minutes of silence. Time the Mac spent asleep, or the daemon spent stopped, does not count as silence.

### Codex

Hooks are `[[hooks.<Event>]]` groups in `~/.codex/config.toml`, after the groups that are there. Codex runs a hook only once its hash is recorded under `[hooks.state]`, so the install asks `codex app-server` for the hash of each and records it. That needs `codex` on the `PATH`. Codex finds a hook's hash by the hook's place among the groups of its event, so if another tool reorders the groups, run `leds install` again.

| Hook event | Session becomes |
| --- | --- |
| `UserPromptSubmit`, `PreToolUse`, `PostToolUse` | working |
| `PermissionRequest`, `PreToolUse` for `request_user_input` or `request_user_input_async` | question |
| `Stop` whose last line ends in a question mark, or that is a `<proposed_plan>` to review | question, as a weak signal |
| `Stop` otherwise | stopped |
| `SessionEnd`, `Interrupt` | removed |

Subagents are treated as Claude Code's are. Codex also runs agents of its own, such as the one that consolidates its memories, as sessions it keeps no transcript of. Those are ignored, as is any session Codex keeps no transcript of. The gaps:

- Codex has no event for a turn that fails, so there is no error state. A session whose request fails stays blue until its next prompt, or for 30 minutes of silence.
- `request_user_input_async` does not stop the agent. The session is orange until the agent's next tool call, not until you answer.
- A plan is recognized only when Codex passes it as the turn's last message.

### Grok

Hooks are a file of their own, `~/.grok/hooks/leds.json`.

| Hook event | Session becomes |
| --- | --- |
| `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `PostToolUseFailure` | working |
| `Notification` of a `permission_prompt`, `PreToolUse` for `ask_user_question` | question |
| `Stop` whose last line ends in a question mark | question, as a weak signal |
| `Stop` otherwise, or `StopCancelled` when Grok gave the turn up by itself | stopped |
| `StopFailure` | error |
| `SessionEnd`, or `StopCancelled` by the user | removed |

Grok fires `Stop` again when the session closes; only the one with reason `end_turn` counts. A subagent runs as a session of its own and says so, which is treated as Claude Code's subagents are. The gaps:

- Grok reports a cancelled turn off to the side, so the report can arrive after the next prompt and remove a session that is working again. Its next tool call brings it back.
- A turn that reports nothing, such as one you rewind, stays blue until the next prompt, or for 30 minutes of silence.

### Cursor

Hooks are commands in `~/.cursor/hooks.json`.

| Hook event | Session becomes |
| --- | --- |
| `beforeSubmitPrompt`, `postToolUse`, `postToolUseFailure` | working |
| `stop` with status `completed` | stopped |
| `stop` with status `error` | error |
| `sessionEnd`, `stop` with status `aborted`, or a tool aborted by an interrupt | removed |

The gaps:

- No question state. Cursor expects the hooks that run before a tool to say whether the tool may run, so those are not hooked, and it has no event for a permission request. `stop` does not carry the agent's last message.
- A session waiting for approval of a tool stays blue.

### Junie

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
- Junie names the session only in `UserPromptSubmit`. The hook keeps the last one in `~/.local/state/leds/junie-session.json` and takes every other event to belong to it. With two Junie sessions at once, an event goes to the one that submitted a prompt last.

### When lights clear

The lights have to mean something, so every one of them has a way out:

| Light | Clears |
| --- | --- |
| Working | When the turn ends, or after 30 minutes of silence. |
| Stopped | At the next prompt, or `keep_read_minutes` after you have looked at the session (15 by default, 0 to 1440). |
| Question, weak: the turn ended on a question | The same as stopped. You may read it and leave it unanswered. |
| Question, strong: a permission request, `AskUserQuestion` or an elicitation | When it is answered. The agent is blocked on it. |
| Error | At the next prompt. |
| Any | When the session ends, at `leds dismiss`, or after `abandon_days` without a word from the session (3 by default, 1 to 365). |

A turn that ends again in a session that had already stopped counts as new: the light is unread again and the change is announced.

No harness has a hook for a session being read. For Claude Code the daemon reads the desktop app's own session files, under `~/Library/Application Support/Claude/claude-code-sessions/`. Each records the session id the hooks report and when the session was last opened. A session counts as read when it was opened after its turn ended, or when you are using the app with that session open: it is the session opened last, the app is the frontmost one (the daemon asks `lsappinfo`), and you touched the keyboard, mouse or trackpad in the last five seconds. The app being in front is not enough, since you may be away from the screen. The daemon looks when a turn ends and every 3 seconds while a session is unread. `leds status` shows when each was read.

The files are undocumented. If the app changes them, or a session runs in a terminal or in another harness, the session is never read: its light stays until its next prompt, until the session ends, or until `abandon_days` pass. If the daemon cannot tell which session is open, because the newest file is half written or is not a Claude Code session, it marks none.

## Use

```sh
cargo install --path .
leds install          # LaunchAgent plus the hooks of every harness in use
leds status           # the queue: state, age, session, harness, folder
leds dismiss          # clear everything that is not working
leds palette          # list palettes; `leds palette bright` switches
leds color question ff7000
leds brightness 128
leds balance 0.32 0.54 0.14   # how bright the red, green and blue dies look
leds front 0          # light 7, the default, is the far end of the card in the MacBook's slot
```

`leds install` registers the hook with each harness that has its folder in your home directory (`~/.claude`, `~/.codex`, `~/.grok`, `~/.cursor`, `~/.junie`) and says which. Run it again after you start using another harness. It leaves every hook that is not its own as it is, and running it twice changes nothing. `leds uninstall` removes the LaunchAgent and its own hooks, and nothing else. Settings live in `~/.config/leds/config.toml`; the socket and log are in `~/.local/state/leds/`.

`keep_read_minutes` and `abandon_days` set when lights clear; see [When lights clear](#when-lights-clear).

The snake's settings are the `[snake]` table of the config. Levels run from 0 to 1 and timings are milliseconds:

| Setting | Meaning |
| --- | --- |
| `head` | Brightness of the head's front, middle and back lights. A front or back at 0 is left out, which shortens the head. |
| `gap` | Whether a dark light separates the head from the tail. |
| `tail` | Brightness of the queue while it is pulled in. |
| `fade_out_ms` | How long the old queue takes to go dark. |
| `step_ms` | The pace of the rest: how long the head, and then the wave, takes to move on one light. Each step of the head is half hold, half move. |

The workshop is the way to tune them. The daemon reads the file only when it starts, so change settings through the CLI or the workshop. A key the service does not know is ignored, so a config from an older version still loads; a value it cannot use stops the daemon with a message in its log.

SidePulse's own software must not drive the same card: set the device to `custom` display in its settings, or stop its service. Its eject prevention helper is still needed to keep the card mounted through sleep.

## Workshop

`prototype/` is a page for tuning the service's settings against the device.

```sh
cd prototype && bun install && bun run dev
```

- The lights on the page are what the installed `leds` binary renders for a test queue, run in the firmware's own interpreter (`vendor/sdled.wasm`, from the SidePulse project). So the page shows what the device will do.
- Every setting is on the page: the snake, when lights clear, the state colors and palettes, overall brightness, the front light and the die balance. Changes apply to the page only until "Save to the service", which replaces the service's settings at once. "Revert" returns to what the service has.
- Changing the test queue plays the change the service would: the snake for a session that comes to want you, the quiet wave for anything else.
- "Show on device" plays the page's queue and unsaved settings on the real card for ten seconds, then the real queue returns.
- "Equal brightness" shows the three dies side by side, on the page or the device, so the balance can be set by eye.
- "Preview look" is how the page draws the lights and is not saved. "True color" shows how a value looks on the Pro's WS2816 LEDs rather than as the same hex on a screen; `src/color.ts` explains the model and its assumptions. Its defaults (response 0.6, diffusion 0.2) were matched against the device by eye.

`bun test` checks the color model and runs what the installed service renders through the firmware interpreter.

## Open questions

- Only Claude Code and Codex 0.153.2 have been checked against real sessions. Grok's events are taken from its bundled hooks guide (1.0.13), Cursor's from the code of `cursor-agent` 2026.05.28, and Junie's from its documentation; Grok does list the registered hooks in `grok inspect`.
- Whether Grok sends its own camelCase payloads to the hooks it reads from `~/.claude/settings.json`. If it sent Claude Code's spelling there, a Grok session would be reported twice.
- Whether Codex's desktop app runs other sessions of its own that do keep a transcript. Those would show as lights.
- Input in the Claude app with a session open is taken as having looked at it. Typing in a second window, or in the Chat tab, would count too.
- An approved permission prompt stays orange until its tool finishes. No hook reports the approval, and the desktop app's session file does not record it.
- Whether "the last line ends in a question mark" is the right weak signal. SidePulse's own software uses a narrower rule: the line must open with a question word or an offer such as "want me to", and casual closers such as "anything else?" do not count.
- How the snake looks on the card. Each program is written when the one before it should end, so a slow write shows as a short pause in the snake.
- The default die balance (0.32, 0.54, 0.14) starts from the dies' luminance and allows for saturated red and blue looking brighter than their luminance. It has not been checked against the device.
- The color model assumes the firmware's white balance lands on D65 and uses estimated die chromaticities. Its response was matched by eye at 0.6, below the 1 the datasheet's linear light gives, so dim values look brighter than their share of the light. Hue accuracy has not been checked the same way.
