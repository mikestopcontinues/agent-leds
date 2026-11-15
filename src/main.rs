//! agent-leds: shows a queue of agent sessions on a SidePulse Pro.

mod config;
// The daemon reads only the declarations; the document is rendered by its test.
#[allow(dead_code)]
mod coverage;
mod daemon;
mod desktop;
mod device;
mod doctor;
mod eject;
mod files;
mod fsm;
mod harness;
mod ipc;
mod launchd;
mod liveness;
mod player;
mod queue;
mod render;
mod settings;
mod source;
mod state;

#[cfg(not(target_os = "macos"))]
compile_error!("agent-leds runs only on macOS: the card sits in a MacBook's SD slot");

use std::io::Read;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde::Deserialize;
use serde_json::{Value, json};

use config::{Config, PALETTES, home};
use harness::Harness;
use ipc::{Request, Status};
use render::Scene;
use source::Hook;
use state::State;

#[derive(Parser)]
#[command(
    version,
    about = "Shows a queue of agent sessions on a SidePulse Pro, the eight-light card in a \
             MacBook's SD slot: red for an error, orange for a question, blue for stopped and \
             unread, green for working",
    after_help = "Run `agent-leds install` once to start the service. It reads Claude Code's own \
                  files, and registers its hooks with Codex, Grok, Cursor, Junie, OpenCode and Pi."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the service at login, and register exactly the hooks each agent harness in use needs
    #[command(
        after_help = "A harness is in use when its folder is in your home directory: ~/.claude, \
                      ~/.codex, ~/.grok, ~/.cursor, ~/.junie, ~/.config/opencode or ~/.pi. Other hooks \
                      are left as they are, and hooks an older agent-leds registered that this one does not use \
                      are removed. Run it again after you start using another harness, or after you move the \
                      agent-leds binary; running it twice changes nothing."
    )]
    Install,
    /// Stop the service and remove its hooks, and nothing else
    Uninstall,
    /// Show the queue: state, age, session, harness and folder of each light
    Status {
        /// Print the queue, the settings and the card's state as JSON
        #[arg(long)]
        json: bool,
    },
    /// Clear every light that is not working, or one session's
    Dismiss {
        /// The session to clear, by the start of its id as `agent-leds status` shows it
        session: Option<String>,
    },
    /// List the palettes, or switch to one
    Palette {
        /// The palette to switch to, such as `bright`
        name: Option<String>,
        /// List the palettes as JSON
        #[arg(long, conflicts_with = "name")]
        json: bool,
    },
    /// Show the settings, or change one
    #[command(after_help = settings::help())]
    Config {
        /// The setting to show or change, such as `snake.step_ms`; every setting without one
        key: Option<String>,
        /// The new value; a list such as `balance` takes one value per item
        #[arg(requires = "key", allow_negative_numbers = true)]
        value: Vec<String>,
    },
    /// Show a program in the card's own LEDS.LED language, then return to the queue
    Play {
        /// The file that holds the program, or - to read it from stdin
        file: PathBuf,
        /// How long to show it
        #[arg(long, default_value_t = 5.0)]
        seconds: f64,
    },
    /// Check the service, the card and the hooks, and say what needs fixing
    Doctor,
    /// Run the service in the foreground; launchd runs this
    Daemon,
    /// Replace every setting with a config read from stdin as JSON; used by the workshop
    #[command(hide = true)]
    Configure,
    /// Render a scene read from stdin as JSON; used by the workshop
    #[command(hide = true)]
    Render,
    /// Show a scene read from stdin as JSON on the device, then return to the queue; used by the workshop
    #[command(hide = true)]
    Show {
        #[arg(long, default_value_t = 10.0)]
        seconds: f64,
    },
    /// Receive one agent hook event on stdin
    #[command(hide = true)]
    Hook { harness: Harness },
}

/// A scene to render as the service would, with the saved config unless one is given.
#[derive(Deserialize)]
struct Input {
    #[serde(flatten)]
    scene: Scene,
    config: Option<Config>,
}

