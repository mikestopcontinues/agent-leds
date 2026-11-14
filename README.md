# leds

`leds` shows a queue of agent sessions on a SidePulse Pro, the eight-light card in the MacBook's SD slot. Each session that is doing something or needs something gets one light: red for an error, orange for a question, green for stopped and unread, blue for working. The queue is sorted in that order with its front on the light furthest from you, and the lights are off when there is nothing to show.

It works with Claude Code today. The colors that reach your eye have not been measured, so the workshop's "true color" view is a datasheet model to tune against the device.

## How it works

A small daemon owns the queue and the device. Claude Code reports to it through a hook, and the CLI and the workshop talk to it over a Unix socket.

The device runs whatever program is written to its `LEDS.LED` file, in SidePulse's own animation language, limited to 512 bytes and 20 lines. The daemon writes programs only when the queue changes, so a steady display costs nothing on the Mac:

- **Every state is equally bright.** A state's color sets its hue only. The renderer scales the four colors to the brightness of the dimmest hue at full strength, using the `balance` setting: how bright the red, green and blue dies look relative to each other. The default balance is an estimate and should be tuned by eye in the workshop.
- **A change** is announced by a snake. The lights fade out. A head in the color of the state a session just entered crosses the strip toward the front of the queue: up to three lights, then a dark one if wanted. It pulls the new queue in behind it as a dimmed tail and runs off the front end. The queue then rises to full brightness in a wave from its front to its back. One setting, `step_ms`, paces everything after the fade out: the head moves on a light every step, and so does the crest of the wave. Each light, the front one included, goes dark in the step before the wave reaches it, so the crest always stands against a dark light, and then rises at an even pace over seven steps. A queue of n lights therefore rises in n + 7 steps. The whole change takes two to three seconds and spans one to three programs, which the daemon writes one after another, each when the one before it ends.

  To fit more lines in a program, a line names only the lights that change, or gives a row cut off after its last lit light, whichever is shorter. The rise is kept in one program where it fits, so that no write falls in the middle of it.
- **More than eight items** are cut to fit. Every state present keeps at least one light and the rest go to states in queue order. Nothing marks the items that are hidden.
- **A played program or shown scene** (`leds play`, or the workshop's "Show on device") holds the lights for a few seconds, then the queue returns.

The daemon saves the queue to `~/.local/state/leds/queue.json` on every change and reloads it when it starts, so restarting it loses nothing.

The daemon reads the card's uptime every 15 seconds. The read keeps the SD reader from powering the card down, and an uptime that went backwards means the card restarted, so the queue is rewritten.

## Claude Code states

| Hook event | Session becomes |
| --- | --- |
| `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `ElicitationResult` | working |
| `PreToolUse` for `AskUserQuestion`, `PermissionRequest`, `Elicitation` | question |
| `Stop` whose last line ends in a question mark | question |
| `Stop` otherwise | stopped |
| `StopFailure` | error |
| `SessionEnd`, or a tool aborted by an interrupt | removed |

Events from subagents never say what the session needs, but they do say that work is going on. A stopped session whose background subagent runs a tool goes back to working; a session with a question or an error stays as it is. Two gaps follow from what Claude Code reports:

- A subagent's permission request does not turn the session orange, because nothing after it says which agent was answered.
- Claude Code fires no hook when you interrupt a turn between tool calls, so that session stays blue until its next prompt, or for 30 minutes of silence.

### Read sessions

A stopped session keeps its light until you have looked at it, and then for `keep_read_minutes` more (15 by default, 0 to 1440). Its next prompt, `leds dismiss` or the end of the session still clear it at once. A question or an error stays until it is dealt with.

Claude Code has no hook for a session being read, so the daemon reads the desktop app's own session files, under `~/Library/Application Support/Claude/claude-code-sessions/`. Each records the session id the hooks report and when the session was last opened. A stopped session counts as read when it was opened after it stopped, or when it is the session open in the app while the app is the frontmost one, which the daemon asks `lsappinfo`. It checks when a session stops and every 15 seconds while a stopped session is unread. `leds status` shows when each was read.

The files are undocumented. If the app changes them, or a session runs in a terminal, the session is never read and keeps its light as before.

## Use

```sh
cargo install --path .
leds install          # LaunchAgent plus hooks in ~/.claude/settings.json
leds status
leds dismiss          # clear everything that is not working
leds palette          # list palettes; `leds palette bright` switches
leds color question ff7000
leds brightness 128
leds balance 0.32 0.54 0.14   # how bright the red, green and blue dies look
leds front 0          # light 7, the default, is the far end of the card in the MacBook's slot
```

`leds uninstall` removes the LaunchAgent and the hooks. Settings live in `~/.config/leds/config.toml`; the socket and log are in `~/.local/state/leds/`.

`keep_read_minutes` is how long a stopped session keeps its light after you have looked at it; see [Read sessions](#read-sessions).

The snake's settings are the `[snake]` table of the config. Levels run from 0 to 1 and timings are milliseconds:

| Setting | Meaning |
| --- | --- |
| `head` | Brightness of the head's front, middle and back lights. A front or back at 0 is left out, which shortens the head. |
| `gap` | Whether a dark light separates the head from the tail. |
| `tail` | Brightness of the queue while it is pulled in. |
| `fade_out_ms` | How long the old queue takes to go dark. |
| `step_ms` | The pace of the rest: how long the head, and then the wave, takes to move on one light. Each step of the head is half hold, half move. |

The workshop is the way to tune them. The daemon reads the file only when it starts, so change settings through the CLI or the workshop.

SidePulse's own software must not drive the same card: set the device to `custom` display in its settings, or stop its service. Its eject prevention helper is still needed to keep the card mounted through sleep.

## Workshop

`prototype/` is a page for tuning the service's settings against the device.

```sh
cd prototype && bun install && bun run dev
```

- The lights on the page are what the installed `leds` binary renders for a test queue, run in the firmware's own interpreter (`vendor/sdled.wasm`, from the SidePulse project). So the page shows what the device will do.
- Every setting is on the page: the snake, how long a read session keeps its light, the state colors and palettes, overall brightness, the front light and the die balance. Changes apply to the page only until "Save to the service", which replaces the service's settings at once. "Revert" returns to what the service has.
- "Show on device" plays the page's queue and unsaved settings on the real card for ten seconds, then the real queue returns.
- "Equal brightness" shows the three dies side by side, on the page or the device, so the balance can be set by eye.
- "Preview look" is how the page draws the lights and is not saved. "True color" shows how a value looks on the Pro's WS2816 LEDs rather than as the same hex on a screen; `src/color.ts` explains the model and its assumptions. Its defaults (response 0.6, diffusion 0.2) were matched against the device by eye.

`bun test` checks the color model and runs what the installed service renders through the firmware interpreter.

## Open questions

- Whether the desktop app records a session as opened when its window comes back to the front with that session already showing. If it does not, the session is read at the daemon's next check, up to 15 seconds later.
- "The app is in front" does not mean you are at the screen, or on the Code tab. A session that stops in the open tab while you are away is read at once and leaves after `keep_read_minutes`.
- How the snake looks on the card. Each program is written when the one before it should end, so a slow write shows as a short pause in the snake.
- The default die balance (0.32, 0.54, 0.14) starts from the dies' luminance and allows for saturated red and blue looking brighter than their luminance. It has not been checked against the device.
- The color model assumes the firmware's white balance lands on D65 and uses estimated die chromaticities. Its response was matched by eye at 0.6, below the 1 the datasheet's linear light gives, so dim values look brighter than their share of the light. Hue accuracy has not been checked the same way.
