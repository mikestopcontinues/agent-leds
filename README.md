# agent-leds

`agent-leds` replaces the SidePulse Pro's own software with a status light for every coding agent session: Claude Code, Codex, Grok, Cursor, Junie, OpenCode and Pi. All sessions share one queue on the card's eight lights, most urgent at the front: red for an error, orange for a question, blue for a finished turn you have not read, green for working.

- Every agent and every session on one strip, sorted by what needs you most.
- Lights clear themselves: a finished Claude Code session goes dark once you have looked at it in the desktop app.
- A change that needs you is announced by a snake; anything else settles in quietly.
- Every color looks equally bright on the card.
- The card stays mounted through sleep and locked-screen wakes.

| On the card | |
| --- | --- |
| <img src="docs/previews/stopped.webp" height="40" alt="A blue snake crosses the strip and the queue rises with blue at the front"> | A session stops |
| <img src="docs/previews/question.webp" height="40" alt="An orange snake crosses the strip and the queue rises with orange at the front"> | A session asks a question |
| <img src="docs/previews/error.webp" height="40" alt="A red snake crosses the strip and the queue rises with red at the front"> | A session fails |
| <img src="docs/previews/read.webp" height="40" alt="A wave runs along the queue and the blue light leaves"> | You read a finished session |

The front of the queue is the light furthest from you, at the right of each picture. macOS only; unofficial and not affiliated with SidePulse.

## Install

You need a SidePulse Pro in your Mac's SD slot and [Homebrew](https://brew.sh). Remove SidePulse's own software first: two programs writing to the card overwrite each other.

```sh
brew install mikestopcontinues/tap/agent-leds
agent-leds install
```

`agent-leds install` starts the service at login and adds its hooks to each harness you use (`~/.claude`, `~/.codex`, `~/.grok`, `~/.cursor`, `~/.junie`, `~/.config/opencode`, `~/.pi`), leaving other hooks alone. OpenCode and Pi get a plugin of their own instead of hooks. Run it again after you start using another harness. `brew upgrade agent-leds` is the whole upgrade: the service restarts on the new version by itself.

If the lights don't follow your sessions, `agent-leds doctor` checks the service, the card and each harness's hooks, and says what to fix.

To remove it, run `agent-leds uninstall`, then `brew uninstall agent-leds`.

## Use

```sh
agent-leds status                     # the queue
agent-leds doctor                     # check the install
agent-leds dismiss                    # clear every light that is not working
agent-leds palette signal             # switch palette
agent-leds config                     # every setting
agent-leds config snake.step_ms 150   # change one
```

Every command takes `-h`; `agent-leds config -h` lists the settings with their ranges and defaults. Settings live in `~/.config/agent-leds/config.toml`, and the log in `~/.local/state/agent-leds/daemon.log`.

| Palette | |
| --- | --- |
| <img src="docs/previews/palette-default.webp" height="40" alt="default"> | `default` |
| <img src="docs/previews/palette-signal.webp" height="40" alt="signal"> | `signal` |
| <img src="docs/previews/palette-terminal.webp" height="40" alt="terminal"> | `terminal` |
| <img src="docs/previews/palette-neon.webp" height="40" alt="neon"> | `neon` |
| <img src="docs/previews/palette-ice.webp" height="40" alt="ice"> | `ice` |
| <img src="docs/previews/palette-colorblind.webp" height="40" alt="colorblind"> | `colorblind` |
| <img src="docs/previews/palette-soft.webp" height="40" alt="soft"> | `soft` |

Each harness reports a little differently; [docs/harnesses.md](docs/harnesses.md) says what each one shows and where it falls short.

## Development

```sh
cargo test && cargo clippy --all-targets
```

CI runs these, and the workshop's tests, on every push to `master` on Apple silicon and Intel Macs.

- [docs/design.md](docs/design.md) explains how the service works and lists its open questions.
- `prototype/` is a workshop page for tuning the settings against the card: `cd prototype && bun install && bun run dev`. It runs what `agent-leds` renders through the firmware's own interpreter, so it shows what the card will do. `bun test` there checks the color model and the rendered programs.
- `bun run previews` in `prototype/` redraws the pictures above; run it after changing rendering, defaults or palettes.
- Pushing a `v*` tag releases: [dist](https://opensource.axo.dev/cargo-dist/) builds the binaries and updates the Homebrew tap.
