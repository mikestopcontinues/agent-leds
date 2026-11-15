//! `agent-leds doctor`: checks the things an install most often gets wrong,
//! so that one pasted output says what is going on.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::config::home;
use crate::device;
use crate::harness;
use crate::ipc::{self, Request, Status};
use crate::launchd;

pub fn run() -> Result<()> {
    let exe = std::env::current_exe()?;
    let status = ipc::ask::<Status>(&Request::Status);
    let mut checks = vec![
        ("service".to_owned(), service(&exe, &status)),
        ("card".to_owned(), card(&status)),
    ];
    if let Ok(status) = &status {
        checks.push(("readings".to_owned(), readings(status)));
    }
    for (harness, outcome) in harness::check(&home(), &exe) {
        let outcome = outcome.map(|()| harness.registered().to_owned());
        checks.push((harness.title().to_owned(), outcome));
    }
    checks.push(("SidePulse".to_owned(), sidepulse()));

    let mut failed = 0;
    for (name, outcome) in &checks {
        match outcome {
            Ok(fine) => println!("ok   {name:<11} {fine}"),
            Err(problem) => {
                failed += 1;
                println!("fix  {name:<11} {problem:#}");
            }
        }
    }
    if failed > 0 {
        bail!("{failed} of {} checks need attention", checks.len());
    }
    Ok(())
}

/// Whether launchd runs this binary as the service, and it answers on this version.
fn service(exe: &Path, status: &Result<Status>) -> Result<String> {
    let program = launchd::program()?.context("not installed; run `agent-leds install`")?;
    if program != exe {
        bail!(
            "the service runs {}, not this binary; run `agent-leds install`",
            program.display()
        );
    }
    let status = match status {
        Ok(status) => status,
        Err(error) => bail!("{error:#}; see {}", launchd::log().display()),
    };
    let version = env!("CARGO_PKG_VERSION");
    if status.version != version {
        bail!(
            "the service is on {} and this binary is {version}; it restarts within 15 seconds of an upgrade",
            status.version
        );
    }
    Ok(format!("running {version} from {}", exe.display()))
}

/// Whether the card is mounted and its status can be read.
fn card(status: &Result<Status>) -> Result<String> {
    let volume =
        device::volume().context("no SidePulse volume is mounted; is the card in the SD slot?")?;
    // The service reads the card past the cache; without it, read it here.
    let uptime = match status {
        Ok(status) => status.device_uptime_ms,
        Err(_) => device::uptime_ms().ok(),
    };
    let uptime = uptime.with_context(|| {
        format!(
            "{} is mounted, but its STATUS.TXT cannot be read",
            volume.display()
        )
    })?;
    let minutes = uptime / 60_000;
    Ok(format!(
        "mounted at {}, up {}h{:02}m",
        volume.display(),
        minutes / 60,
        minutes % 60
    ))
}

/// Whether everything the harnesses' files and hooks told was declared and
/// unambiguous. A harness whose format changed shows up here first.
fn readings(status: &Status) -> Result<String> {
    let mut problems = Vec::new();
    if status.undeclared > 0 {
        problems.push(format!(
            "{} observations told signals their harness does not declare",
            status.undeclared
        ));
    }
    for overlap in &status.overlaps {
        let signals: Vec<String> = overlap.signals.iter().map(|s| format!("{s:?}")).collect();
        problems.push(format!(
            "{} told {} together {} times",
            overlap.harness.title(),
            signals.join(" and "),
            overlap.count
        ));
    }
    if !problems.is_empty() {
        bail!(
            "{}; the daemon's log says more, and the harness's format may have changed",
            problems.join("; ")
        );
    }
    Ok("everything the harnesses told was declared and unambiguous".into())
}

/// Whether any of SidePulse's own software is left: its services write to
/// the card too, and their eject guard competes with this one.
fn sidepulse() -> Result<String> {
    let mut found = Vec::new();
    let folders = [
        home().join("Library/LaunchAgents"),
        "/Library/LaunchAgents".into(),
        "/Library/LaunchDaemons".into(),
    ];
    for folder in folders {
        for entry in std::fs::read_dir(&folder).into_iter().flatten().flatten() {
            let path = entry.path();
            if is_sidepulse(&path.to_string_lossy()) {
                found.push(path.display().to_string());
            }
        }
    }
    let processes = Command::new("pgrep").args(["-lif", "sidepulse"]).output()?;
    let processes = String::from_utf8_lossy(&processes.stdout);
    found.extend(
        processes
            .lines()
            .map(|process| format!("process {process}")),
    );
    if !found.is_empty() {
        bail!(
            "its own software is still installed, and writes to the card too: {}",
            found.join(", ")
        );
    }
    Ok("none of its own software is installed".into())
}

fn is_sidepulse(path: &str) -> bool {
    Path::new(path)
        .file_name()
        .is_some_and(|name| name.to_string_lossy().to_lowercase().contains("sidepulse"))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::config::Config;
    use crate::fsm::Signal;
    use crate::harness::Harness;
    use crate::ipc::Overlap;

    fn status(undeclared: u64, overlaps: Vec<Overlap>) -> Status {
        Status {
            version: "0".into(),
            items: Vec::new(),
            now: 0,
            config: Config::default(),
            device: None,
            device_uptime_ms: None,
            undeclared,
            overlaps,
        }
    }

    #[test]
    fn readings_that_overlap_or_are_undeclared_need_attention() {
        assert!(readings(&status(0, Vec::new())).is_ok());
        let overlap = Overlap {
            harness: Harness::Claude,
            signals: BTreeSet::from([Signal::TurnEnded, Signal::PermissionAsked]),
            count: 3,
        };
        let problem = readings(&status(2, vec![overlap])).unwrap_err().to_string();
        assert!(problem.contains("2 observations"), "{problem}");
        assert!(
            problem.contains("Claude Code told PermissionAsked and TurnEnded together 3 times"),
            "{problem}"
        );
    }

    #[test]
    fn sidepulse_is_found_by_the_name_of_its_files() {
        assert!(is_sidepulse(
            "/Users/me/Library/LaunchAgents/io.sidepulse.agentstatus.plist"
        ));
        assert!(!is_sidepulse(
            "/Users/me/Library/LaunchAgents/io.github.mikestopcontinues.agent-leds.plist"
        ));
    }
}
