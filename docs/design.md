# Design

How the service decides what the lights show and keeps them showing it. [harnesses.md](harnesses.md) covers what each agent harness reports.

## How it works

A small daemon owns the queue and the device. The CLI and the workshop talk to it over a Unix socket.

The daemon learns what each session does from the harness's own files. Each harness has one source, which reads the folders the harness writes its sessions to as FSEvents reports each change. When the daemon starts, each source reads where each running session's files leave it, and goes on from their end, so nothing is replayed. A hook remains only for a moment no file records, such as Codex's approval wait; [harnesses.md](harnesses.md) lists each, and the hook goes through the same source.

A source reports what each record says as signals, each a fact of its own: a prompt, a wait on the user, the end of a turn, an error. Every signal goes through one table, in `src/fsm.rs`, whose rows are in priority order: from the session's phase, the first row that matches one of the signals decides. Some signals belong together, as the ways a turn can end. Others told together overlap, as when a harness's format changes; the table's order still decides, and the daemon logs each set once and counts it in `agent-leds status --json`, which `agent-leds doctor` reports. Each harness declares the signals it gives and how each is known. A signal it does not declare is dropped and counted the same way. [coverage.md](coverage.md), generated from those declarations and the table, lists what each harness cannot show.

No timer infers what a session is doing. The only clocks are the two ways out in [When lights clear](#when-lights-clear): `keep_read_minutes` for a light that was read, and `abandon_days` for any light, working included, whose harness has said nothing of the session for that long. A session's process is known only by what the harness names: a pid and its start time in its own files, a file its process keeps locked, a marker, or the pid its hook or plugin passes on. The daemon looks up only that process, and never lists the process table.

`agent-leds doctor` reads each harness in use with the same sources, as the daemon does when it starts, restores what they show against the saved queue as the daemon does, and writes nothing. It prints a line for the saved queue, then one per harness: how many sessions run, by light, and the folders it read, then what the check of its hooks found. Cursor's, OpenCode's and Pi's files start no session, so for them the count is of the sessions their hooks or plugin reported, as the saved queue holds them. Junie's folder is the one launchd's environment gives the service. A saved queue the daemon cannot read, a folder that cannot be read, a file a seed cannot read, or a signal the harness does not declare or that overlaps needs attention. So does a record a seed reads of a kind the harness was not seen to write, counted by kind for every harness. Each source lists the kinds seen in real sessions and its fixtures, and a fixture with a kind its source does not list fails its tests, so a harness that writes otherwise after an update shows here first. Lines that are not records its source reads, and lines too long to read at all (over 4 MiB, as an image pasted into a prompt can be), are counted and said, but need no attention by themselves: a crash or a rewrite leaves a line cut short, as Pi's resume does. The daemon logs the same counts as it starts, and as it reads after that, each unknown kind the first time. While the daemon runs, doctor also reports what the daemon counted since it last started.

The device runs whatever program is written to its `LEDS.LED` file, in SidePulse's own animation language, limited to 512 bytes and 20 lines. The daemon writes programs only when the queue changes, so a steady display costs nothing on the Mac:

- **Every state is equally bright.** A state's color sets its hue only. The renderer scales the four colors to the brightness of the dimmest hue at full strength, using the `balance` setting: how bright the red, green and blue dies look relative to each other. The default balance is an estimate; tune it by eye with `agent-leds config balance`.
- **A session that comes to want you** (stopped, a question, an error) is announced by a snake. The lights fade out. A head in the color of the state a session just entered crosses the strip toward the front of the queue: up to three lights, then a dark one if wanted. It pulls the new queue in behind it as a dimmed tail and runs off the front end. The queue then rises to full brightness in a wave from its front to its back. One setting, `step_ms`, paces everything after the fade out: the head moves on a light every step, and so does the crest of the wave. Each light, the front one included, goes dark in the step before the wave reaches it, so the crest always stands against a dark light, and then rises at an even pace over seven steps. A queue of n lights therefore rises in n + 7 steps. The whole change takes two to three seconds and spans one to three programs, which the daemon writes one after another, each when the one before it ends.

  To fit more lines in a program, a line names only the lights that change, or gives a row cut off after its last lit light, whichever is shorter. The rise is kept in one program where it fits, so that no write falls in the middle of it.
- **More than eight items** are cut to fit. Every state present keeps at least one light and the rest go to states in queue order. Nothing marks the items that are hidden.
- **Any other change** settles in quietly: a session going back to work, or a light leaving because its session was read, ended, was dismissed or was abandoned. The wave alone runs from the front of the queue to the back, with no fade out and no snake. Each light holds its old color until the wave reaches it, goes dark for a step, and rises in its new color. A quiet change never interrupts a snake; the snake ends on the queue as it is by then.
- **A played program or shown scene** (`agent-leds play`, or the workshop's "Show on device") holds the lights for a few seconds, then the queue returns.

The daemon saves the queue to `~/.local/state/agent-leds/queue.json` on every change. When it starts, it rebuilds the queue from the harnesses' files, and keeps a saved session's read mark, dismissal or state only a hook told while that is still about the same turn and the files do not contradict it; `Queue::restore` holds the rule. A read mark or dismissal stands only while the files show the session where the daemon made it, or where the daemon alone could have taken it from there. A turn that ended while the daemon was down shows unread, even when the user dismissed it while it worked; a wait that began then shows and lets go back to the read mark or dismissal the session had, while a wait the user dismissed stays dismissed; a session whose process is gone is dropped, and a state only a hook told is lost once the files have moved past it, as [harnesses.md](harnesses.md) says of Grok's errors. A saved queue the daemon cannot read, as after an upgrade that changes its format, is ignored. The counts of undeclared and overlapping signals are not saved, and start again from nothing.

The daemon reads the card's uptime every 15 seconds. The read keeps the SD reader from powering the card down, and an uptime that went backwards means the card restarted, so the queue is rewritten.

The same check notices an upgrade. The LaunchAgent and the hooks run the binary by the path `agent-leds install` ran it from, which for Homebrew is the symlink `/opt/homebrew/bin/agent-leds` that each upgrade repoints. When that path leads to a different file than the daemon started from, the daemon exits and launchd starts the new version. A binary that is gone, as mid-uninstall, does not count. An upgrade that registers hooks differently still needs `agent-leds install`, which `agent-leds doctor` says.

The daemon also keeps the card mounted. macOS ejects a disk that appears while the screen is locked, which the card does when the Mac wakes from hibernation, and an ejected card stays dark until it is taken out and put back. Disk Arbitration asks its clients before it ejects a disk, so the daemon refuses every eject of a disk whose volume name starts with `SidePulse`, and then tries every 5 seconds to mount it until that works, which it does once the screen is unlocked. Other cards in the reader eject as usual. Two things follow:

- The card cannot be ejected from Finder or with `diskutil eject` while the daemon runs. Pull it out, or stop the daemon first.
- A session with Disk Arbitration is asked nothing once that daemon restarts. The daemon watches its process id and exits when it changes; launchd starts it again, with a new session.

## When lights clear

The lights have to mean something, so every one of them has a way out:

| Light | Clears |
| --- | --- |
| Working | When the turn ends with no subagent or workflow left running, when it is interrupted, or when it waits on the user; and, like every light, after `abandon_days` without a word from the session. |
| Stopped | At the next prompt, or `keep_read_minutes` after you have looked at the session. |
| Question, weak: the turn ended on a question | The same as stopped. You may read it and leave it unanswered. |
| Question, strong: a permission request, `AskUserQuestion` or an elicitation | When it is answered. The agent is blocked on it. One that work running after its turn made goes back, once answered, to the light it covered, as you left it: read, dismissed or not. |
| Error | At the next prompt. |
| Any | When the session ends, or the process its harness names does, which the daemon checks every few seconds; at `agent-leds dismiss`; or after `abandon_days` without a word from the session. |

A turn that ends again in a session that had already stopped counts as new: the light is unread again and the change is announced.

The daemon applies everything one read finds before it shows anything, and announces only a state its session still holds after all of it. A turn that ends while a prompt you queued starts the next one in the same read goes on working without a snake.

No harness has a hook for a session being read. OpenCode 2.x records it in its database, which the daemon reads with the session (see `docs/harnesses.md`). For Claude Code the daemon reads the desktop app's own session files, under `~/Library/Application Support/Claude/claude-code-sessions/`. Each records the session id Claude Code's own files name and when the session was last opened. A session counts as read when it was opened after its turn ended, or when you are using the app with that session open: it is the session opened last, the app is the frontmost one (the daemon asks `lsappinfo`), and you touched the keyboard, mouse or trackpad in the last five seconds. The app being in front is not enough, since you may be away from the screen. The daemon looks when a turn ends and every 3 seconds while a session is unread. `agent-leds status` shows when each was read.

The files are undocumented. If the app changes them, or a session runs in a terminal or in another harness that records no reading, the session is never read: its light stays until its next prompt, until the session ends, until you dismiss it, or until `abandon_days` pass. `docs/coverage.md` lists these under its stuck phases. If the daemon cannot tell which session is open, because the newest file is half written or is not a Claude Code session, it marks none.

## The weak question

A turn whose last line ends in a question mark shows as a question that reading settles. SidePulse's own software uses a narrower rule: the line must open with a question word or an offer such as "want me to", so that closers such as "anything else?" do not count. In 2,528 Claude Code turns on the author's Mac over 30 days, 132 ended on a question mark. None was a casual closer, and the narrower rule would have missed 64 of them, nearly all decisions put to the user ("Two decisions for you: should I start at step 1, and …?"). So the broad rule stays.

## Code

Each module owns one concern; the ones that touch the system are thin.

- `main.rs`: the command line.
- `doctor.rs`: `agent-leds doctor`, which checks the service, the card, what the daemon has read, the queue it saved, each harness's files and hooks, and for SidePulse's own software.
- `daemon.rs`: the service's loop. Everything it reads or changes outside itself goes through its `World` trait, which tests replace.
- `player.rs`: what the lights play and when, given the time. No I/O.
- `render.rs`: a queue or a change, as LEDS.LED programs. No I/O.
- `fsm.rs`: the session table, every phase a session can be in and the signal that moves it to each other. No I/O.
- `queue.rs`: the sessions followed, changed only by applying observations through the table, and the order their lights show in. No I/O. `state.rs` is the four lights.
- `coverage.rs`: renders `docs/coverage.md` from what each harness declares.
- `source.rs` and `source/`: reading the harnesses, and the watcher that tells the sources what changed. One file per harness reads its files and translates the hooks it keeps, with `claude.rs` the model the others follow; `tail.rs` reads files as they grow; `fixture.rs` replays recorded sessions in the tests.
- `liveness.rs`: whether the process running a session is still the one that started it, and the hook process's own ancestors.
- `harness.rs` and `harness/`: what each harness declares it tells, the plugins OpenCode and Pi load, and installing, checking and removing hooks.
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
- What each harness writes was studied in real sessions of Claude Code 2.1.325, Codex 0.153.2, Grok 1.0.77, Cursor 2026.11.13, Junie 26.11.1 (4020.10), OpenCode 1.19.5 and Pi 0.84.4 and 1.5.2. [harnesses.md](harnesses.md) lists what each run left untried, and [coverage.md](coverage.md) marks each signal known only from code, docs or captures.
- Refusing an eject has been checked with `diskutil eject`: the eject fails and the card is mounted again within 5 seconds. A wake from hibernation with the screen locked has not been seen, nor has a restart of Disk Arbitration.
- Input in the Claude app with a session open is taken as having looked at it. Typing in a second window, or in the Chat tab, would count too.
- How the snake looks on the card. Each program is written when the one before it should end, so a slow write shows as a short pause in the snake.
- The default die balance (0.32, 0.54, 0.14) starts from the dies' luminance and allows for saturated red and blue looking brighter than their luminance. It has not been checked against the device.
- The color model assumes the firmware's white balance lands on D65 and uses estimated die chromaticities. Its response was matched by eye at 0.6, below the 1 the datasheet's linear light gives, so dim values look brighter than their share of the light. Hue accuracy has not been checked the same way.