impl Input {
    fn read() -> Result<(Scene, Config)> {
        let input: Input = serde_json::from_str(&stdin()?).context("reading the scene")?;
        let config = match input.config {
            Some(config) => config,
            None => Config::load()?,
        };
        config.check()?;
        Ok((input.scene, config))
    }
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Daemon => daemon::run(),
        Command::Doctor => doctor::run(),
        Command::Install => {
            let exe = std::env::current_exe()?;
            launchd::install(&exe)?;
            let registered = registered(harness::install(&home(), &exe))?;
            println!(
                "The service is running and starts at login. It follows the sessions of: {}.",
                registered.join(", ")
            );
            Ok(())
        }
        Command::Uninstall => {
            let removed = registered(harness::uninstall(&home()));
            launchd::uninstall()?;
            removed?;
            println!("The service is stopped and its hooks are removed.");
            Ok(())
        }
        Command::Status { json } => status(json),
        Command::Dismiss { session } => {
            if session.as_deref() == Some("") {
                // An empty prefix would name every session, working ones too.
                anyhow::bail!("name the session to clear by the start of its id, or name none");
            }
            ipc::request(&Request::Dismiss { session }).map(drop)
        }
        Command::Palette {
            name: Some(name), ..
        } => ipc::request(&Request::Palette { name }).map(drop),
        Command::Palette { name: None, json } => {
            palettes(json);
            Ok(())
        }
        Command::Config { key, value } => configure(key.as_deref(), &value),
        Command::Play { file, seconds } => {
            let program = if file.as_os_str() == "-" {
                stdin()?
            } else {
                std::fs::read_to_string(&file)?
            };
            ipc::request(&Request::Play {
                program: program.trim().to_owned(),
                seconds,
            })
            .map(drop)
        }
        Command::Configure => {
            let config = serde_json::from_str(&stdin()?).context("reading the config")?;
            let saved = ipc::request(&Request::Configure { config })?;
            println!("{saved}");
            Ok(())
        }
        Command::Render => render_scene(),
        Command::Show { seconds } => {
            let (scene, config) = Input::read()?;
            ipc::request(&Request::Show {
                scene,
                config,
                seconds,
            })
            .map(drop)
        }
        Command::Hook { harness } => {
            // A hook must never disturb the agent: stay silent and succeed whatever happens.
            if let Some(payload) = stdin()
                .ok()
                .and_then(|text| serde_json::from_str(&text).ok())
            {
                // The processes the hook runs under say which one runs the session.
                let ancestry = liveness::ancestry(8);
                let hook = Hook {
                    harness,
                    payload,
                    ancestry,
                };
                let _ = ipc::send(&Request::Hook(hook));
            }
            Ok(())
        }
    }
}

/// The harnesses an install or uninstall went through for, after saying
/// what went wrong with the others.
fn registered(outcomes: Vec<(Harness, Result<()>)>) -> Result<Vec<&'static str>> {
    let mut registered = Vec::new();
    let mut failed = Vec::new();
    for (harness, outcome) in outcomes {
        match outcome {
            Ok(()) => registered.push(harness.title()),
            Err(error) => {
                eprintln!("agent-leds: {}: {error:#}", harness.title());
                failed.push(harness.title());
            }
        }
    }
    if !failed.is_empty() {
        anyhow::bail!("the hooks of {} need attention", failed.join(", "));
    }
    Ok(registered)
}

fn stdin() -> Result<String> {
    let mut text = String::new();
    std::io::stdin().read_to_string(&mut text)?;
    Ok(text)
}

fn status(json: bool) -> Result<()> {
    let status = ipc::request(&Request::Status)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&status)?);
        return Ok(());
    }
    let status: Status = serde_json::from_value(status).context("reading the queue")?;
    print!("{}", status_text(&status));
    Ok(())
}

