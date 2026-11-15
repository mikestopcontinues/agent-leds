# Design

How the service decides what the lights show and keeps them showing it. [harnesses.md](harnesses.md) covers what each agent harness reports.

## How it works

A small daemon owns the queue and the device. It reads what Claude Code writes about its sessions in `~/.claude/sessions/` and `~/.claude/projects/`, as FSEvents reports each change; the other harnesses report to it through hooks, until their own files are read too. Each harness declares the signals it gives and how each is known, and [coverage.md](coverage.md), generated from those declarations and the session table in `src/fsm.rs`, lists what each one cannot show. The CLI and the workshop talk to the daemon over a Unix socket.

The device runs whatever program is written to its `LEDS.LED` file, in SidePulse's own animation language, limited to 512 bytes and 20 lines. The daemon writes programs only when the queue changes, so a steady display costs nothing on the Mac:

- **Every state is equally bright.** A state's color sets its hue only. The renderer scales the four colors to the brightness of the dimmest hue at full strength, using the `balance` setting: how bright the red, green and blue dies look relative to each other. The default balance is an estimate; tune it by eye with `agent-leds config balance`.
- **A session that comes to want you** (stopped, a question, an error) is announced by a snake. The lights fade out. A head in the color of the state a session just entered crosses the strip toward the front of the queue: up to three lights, then a dark one if wanted. It pulls the new queue in behind it as a dimmed tail and runs off the front end. The queue then rises to full brightness in a wave from its front to its back. One setting, `step_ms`, paces everything after the fade out: the head moves on a light every step, and so does the crest of the wave. Each light, the front one included, goes dark in the step before the wave reaches it, so the crest always stands against a dark light, and then rises at an even pace over seven steps. A queue of n lights therefore rises in n + 7 steps. The whole change takes two to three seconds and spans one to three programs, which the daemon writes one after another, each when the one before it ends.

  To fit more lines in a program, a line names only the lights that change, or gives a row cut off after its last lit light, whichever is shorter. The rise is kept in one program where it fits, so that no write falls in the middle of it.
