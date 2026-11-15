//! Runs the daemon at login as a per-user LaunchAgent.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

use crate::config::{home, state_dir};
use crate::files;

const LABEL: &str = "io.github.mikestopcontinues.agent-leds";

fn plist_path() -> PathBuf {
    home().join(format!("Library/LaunchAgents/{LABEL}.plist"))
}

fn domain() -> String {
    // SAFETY: getuid has no preconditions and cannot fail.
    format!("gui/{}", unsafe { libc::getuid() })
}

/// Where the daemon's errors go.
pub fn log() -> PathBuf {
    state_dir().join("daemon.log")
}

/// A variable in the environment launchd starts the user's agents with, as
/// `launchctl setenv` sets it, if it is set. A shell's own exports never
/// reach it.
pub fn getenv(name: &str) -> Option<OsString> {
    let output = Command::new("launchctl")
        .args(["getenv", name])
        .output()
        .ok()?;
    let value = output.stdout.strip_suffix(b"\n").unwrap_or(&output.stdout);
    (output.status.success() && !value.is_empty()).then(|| OsStr::from_bytes(value).to_owned())
}

/// The binary the installed LaunchAgent runs, if one is installed.
pub fn program() -> Result<Option<PathBuf>> {
    let Some(xml) = files::read_optional(&plist_path())? else {
        return Ok(None);
    };
    let agent: plist::Dictionary = plist::from_bytes(xml.as_bytes())?;
    let program = agent
        .get("ProgramArguments")
        .and_then(plist::Value::as_array)
        .and_then(|arguments| arguments.first()?.as_string());
    Ok(program.map(PathBuf::from))
}

/// The LaunchAgent: run `exe daemon` at login, again whenever it stops,
/// with its errors in `log`.
fn agent(exe: &Path, log: &Path) -> Result<String> {
    #[derive(serde::Serialize)]
    #[serde(rename_all = "PascalCase")]
    struct Agent<'a> {
        label: &'a str,
        program_arguments: [&'a Path; 2],
        run_at_load: bool,
        keep_alive: bool,
        standard_error_path: &'a Path,
    }
    let agent = Agent {
        label: LABEL,
        program_arguments: [exe, Path::new("daemon")],
        run_at_load: true,
        keep_alive: true,
        standard_error_path: log,
    };
    let mut xml = Vec::new();
    plist::to_writer_xml(&mut xml, &agent)?;
    Ok(String::from_utf8(xml)?)
}

/// Installs the LaunchAgent and (re)starts the daemon from `exe`.
pub fn install(exe: &Path) -> Result<()> {
    fs::create_dir_all(state_dir())?;
    let plist = agent(exe, &log())?;
    let path = plist_path();
    files::replace(&path, &plist)?;

    stop();
    let started = Command::new("launchctl")
        .args(["bootstrap", &domain()])
        .arg(&path)
        .status()?;
    if !started.success() {
        bail!("launchctl could not start {LABEL}");
    }
    Ok(())
}

pub fn uninstall() -> Result<()> {
    stop();
    files::remove(&plist_path())
}

/// Stops the running daemon, if launchd has one, and waits for launchd to
/// let it go: `bootout` can return while the service is still being removed,
/// and a `bootstrap` then fails with an input/output error.
fn stop() {
    let service = format!("{}/{LABEL}", domain());
    let _ = Command::new("launchctl")
        .args(["bootout", &service])
        .output();
    let loaded = || {
        Command::new("launchctl")
            .args(["print", &service])
            .output()
            .is_ok_and(|output| output.status.success())
    };
    let started = Instant::now();
    while loaded() && started.elapsed() < Duration::from_secs(10) {
        thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_agent_runs_the_daemon_at_login_and_keeps_it_running() {
        let xml = agent(
            Path::new("/opt/My & Tools/agent-leds"),
            Path::new("/x/daemon.log"),
        )
        .unwrap();
        let agent: plist::Dictionary = plist::from_bytes(xml.as_bytes()).unwrap();
        assert_eq!(agent["Label"].as_string(), Some(LABEL));
        let arguments: Vec<_> = agent["ProgramArguments"]
            .as_array()
            .unwrap()
            .iter()
            .map(|argument| argument.as_string().unwrap())
            .collect();
        assert_eq!(arguments, ["/opt/My & Tools/agent-leds", "daemon"]);
        assert_eq!(agent["RunAtLoad"].as_boolean(), Some(true));
        assert_eq!(agent["KeepAlive"].as_boolean(), Some(true));
        assert_eq!(
            agent["StandardErrorPath"].as_string(),
            Some("/x/daemon.log")
        );
    }
}