/// The queue as `agent-leds status` prints it: a line per session.
fn status_text(status: &Status) -> String {
    let age = |since: u64| match status.now.saturating_sub(since) / 1000 {
        seconds @ 0..60 => format!("{seconds}s"),
        seconds @ 60..3600 => format!("{}m", seconds / 60),
        seconds => format!("{}h{:02}m", seconds / 3600, seconds % 3600 / 60),
    };
    let mut text = String::new();
    for item in &status.items {
        let weak = if item.weak {
            "  ended on a question"
        } else {
            ""
        };
        let read = item
            .read
            .map_or(String::new(), |at| format!("  read {} ago", age(at)));
        let session: String = item.session.chars().take(8).collect();
        let state = format!("{:?}", item.state).to_lowercase();
        let harness = format!("{:?}", item.harness).to_lowercase();
        text += &format!(
            "{state:<9} {:>6}  {session}  {harness:<8}  {}{weak}{read}\n",
            age(item.since),
            item.label,
        );
    }
    if status.items.is_empty() {
        text += "The queue is empty.\n";
    }
    if status.device.is_none() {
        text += "No SidePulse volume is mounted.\n";
    }
    text
}

/// Shows every setting, shows one, or changes one.
fn configure(key: Option<&str>, values: &[String]) -> Result<()> {
    let config = ipc::ask::<Status>(&Request::Status)?.config;
    if let Some(key) = key
        && !values.is_empty()
    {
        let config = settings::set(&config, key, values)?;
        return ipc::request(&Request::Configure { config }).map(drop);
    }
    for (name, value) in settings::shown(&config, key)? {
        println!("{name} = {value}");
    }
    Ok(())
}

fn palettes(json: bool) {
    if json {
        let list: Vec<Value> = PALETTES
            .iter()
            .map(|palette| json!({ "name": palette.name, "colors": palette.colors }))
            .collect();
        println!("{}", json!(list));
        return;
    }
    for palette in &PALETTES {
        let colors = State::ALL
            .map(|state| palette.colors.of(state).to_string())
            .join(" ");
        println!("{:<11} {colors}", palette.name);
    }
}

fn render_scene() -> Result<()> {
    let (scene, config) = Input::read()?;
    let shown = scene.render(&config);
    let output = json!({
        "steady": render::steady(&shown.slots, &config),
        "slots": shown.slots,
        "steps": shown.steps,
        "lit": render::balanced(&config),
        "config": config,
    });
    println!("{output}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::View;

    #[test]
    fn status_lists_each_session_with_its_age() {
        let minute = 60_000;
        let view = |harness, session: &str, label: &str, state, since, weak| View {
            harness,
            session: session.into(),
            label: label.into(),
            state,
            since,
            weak,
            read: None,
        };
        let status = Status {
            version: "0".into(),
            items: vec![
                view(
                    Harness::Claude,
                    "abcdefghij",
                    "web",
                    State::Question,
                    2 * minute,
                    true,
                ),
                view(
                    Harness::Codex,
                    "0123456789",
                    "app",
                    State::Working,
                    0,
                    false,
                ),
            ],
            now: 3 * 3600 * 1000 + 5 * minute,
            config: Config::default(),
            device: Some("/Volumes/SidePulse".into()),
            device_uptime_ms: Some(1),
            undeclared: 0,
            overlaps: Vec::new(),
        };
        let text = status_text(&status);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "{text}");
        assert!(lines[0].starts_with("question") && lines[0].contains("abcdefgh  claude    web"));
        assert!(lines[0].ends_with("ended on a question"));
        assert!(lines[1].starts_with("working") && lines[1].contains("3h05m"));
    }

    #[test]
    fn status_says_when_nothing_is_there() {
        let status = Status {
            version: "0".into(),
            items: Vec::new(),
            now: 0,
            config: Config::default(),
            device: None,
            device_uptime_ms: None,
            undeclared: 0,
            overlaps: Vec::new(),
        };
        assert_eq!(
            status_text(&status),
            "The queue is empty.\nNo SidePulse volume is mounted.\n"
        );
    }

    #[test]
    fn every_command_has_help() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
        for command in Cli::command().get_subcommands() {
            assert!(
                command.is_hide_set() || command.get_about().is_some(),
                "{} has no help",
                command.get_name()
            );
        }
    }
}
