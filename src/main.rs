//! agent-leds: shows a queue of agent sessions on a SidePulse Pro.

mod config;
mod daemon;
mod desktop;
mod device;
mod eject;
mod harness;
mod launchd;
mod render;
mod settings;
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
use daemon::Request;
use harness::Harness;
use state::State;

#[derive(Parser)]
#[command(
    version,
    about = "Shows a queue of agent sessions on a SidePulse Pro, the eight-light card in a \
             MacBook's SD slot: red for an error, orange for a question, blue for stopped and \
             unread, green for working",
    after_help = "Run `agent-leds install` once to start the service and register its hooks with \
                  Claude Code, Codex, Grok, Cursor and Junie."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the service at login and register the hooks of every agent harness in use
    #[command(
        after_help = "A harness is in use when its folder is in your home directory: ~/.claude, \
                      ~/.codex, ~/.grok, ~/.cursor or ~/.junie. Other hooks are left as they are. \
                      Run it again after you start using another harness, or after you move or \
                      upgrade the agent-leds binary; running it twice changes nothing."
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

/// A queue to render as the service would, with the saved config unless one is given.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Scene {
    states: Vec<State>,
    /// The state a session has just entered, if the change is one.
    announce: Option<State>,
    /// The queue before the change.
    #[serde(default)]
    from: Vec<State>,
    config: Option<Config>,
}

impl Scene {
    fn read() -> Result<(Scene, Config)> {
        let mut scene: Scene = serde_json::from_str(&stdin()?).context("reading the scene")?;
        let config = match scene.config.take() {
            Some(config) => config,
            None => Config::load()?,
        };
        config.check().map_err(anyhow::Error::msg)?;
        Ok((scene, config))
    }
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Daemon => daemon::run(),
        Command::Install => {
            let exe = std::env::current_exe()?;
            launchd::install(&exe)?;
            let registered = registered(harness::install(&home(), &exe))?;
            println!(
                "The service is running and starts at login. New sessions report to it from: {}.",
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
        Command::Dismiss { session } => daemon::request(&Request::Dismiss { session }).map(drop),
        Command::Palette {
            name: Some(name), ..
        } => daemon::request(&Request::Palette { name }).map(drop),
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
            daemon::request(&Request::Play {
                program: program.trim().to_owned(),
                seconds,
            })
            .map(drop)
        }
        Command::Configure => {
            let config = serde_json::from_str(&stdin()?).context("reading the config")?;
            let saved = daemon::request(&Request::Configure { config })?;
            println!("{saved}");
            Ok(())
        }
        Command::Render => render_scene(),
        Command::Show { seconds } => {
            let (scene, config) = Scene::read()?;
            daemon::request(&Request::Show {
                states: scene.states,
                announce: scene.announce,
                from: scene.from,
                config,
                seconds,
            })
            .map(drop)
        }
        Command::Hook { harness } => {
            // A hook must never disturb the agent: stay silent and succeed whatever happens.
            if let Some(report) = stdin()
                .ok()
                .and_then(|text| serde_json::from_str(&text).ok())
                .and_then(|payload| harness::report(harness, &payload))
            {
                let _ = daemon::send(&report);
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
    let status = daemon::request(&Request::Status)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&status)?);
        return Ok(());
    }
    let now = status["now"].as_u64().unwrap_or_default();
    for item in status["items"].as_array().into_iter().flatten() {
        let age = |since: &Value| match now.saturating_sub(since.as_u64().unwrap_or(now)) / 1000 {
            seconds @ 0..60 => format!("{seconds}s"),
            seconds @ 60..3600 => format!("{}m", seconds / 60),
            seconds => format!("{}h{:02}m", seconds / 3600, seconds % 3600 / 60),
        };
        let weak = match item["weak"].as_bool() {
            Some(true) => "  ended on a question",
            _ => "",
        };
        let read = match &item["read"] {
            Value::Null => String::new(),
            at => format!("  read {} ago", age(at)),
        };
        println!(
            "{:<9} {:>6}  {}  {:<6}  {}{weak}{read}",
            item["state"].as_str().unwrap_or_default(),
            age(&item["since"]),
            item["session"]
                .as_str()
                .unwrap_or_default()
                .chars()
                .take(8)
                .collect::<String>(),
            item["harness"].as_str().unwrap_or_default(),
            item["label"].as_str().unwrap_or_default(),
        );
    }
    if status["items"].as_array().is_none_or(Vec::is_empty) {
        println!("The queue is empty.");
    }
    if status["device"].is_null() {
        println!("No SidePulse volume is mounted.");
    }
    Ok(())
}

/// Shows every setting, shows one, or changes one.
fn configure(key: Option<&str>, values: &[String]) -> Result<()> {
    let status = daemon::request(&Request::Status)?;
    let config: Config =
        serde_json::from_value(status["config"].clone()).context("reading the settings")?;
    if let Some(key) = key
        && !values.is_empty()
    {
        let config = settings::set(&config, key, values)?;
        return daemon::request(&Request::Configure { config }).map(drop);
    }
    let shown: Vec<_> = settings::settings(&config)
        .into_iter()
        .filter(|(name, _)| {
            key.is_none_or(|key| name == key || name.starts_with(&format!("{key}.")))
        })
        .collect();
    if shown.is_empty() {
        anyhow::bail!(
            "no setting named {:?}; see `agent-leds config -h`",
            key.unwrap_or_default()
        );
    }
    for (name, value) in shown {
        println!("{name} = {value}");
    }
    Ok(())
}

fn palettes(json: bool) {
    if json {
        let list: Vec<Value> = PALETTES
            .iter()
            .map(|palette| json!({ "name": palette.name, "about": palette.about, "colors": palette.colors }))
            .collect();
        println!("{}", json!(list));
        return;
    }
    for palette in &PALETTES {
        let colors = State::ALL
            .map(|state| palette.colors.of(state).to_string())
            .join(" ");
        println!("{:<11} {colors}  {}", palette.name, palette.about);
    }
}

fn render_scene() -> Result<()> {
    let (scene, config) = Scene::read()?;
    let slots = render::slots(&scene.states);
    let from = render::slots(&scene.from);
    let steps = render::change(scene.announce, &from, &slots, &config);
    let output = json!({
        "slots": slots,
        "steady": render::steady(&slots, &config),
        "steps": steps,
        "lit": render::balanced(&config),
        "config": config,
    });
    println!("{output}");
    Ok(())
}