- **More than eight items** are cut to fit. Every state present keeps at least one light and the rest go to states in queue order. Nothing marks the items that are hidden.
- **Any other change** settles in quietly: a session going back to work, or a light leaving because its session was read, went silent, ended or was dismissed. The wave alone runs from the front of the queue to the back, with no fade out and no snake. Each light holds its old color until the wave reaches it, goes dark for a step, and rises in its new color. A quiet change never interrupts a snake; the snake ends on the queue as it is by then.
- **A played program or shown scene** (`agent-leds play`, or the workshop's "Show on device") holds the lights for a few seconds, then the queue returns.

The daemon saves the queue to `~/.local/state/agent-leds/queue.json` on every change and reloads it when it starts, so restarting it loses nothing.

The daemon reads the card's uptime every 15 seconds. The read keeps the SD reader from powering the card down, and an uptime that went backwards means the card restarted, so the queue is rewritten.

The same check notices an upgrade. The LaunchAgent and the hooks run the binary by the path `agent-leds install` ran it from, which for Homebrew is the symlink `/opt/homebrew/bin/agent-leds` that each upgrade repoints. When that path leads to a different file than the daemon started from, the daemon exits and launchd starts the new version. A binary that is gone, as mid-uninstall, does not count. An upgrade that registers hooks differently still needs `agent-leds install`, which `agent-leds doctor` says.

The daemon also keeps the card mounted. macOS ejects a disk that appears while the screen is locked, which the card does when the Mac wakes from hibernation, and an ejected card stays dark until it is taken out and put back. Disk Arbitration asks its clients before it ejects a disk, so the daemon refuses every eject of a disk whose volume name starts with `SidePulse`, and then tries every 5 seconds to mount it until that works, which it does once the screen is unlocked. Other cards in the reader eject as usual. Two things follow:

- The card cannot be ejected from Finder or with `diskutil eject` while the daemon runs. Pull it out, or stop the daemon first.
- A session with Disk Arbitration is asked nothing once that daemon restarts. The daemon watches its process id and exits when it changes; launchd starts it again, with a new session.

## When lights clear

The lights have to mean something, so every one of them has a way out:

| Light | Clears |
| --- | --- |
| Working | When the turn ends with no subagent or workflow left running, when it is interrupted, or when it waits on the user. No light clears on silence. |
| Stopped | At the next prompt, or `keep_read_minutes` after you have looked at the session. |
| Question, weak: the turn ended on a question | The same as stopped. You may read it and leave it unanswered. |
| Question, strong: a permission request, `AskUserQuestion` or an elicitation | When it is answered. The agent is blocked on it. |
| Error | At the next prompt. |
| Any | When the session ends, or the process running it does, which the daemon checks every few seconds; at `agent-leds dismiss`; or after `abandon_days` without a word from the session. |

A turn that ends again in a session that had already stopped counts as new: the light is unread again and the change is announced.

No harness has a hook for a session being read. For Claude Code the daemon reads the desktop app's own session files, under `~/Library/Application Support/Claude/claude-code-sessions/`. Each records the session id Claude Code's own files name and when the session was last opened. A session counts as read when it was opened after its turn ended, or when you are using the app with that session open: it is the session opened last, the app is the frontmost one (the daemon asks `lsappinfo`), and you touched the keyboard, mouse or trackpad in the last five seconds. The app being in front is not enough, since you may be away from the screen. The daemon looks when a turn ends and every 3 seconds while a session is unread. `agent-leds status` shows when each was read.

The files are undocumented. If the app changes them, or a session runs in a terminal or in another harness, the session is never read: its light stays until its next prompt, until the session ends, or until `abandon_days` pass. If the daemon cannot tell which session is open, because the newest file is half written or is not a Claude Code session, it marks none.

## The weak question

A turn whose last line ends in a question mark shows as a question that reading settles. SidePulse's own software uses a narrower rule: the line must open with a question word or an offer such as "want me to", so that closers such as "anything else?" do not count. In 2,528 Claude Code turns on the author's Mac over 30 days, 132 ended on a question mark. None was a casual closer, and the narrower rule would have missed 64 of them, nearly all decisions put to the user ("Two decisions for you: should I start at step 1, and …?"). So the broad rule stays.

## Code

Each module owns one concern; the ones that touch the system are thin.

- `main.rs`: the command line.
- `doctor.rs`: `agent-leds doctor`, which checks the service, the card, the hooks and for SidePulse's own software.
- `daemon.rs`: the service's loop. Everything it reads or changes outside itself goes through its `World` trait, which tests replace.
- `player.rs`: what the lights play and when, given the time. No I/O.
- `render.rs`: a queue or a change, as LEDS.LED programs. No I/O.
- `fsm.rs`: the session table, every phase a session can be in and the signal that moves it to each other. No I/O.
- `queue.rs`: the sessions followed, changed only by applying observations through the table, and the order their lights show in. No I/O. `state.rs` is what the daemon saves.
- `coverage.rs`: renders `docs/coverage.md` from what each harness declares.
- `source.rs` and `source/`: reading the harnesses. `claude.rs` reads Claude Code's session files and transcripts; `interim.rs` reads the other harnesses' hooks; `tail.rs` reads files as they grow; `fixture.rs` replays recorded sessions in the tests.
- `liveness.rs`: whether the process running a session is still the one that started it, and the hook process's own ancestors.
- `harness.rs` and `harness/`: what each harness declares it tells, its hook payloads until its files are read, and the plugins OpenCode and Pi load; installing, checking and removing hooks.
- `ipc.rs`: the socket protocol between the command line, the hooks and the service.
- `config.rs`, `settings.rs`: the settings, and reading and changing them by name.
- `desktop.rs`: which sessions the user has looked at in the Claude desktop app.
- `device.rs`, `eject.rs`, `launchd.rs`, `files.rs`: the card, keeping it mounted, the LaunchAgent, and writing shared files whole.

## Workshop

`prototype/` is a page for tuning the service's settings against the device. It is not part of the package: it needs [Bun](https://bun.sh) and `agent-leds` on the `PATH`, and talks to the running service. The colors that reach your eye have not been measured, so its "true color" view is a datasheet model to tune against the device.

- The lights on the page are what the installed `agent-leds` binary renders for a test queue, run in the firmware's own interpreter (`vendor/sdled.wasm`, from the SidePulse project). So the page shows what the device will do.
- Every setting is on the page: the snake, when lights clear, the state colors and palettes, overall brightness, the front light and the die balance. Changes apply to the page only until "Save to the service", which replaces the service's settings at once. "Revert" returns to what the service has.
- Changing the test queue plays the change the service would: the snake for a session that comes to want you, the quiet wave for anything else.
- "Show on device" plays the page's queue and unsaved settings on the real card for ten seconds, then the real queue returns.
- "Equal brightness" shows the three dies side by side, on the page or the device, so the balance can be set by eye.
- "Preview look" is how the page draws the lights and is not saved. "True color" shows how a value looks on the Pro's WS2816 LEDs rather than as the same hex on a screen; `src/color.ts` explains the model and its assumptions. Its defaults (response 0.6, diffusion 0.2) were matched against the device by eye.

## Open questions

- Installing with Homebrew has been checked on Apple silicon. On Intel, CI builds and tests every push, but no install has been run.
- Claude Code, Codex 0.153.2, Grok 1.0.13 and `cursor-agent` 2026.05.28 have been checked against real sessions; Grok, Cursor and Codex's subagents only from the command line (`grok -p`, `cursor-agent -p`, `codex exec`). What Cursor sends from its interface is taken from its code, and Junie's events from its documentation.
- Refusing an eject has been checked with `diskutil eject`: the eject fails and the card is mounted again within 5 seconds. A wake from hibernation with the screen locked has not been seen, nor has a restart of Disk Arbitration.
- Input in the Claude app with a session open is taken as having looked at it. Typing in a second window, or in the Chat tab, would count too.
- How the snake looks on the card. Each program is written when the one before it should end, so a slow write shows as a short pause in the snake.
- The default die balance (0.32, 0.54, 0.14) starts from the dies' luminance and allows for saturated red and blue looking brighter than their luminance. It has not been checked against the device.
- The color model assumes the firmware's white balance lands on D65 and uses estimated die chromaticities. Its response was matched by eye at 0.6, below the 1 the datasheet's linear light gives, so dim values look brighter than their share of the light. Hue accuracy has not been checked the same way.
