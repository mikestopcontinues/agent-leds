//! leds: shows a queue of agent sessions on a SidePulse Pro.

mod config;
mod daemon;
mod desktop;
mod device;
mod harness;
mod launchd;
mod render;
mod state;

use std::io::Read;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde::Deserialize;
use serde_json::{Value, json};

use config::{Color, Config, PALETTES, home};
use daemon::Request;
use harness::Harness;
use state::State;

#[derive(Parser)]
#[command(version, about = "Shows a queue of agent sessions on a SidePulse Pro")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the service in the foreground (launchd runs this)
    Daemon,
    /// Start the service at login and register the hooks of every agent harness in use
    Install,
    /// Stop the service and remove the hooks
    Uninstall,
    /// Show the queue
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Clear everything that is not working, or one session by id prefix
    Dismiss { session: Option<String> },
    /// Set the color of one state, e.g. `leds color question ff5000`
    Color { state: State, color: Color },
    /// List the palettes, or switch to one
    Palette {
        name: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Set overall brightness, 0-255
    Brightness { level: u8 },
    /// Set which light shows the front of the queue: 0 or 7
    Front { led: u8 },
    /// Set how bright the red, green and blue dies look relative to each other,
    /// which is what keeps every state equally bright
    Balance { red: f64, green: f64, blue: f64 },
    /// Show a LEDS.LED program from a file (or - for stdin), then return to the queue
    Play {
        file: PathBuf,
        #[arg(long, default_value_t = 5.0)]
        seconds: f64,
    },
    /// Replace every setting with a config read from stdin as JSON; used by the workshop
    Configure,
    /// Render a scene read from stdin as JSON; used by the workshop
    Render,
    /// Show a scene read from stdin as JSON on the device, then return to the queue; used by the workshop
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
    /// The state a session has just entered, to announce the change.
    announce: Option<State>,
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
        Command::Color { state, color } => {
            daemon::request(&Request::Color { state, color }).map(drop)
        }
        Command::Palette {
            name: Some(name), ..
        } => daemon::request(&Request::Palette { name }).map(drop),
        Command::Palette { name: None, json } => {
            palettes(json);
            Ok(())
        }
        Command::Brightness { level } => daemon::request(&Request::Brightness { level }).map(drop),
        Command::Front { led } => daemon::request(&Request::Front { led }).map(drop),
        Command::Balance { red, green, blue } => daemon::request(&Request::Balance {
            weights: [red, green, blue],
        })
        .map(drop),
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
                eprintln!("leds: {}: {error:#}", harness.title());
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
    let steps = scene
        .announce
        .map_or_else(Vec::new, |state| render::announce(state, &slots, &config));
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
