//! leds: shows a queue of agent sessions on a SidePulse Pro.

mod claude;
mod config;
mod daemon;
mod device;
mod launchd;
mod render;
mod state;

use std::io::Read;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use serde::Deserialize;
use serde_json::{Value, json};

use config::{Color, Config, PALETTES, Palette};
use daemon::Request;
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
    /// Start the service at login and register the Claude Code hooks
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
    /// Render a scene read from stdin as JSON; used by the prototype
    Render,
    /// Receive one agent hook event on stdin
    #[command(hide = true)]
    Hook { agent: Agent },
}

#[derive(Clone, ValueEnum)]
enum Agent {
    Claude,
}

/// A queue to render without the daemon. Anything left out comes from the saved config.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Scene {
    states: Vec<State>,
    announce: Option<State>,
    colors: Option<Palette>,
    brightness: Option<u8>,
    front_led: Option<u8>,
    balance: Option<[f64; 3]>,
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Daemon => daemon::run(),
        Command::Install => {
            let exe = std::env::current_exe()?;
            launchd::install(&exe)?;
            claude::install(&exe)?;
            println!(
                "The service is running and starts at login. New Claude Code sessions report to it."
            );
            Ok(())
        }
        Command::Uninstall => {
            claude::uninstall()?;
            launchd::uninstall()?;
            println!("The service is stopped and the Claude Code hooks are removed.");
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
        Command::Render => render_scene(),
        Command::Hook {
            agent: Agent::Claude,
        } => {
            // A hook must never disturb the agent: stay silent and succeed whatever happens.
            if let Some(report) = stdin()
                .ok()
                .and_then(|text| serde_json::from_str(&text).ok())
                .and_then(|event| claude::report(&event))
            {
                let _ = daemon::send(&report);
            }
            Ok(())
        }
    }
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
        let seconds = now.saturating_sub(item["since"].as_u64().unwrap_or(now)) / 1000;
        let age = match seconds {
            0..60 => format!("{seconds}s"),
            60..3600 => format!("{}m", seconds / 60),
            _ => format!("{}h{:02}m", seconds / 3600, seconds % 3600 / 60),
        };
        println!(
            "{:<9} {:>6}  {}  {}",
            item["state"].as_str().unwrap_or_default(),
            age,
            item["session"]
                .as_str()
                .unwrap_or_default()
                .chars()
                .take(8)
                .collect::<String>(),
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
    let mut scene: Scene = serde_json::from_str(&stdin()?).context("reading the scene")?;
    let saved = Config::load()?;
    let config = Config {
        brightness: scene.brightness.unwrap_or(saved.brightness),
        front_led: scene.front_led.unwrap_or(saved.front_led),
        balance: scene.balance.unwrap_or(saved.balance),
        colors: scene.colors.unwrap_or(saved.colors),
    };
    scene.states.sort();
    let slots = render::slots(&scene.states);
    let output = json!({
        "slots": slots,
        "steady": render::steady(&slots, &config),
        "announce": scene.announce.map(|state| render::announce(state, &slots, &config)),
        "announce_ms": render::ANNOUNCE_MS,
        "lit": render::balanced(&config),
        "config": config,
    });
    println!("{output}");
    Ok(())
}
