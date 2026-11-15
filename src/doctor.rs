//! `agent-leds doctor`: checks the things an install most often gets wrong,
//! so that one pasted output says what is going on.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::config::home;
use crate::daemon;
use crate::device;
use crate::fsm::{self, Signal};
use crate::harness;
use crate::ipc::{self, Request, Status};
use crate::launchd;
use crate::liveness::{Kernel, Liveness, System};
use crate::queue::{Key, Queue};
use crate::source::{self, Observation, Source};

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
    let home = home();
    // The sessions only a hook could bind, as the daemon last saved them. A
    // saved queue the daemon cannot read, it ignores too.
    let bound = daemon::saved_queue()
        .map(|saved| saved.bound())
        .unwrap_or_default();
    for (harness, hooks) in harness::check(&home, &exe) {
        let mut source = source::of(harness, &home, &source::junie::home());
        let files = files(source.as_mut(), &System, &bound, &home);
        let hooks = hooks.map(|()| harness.registered().to_owned());
        checks.push((harness.title().to_owned(), both(files, hooks)));
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

/// What a harness's files show, read by its source as the daemon reads them
/// at start: how many sessions run and where they stand, and where from.
/// The source keeps its offsets in this process, and nothing is written.
fn files(
    source: &mut dyn Source,
    k: &dyn Kernel,
    bound: &[(Key, Liveness)],
    home: &Path,
) -> Result<String> {
    let seeded = source::seed(source, k, bound);
    let roots: Vec<PathBuf> = source
        .roots()
        .into_iter()
        .filter(|root| root.exists())
        .collect();
    for root in &roots {
        std::fs::read_dir(root).with_context(|| format!("{} cannot be read", tilde(root, home)))?;
    }
    let seeded = seeded.context("reading its files failed")?;
    if roots.is_empty() {
        return Ok("no session files yet".into());
    }
    let roots: Vec<String> = roots.iter().map(|root| tilde(root, home)).collect();
    Ok(format!("{} from {}", sessions(&seeded)?, roots.join(", ")))
}

/// The sessions a seed started, by the light each shows, or what the seed
/// told that its harness does not declare or that overlaps.
fn sessions(seeded: &[Observation]) -> Result<String> {
    let mut queue = Queue::default();
    let mut problems = BTreeSet::new();
    for o in seeded {
        let undeclared = o.undeclared();
        if !undeclared.is_empty() {
            problems.insert(format!(
                "its files told {} through {:?}, which it does not declare",
                names(undeclared),
                o.channel
            ));
            continue;
        }
        let signals = o.event.signals();
        if fsm::overlaps(queue.phase(&o.key), &signals) {
            problems.insert(format!("its files told {} together", names(signals)));
        }
        queue.apply(o, 0);
    }
    if !problems.is_empty() {
        let problems: Vec<String> = problems.into_iter().collect();
        bail!(
            "{}; the harness's format may have changed",
            problems.join("; ")
        );
    }
    // Idle sorts last, after every light.
    let mut lights = BTreeMap::new();
    for session in queue.sessions() {
        let light = session.phase.state();
        *lights.entry((light.is_none(), light)).or_insert(0) += 1;
    }
    let count = queue.sessions().len();
    let lights: Vec<String> = lights
        .into_iter()
        .map(|((_, light), n)| {
            let name = light.map_or("idle".to_owned(), |l| format!("{l:?}").to_lowercase());
            if count == 1 {
                name
            } else {
                format!("{n} {name}")
            }
        })
        .collect();
    Ok(match count {
        0 => "no sessions running".to_owned(),
        1 => format!("1 session running ({})", lights.join(", ")),
        _ => format!("{count} sessions running ({})", lights.join(", ")),
    })
}

fn names(signals: impl IntoIterator<Item = Signal>) -> String {
    let names: Vec<String> = signals.into_iter().map(|s| format!("{s:?}")).collect();
    names.join(" and ")
}

/// A path as the user writes it, from `~` when it is in the home directory.
fn tilde(path: &Path, home: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

/// Two checks of one thing as one line, each saying what it found or what
/// is wrong, that needs attention if either does.
fn both(first: Result<String>, second: Result<String>) -> Result<String> {
    let fine = first.is_ok() && second.is_ok();
    let said: Vec<String> = [first, second]
        .into_iter()
        .map(|outcome| outcome.unwrap_or_else(|problem| format!("{problem:#}")))
        .collect();
    let said = said.join("; ");
    if fine { Ok(said) } else { bail!("{said}") }
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
    use crate::fsm::Channel;
    use crate::harness::Harness;
    use crate::ipc::Overlap;
    use crate::source::Event;

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

    /// One process, started when `CLAUDE_SESSION`'s `procStart` says.
    struct Running(i32);

    impl Kernel for Running {
        fn started_us(&self, pid: i32) -> Option<u64> {
            (pid == self.0).then_some(1_794_693_357_250_000)
        }

        fn locked(&self, path: &Path) -> std::io::Result<bool> {
            std::fs::metadata(path).map(|_| false)
        }
    }

    /// The source of `harness` on `home`, as the daemon makes it.
    fn source_of(harness: Harness, home: &Path) -> Box<dyn Source> {
        source::of(harness, home, &home.join(".junie"))
    }

    fn write(home: &Path, file: &str, text: &str) {
        crate::files::replace(&home.join(file), text).unwrap();
    }

    /// The session file of tests/fixtures/claude/started.file.jsonl.
    const CLAUDE_SESSION: &str = r#"{"pid": 90859, "sessionId": "e8f843a7-1f54-44e3-b14d-99f53e9cec7a", "cwd": "/Users/me/Code/app", "startedAt": 1794693357235, "procStart": "Sat Nov 14 21:55:57 2026", "version": "2.1.325", "kind": "interactive", "status": "idle"}"#;

    #[test]
    fn files_say_how_many_sessions_run_and_from_where() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        let files = |kernel: &dyn Kernel| {
            let mut claude = source_of(Harness::Claude, home);
            files(claude.as_mut(), kernel, &[], home)
        };
        assert_eq!(files(&Running(1)).unwrap(), "no session files yet");
        write(home, ".claude/sessions/90859.json", CLAUDE_SESSION);
        assert_eq!(
            files(&Running(1)).unwrap(),
            "no sessions running from ~/.claude/sessions"
        );
        assert_eq!(
            files(&Running(90859)).unwrap(),
            "1 session running (idle) from ~/.claude/sessions"
        );
    }

    #[test]
    fn files_that_cannot_be_read_need_attention() {
        use std::os::unix::fs::PermissionsExt;

        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        write(home, ".grok/active_sessions.json", "not json");
        let mut grok = source_of(Harness::Grok, home);
        let problem = files(grok.as_mut(), &Running(1), &[], home).unwrap_err();
        let problem = format!("{problem:#}");
        assert!(
            problem.starts_with("reading its files failed: parsing"),
            "{problem}"
        );

        write(home, ".claude/sessions/90859.json", CLAUDE_SESSION);
        let sessions = home.join(".claude/sessions");
        let mode = |mode| {
            std::fs::set_permissions(&sessions, std::fs::Permissions::from_mode(mode)).unwrap()
        };
        mode(0o000);
        let mut claude = source_of(Harness::Claude, home);
        let problem = files(claude.as_mut(), &Running(90859), &[], home);
        mode(0o755);
        let problem = format!("{:#}", problem.unwrap_err());
        assert!(
            problem.starts_with("~/.claude/sessions cannot be read: "),
            "{problem}"
        );
    }

    #[test]
    fn sessions_count_by_light_and_refuse_what_is_undeclared() {
        let start = |id: &str| {
            Observation::new(
                Harness::Claude,
                id,
                Channel::File,
                Event::Started(source::Start {
                    cwd: "/Users/me/Code/app".into(),
                    liveness: Liveness::Lock {
                        path: "/nowhere".into(),
                    },
                }),
                None,
            )
        };
        let told = |id: &str, signal, channel| {
            Observation::new(Harness::Claude, id, channel, Event::one(signal), None)
        };
        let seeded = [
            start("a"),
            start("b"),
            start("c"),
            told("b", Signal::Prompted, Channel::File),
            told("c", Signal::Prompted, Channel::File),
        ];
        assert_eq!(
            sessions(&seeded).unwrap(),
            "3 sessions running (2 working, 1 idle)"
        );
        // Claude Code declares no hooks.
        let hooked = [start("a"), told("a", Signal::Prompted, Channel::Hook)];
        assert_eq!(
            sessions(&hooked).unwrap_err().to_string(),
            "its files told Prompted through Hook, which it does not declare; the harness's \
             format may have changed"
        );
    }

    #[test]
    fn one_line_holds_both_checks_of_a_harness() {
        let ok = |text: &str| Ok(text.to_owned());
        let problem = |text: &str| Err(anyhow::anyhow!("{text}"));
        assert_eq!(both(ok("a"), ok("b")).unwrap(), "a; b");
        // What the files show stays beside a hook that needs fixing.
        assert_eq!(both(ok("a"), problem("b")).unwrap_err().to_string(), "a; b");
        assert_eq!(
            both(problem("a"), problem("b")).unwrap_err().to_string(),
            "a; b"
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
