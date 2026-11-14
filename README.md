# leds

`leds` shows a queue of agent sessions on a SidePulse Pro, the eight-light card in the MacBook's SD slot. Each session that is doing something or needs something gets one light: red for an error, orange for a question, green for stopped and unread, blue for working. The queue is sorted in that order with its front on the light furthest from you, and the lights are off when there is nothing to show.

It works with Claude Code today. The colors that reach your eye have not been measured, so the workshop's "true color" view is a datasheet model to tune against the device.

## How it works

A small daemon owns the queue and the device. Claude Code reports to it through a hook, and the CLI and the workshop talk to it over a Unix socket.

The device runs whatever program is written to its `LEDS.LED` file, in SidePulse's own animation language, limited to 512 bytes and 20 lines. The daemon writes programs only when the queue changes, so a steady display costs nothing on the Mac:

- **Every state is equally bright.** A state's color sets its hue only. The renderer scales the four colors to the brightness of the dimmest hue at full strength, using the `balance` setting: how bright the red, green and blue dies look relative to each other. The default balance is an estimate and should be tuned by eye in the workshop.
- **A change** is announced by a snake. The lights fade out. A head in the color of the state a session just entered crosses the strip toward the front of the queue: up to three lights, then a dark one if wanted. It pulls the new queue in behind it as a dimmed tail and runs off the front end. The queue can then crunch against that end, squeezing toward it and springing back, and it rises to full brightness. The whole change takes two to three seconds. Every step is a full row of colors, so the animation spans several programs, and the daemon writes each when the one before it ends. The crunch and the rise share one program, so no write falls in the middle of the crunch.
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

Events from subagents never say what the session needs, but they do say that work is going on. A stopped session whose background subagent runs a tool goes back to working; a session with a question or an error stays as it is. Three gaps follow from what Claude Code reports:

- A subagent's permission request does not turn the session orange, because nothing after it says which agent was answered.
- Claude Code fires no hook when you interrupt a turn between tool calls, so that session stays blue until its next prompt, or for 30 minutes of silence.
- "Unread" ends at the session's next prompt, at `leds dismiss`, or when the session ends. Nothing detects that you looked at it.

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

The snake's settings are the `[snake]` table of the config. Levels run from 0 to 1 and timings are milliseconds:

| Setting | Meaning |
| --- | --- |
| `head` | Brightness of the head's front, middle and back lights. A front or back at 0 is left out, which shortens the head. |
| `gap` | Whether a dark light separates the head from the tail. |
| `tail` | Brightness of the queue while it is pulled in. |
| `hold_ms`, `move_ms` | How long each step stays still, then takes to move one light on. |
| `fade_out_ms`, `fade_in_ms` | How long the old queue takes to go dark and the new one to rise to full. |
| `crunch` | How far the arriving queue squeezes toward the front end, as a share of its length. 0 is no crunch. |
| `crunch_ms` | How long the squeeze takes. |
| `bounce` | How far past its resting length the queue springs back, as a share of the squeeze. 0 returns straight to rest. |
| `spring_ms` | How long the queue takes to spring back and come to rest. |

The workshop is the way to tune them. The daemon reads the file only when it starts, so change settings through the CLI or the workshop.

SidePulse's own software must not drive the same card: set the device to `custom` display in its settings, or stop its service. Its eject prevention helper is still needed to keep the card mounted through sleep.

## Workshop

`prototype/` is a page for tuning the service's settings against the device.

```sh
cd prototype && bun install && bun run dev
```

- The lights on the page are what the installed `leds` binary renders for a test queue, run in the firmware's own interpreter (`vendor/sdled.wasm`, from the SidePulse project). So the page shows what the device will do.
- Every setting is on the page: the snake, the state colors and palettes, overall brightness, the front light and the die balance. Changes apply to the page only until "Save to the service", which replaces the service's settings at once. "Revert" returns to what the service has.
- "Show on device" plays the page's queue and unsaved settings on the real card for ten seconds, then the real queue returns.
- "Equal brightness" shows the three dies side by side, on the page or the device, so the balance can be set by eye.
- "Preview look" is how the page draws the lights and is not saved. "True color" shows how a value looks on the Pro's WS2816 LEDs rather than as the same hex on a screen; `src/color.ts` explains the model and its assumptions. Its defaults (response 0.6, diffusion 0.2) were matched against the device by eye.

`bun test` checks the color model and runs what the installed service renders through the firmware interpreter.

## Open questions

- How the snake looks on the card. Each program is written when the one before it should end, so a slow write shows as a short pause in the snake.
- The default die balance (0.32, 0.54, 0.14) starts from the dies' luminance and allows for saturated red and blue looking brighter than their luminance. It has not been checked against the device.
- The color model assumes the firmware's white balance lands on D65 and uses estimated die chromaticities. Its response was matched by eye at 0.6, below the 1 the datasheet's linear light gives, so dim values look brighter than their share of the light. Hue accuracy has not been checked the same way.
