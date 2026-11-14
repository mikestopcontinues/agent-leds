//! Runs the daemon at login as a per-user LaunchAgent.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::config::{home, state_dir};

const LABEL: &str = "io.github.mikestopcontinues.agent-leds";

fn plist_path() -> PathBuf {
    home().join(format!("Library/LaunchAgents/{LABEL}.plist"))
}

fn domain() -> String {
    // SAFETY: getuid has no preconditions and cannot fail.
    format!("gui/{}", unsafe { libc::getuid() })
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Installs the LaunchAgent and (re)starts the daemon from `exe`.
pub fn install(exe: &Path) -> Result<()> {
    fs::create_dir_all(state_dir())?;
    let log = state_dir().join("daemon.log");
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>daemon</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardErrorPath</key>
    <string>{log}</string>
</dict>
</plist>
"#,
        exe = escape(&exe.to_string_lossy()),
        log = escape(&log.to_string_lossy()),
    );
    let path = plist_path();
    fs::create_dir_all(path.parent().unwrap())?;
    fs::write(&path, plist).with_context(|| format!("writing {}", path.display()))?;

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
    match fs::remove_file(plist_path()) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.into()),
        _ => Ok(()),
    }
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
