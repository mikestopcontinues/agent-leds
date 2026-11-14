# leds

`leds` shows a queue of agent sessions on a SidePulse Pro, the eight-light card in the MacBook's SD slot. Each session that is doing something or needs something gets one light: red for an error, orange for a question, green for stopped and unread, blue for working. The queue is sorted in that order with its front on the light furthest from you, and the lights are off when there is nothing to show.

It works with Claude Code today. The colors that reach your eye have not been measured, so the prototype's "true color" view is a datasheet model to tune against the device.

## How it works

A small daemon owns the queue and the device. Claude Code reports to it through a hook, and the CLI and the prototype talk to it over a Unix socket.

The device runs whatever program is written to its `LEDS.LED` file, in SidePulse's own animation language, limited to 512 bytes and 20 lines. The daemon writes a program only when the queue changes, so a steady display costs nothing on the Mac:

- **Every state is equally bright.** A state's color sets its hue only. The renderer scales the four colors to the brightness of the dimmest hue at full strength, using the `balance` setting: how bright the red, green and blue dies look relative to each other. The default balance is an estimate and should be tuned by eye in the prototype.
- **A change** fades the lights out, slides the color of the state a session just entered along the dark strip toward the front of the queue, then fades in the new queue. It takes about two and a half seconds.
- **More than eight items** are compressed. Every state present keeps at least one light, the rest go to states in queue order, and the last light of a state that did not fit breathes to say it stands for several. The prototype has brightness-based alternatives to the breathing.
- **A played program** (`leds play`, or the prototype's "Show on device") holds the lights for a few seconds, then the queue returns.

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

Events from subagents only mark the session as alive. Two gaps follow from what Claude Code reports:

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

SidePulse's own software must not drive the same card: set the device to `custom` display in its settings, or stop its service. Its eject prevention helper is still needed to keep the card mounted through sleep.

## Prototype

`prototype/` is a workshop page for trying ideas before they go into the service.

```sh
cd prototype && bun install && bun run dev
```

- Programs run in the firmware's own interpreter (`vendor/sdled.wasm`, from the SidePulse project), so what the page shows is what the device will do.
- "Service" renders through the installed `leds` binary. The other designs are TypeScript sketches in `src/designs.ts`: four ways to show that a state has more items than lights, and one that makes the front of the queue breathe.
- "Equal brightness" shows the three dies side by side, on the page or the device, so the balance can be set by eye.
- "True color" shows how a value looks on the Pro's WS2816 LEDs rather than as the same hex on a screen; `src/color.ts` explains the model and its assumptions.
- "Show on device" plays the current programs on the real card for ten seconds.

`bun test` checks the color model and runs every design's programs through the firmware interpreter.

## Open questions

- The default die balance (0.32, 0.54, 0.14) starts from the dies' luminance and allows for saturated red and blue looking brighter than their luminance. It has not been checked against the device.
- The color model assumes the firmware's white balance lands on D65 and uses estimated die chromaticities. Comparing the page against the card will show how far off that is.
