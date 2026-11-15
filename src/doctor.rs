//! `agent-leds doctor`: checks the things an install most often gets wrong,
//! so that one pasted output says what is going on.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::config::home;
use crate::daemon;
use crate::device;
use crate::fsm::{Channel, Signal};
use crate::harness::{self, Harness};
use crate::ipc::{self, Request, Status};
use crate::launchd;
use crate::liveness::{Kernel, System};
use crate::queue::{Problem, Queue};
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
    // The queue as the daemon last saved it, which it restores what the
    // files show against. One it cannot read, it ignores too.
    let saved = daemon::saved_queue();
    checks.push(("queue".to_owned(), queue(&saved, &home)));
    let saved = saved.unwrap_or_default();
    // Junie's folder as the service finds it, from launchd's environment.
    let junie = source::junie::folder(launchd::getenv("JUNIE_HOME").as_deref());
    for (harness, hooks) in harness::check(&home, &exe) {
        let mut source = source::of(harness, &home, &junie);
        let mut files = files(source.as_mut(), &System, &saved, &home);
        if harness == Harness::Junie
            && let Some(problem) = elsewhere(&junie, &source::junie::home(), &home)
        {
            files = both(files, Err(anyhow::anyhow!(problem)));
        }
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

/// Whether the daemon can read the queue it saved, which keeps what the
/// files cannot say across a restart: read marks, dismissals, and the
/// sessions only a hook or plugin told of.
fn queue(saved: &Result<Queue>, home: &Path) -> Result<String> {
    let path = tilde(&daemon::queue_path(), home);
    let saved = match saved {
        Ok(saved) => saved,
        Err(error) => {
            let bound: Vec<&str> = Harness::ALL
                .into_iter()
                .filter(|&harness| hook_bound(harness))
                .map(Harness::title)
                .collect();
            let (last, rest) = bound.split_last().expect("some harness is hook-bound");
            bail!(
                "{path} cannot be read, so the daemon ignores it when it starts, and the \
                 sessions of {} and {last} show only once their hooks or plugin report them \
                 again: {error:#}",
                rest.join(", ")
            );
        }
    };
    Ok(match saved.sessions().len() {
        0 => format!("no sessions saved in {path}"),
        1 => format!("1 session saved in {path}"),
        n => format!("{n} sessions saved in {path}"),
    })
}

/// What is wrong when Junie, run from this shell, keeps its folder at
/// `here` while the service reads `service`: a `JUNIE_HOME` exported in a
/// shell's profile never reaches a LaunchAgent.
fn elsewhere(service: &Path, here: &Path, home: &Path) -> Option<String> {
    (service != here).then(|| {
        format!(
            "JUNIE_HOME here is {}, which the service does not see; run \
             `launchctl setenv JUNIE_HOME {}`, then `agent-leds install` to restart it",
            tilde(here, home),
            here.display()
        )
    })
}

/// Whether only a hook or plugin can start a session of `harness`, whose
/// files name no process.
fn hook_bound(harness: Harness) -> bool {
    !harness::declares(harness, Signal::Started, Channel::File)
}

/// What a harness's files show, read by its source as the daemon reads them
/// at start, and restored against the saved queue as the daemon restores
/// them: how many sessions run and where they stand, and where from. The
/// source keeps its offsets in this process, and nothing is written.
fn files(source: &mut dyn Source, k: &dyn Kernel, saved: &Queue, home: &Path) -> Result<String> {
    let seeded = source::seed(source, k, &saved.bound());
    let mut roots = Vec::new();
    for root in source.roots() {
        // A folder whose parent cannot be searched cannot be told from one
        // that is not there, except by the error.
        let exists = root
            .try_exists()
            .with_context(|| format!("{} cannot be read", tilde(&root, home)))?;
        if exists {
            std::fs::read_dir(&root)
                .with_context(|| format!("{} cannot be read", tilde(&root, home)))?;
            roots.push(root);
        }
    }
    let seeded = seeded.context("reading its files failed")?;
    if roots.is_empty() {
        return Ok("no session files yet".into());
    }
    let roots: Vec<String> = roots.iter().map(|root| tilde(root, home)).collect();
    let harness = source.harness();
    let sessions = if hook_bound(harness) {
        let by = match harness {
            Harness::OpenCode | Harness::Pi => "its plugin",
            _ => "its hooks",
        };
        sessions(
            &seeded.observations,
            saved,
            &format!("{by} reported still running"),
        )?
    } else {
        sessions(&seeded.observations, saved, "running")?
    };
    let said = format!("{sessions}, read from {}", roots.join(", "));
    // A file it could not make sense of may be a session it missed.
    if seeded.unread.is_empty() {
        return Ok(said);
    }
    let mut unread: Vec<String> = seeded
        .unread
        .iter()
        .take(UNREAD_SHOWN)
        .map(|unread| format!("{}: {}", tilde(&unread.path, home), unread.error))
        .collect();
    if seeded.unread.len() > UNREAD_SHOWN {
        unread.push(format!(
            "and {} more files",
            seeded.unread.len() - UNREAD_SHOWN
        ));
    }
    bail!(
        "{said}; but could not read {}; the harness's format may have changed",
        unread.join("; ")
    )
}

/// How many of the files a seed could not read are named.
const UNREAD_SHOWN: usize = 3;

/// The sessions a seed started, restored against the saved queue, by the
/// light each shows, or what the seed told that its harness does not
/// declare or that overlaps. `running` says how they are known to run.
fn sessions(seeded: &[Observation], saved: &Queue, running: &str) -> Result<String> {
    let mut fresh = Queue::default();
    let mut problems = BTreeSet::new();
    for o in seeded {
        match fresh.admit(o, 0).1 {
            None => {}
            Some(Problem::Undeclared(undeclared)) => {
                problems.insert(format!(
                    "its files told {} through {:?}, which it does not declare",
                    names(undeclared),
                    o.channel
                ));
            }
            Some(Problem::Overlap { .. }) => {
                problems.insert(format!(
                    "its files told {} together",
                    names(o.event.signals())
                ));
            }
        }
    }
    if !problems.is_empty() {
        let problems: Vec<String> = problems.into_iter().collect();
        bail!(
            "{}; the harness's format may have changed",
            problems.join("; ")
        );
    }
    let queue = Queue::restore(saved.clone(), fresh);
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
        0 => format!("no sessions {running}"),
        1 => format!("1 session {running} ({})", lights.join(", ")),
        _ => format!("{count} sessions {running} ({})", lights.join(", ")),
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
    use crate::ipc::Overlap;
    use crate::liveness::Liveness;
    use crate::queue::TurnKey;
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
            files(claude.as_mut(), kernel, &Queue::default(), home)
        };
        assert_eq!(files(&Running(1)).unwrap(), "no session files yet");
        write(home, ".claude/sessions/90859.json", CLAUDE_SESSION);
        assert_eq!(
            files(&Running(1)).unwrap(),
            "no sessions running, read from ~/.claude/sessions"
        );
        assert_eq!(
            files(&Running(90859)).unwrap(),
            "1 session running (idle), read from ~/.claude/sessions"
        );
    }

    #[test]
    fn files_the_seed_cannot_make_sense_of_need_attention() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        write(home, ".claude/sessions/90859.json", CLAUDE_SESSION);
        let renamed = CLAUDE_SESSION.replace("procStart", "processStart");
        write(home, ".claude/sessions/90860.json", &renamed);
        let mut claude = source_of(Harness::Claude, home);
        let problem = files(claude.as_mut(), &Running(90859), &Queue::default(), home);
        assert_eq!(
            problem.unwrap_err().to_string(),
            "1 session running (idle), read from ~/.claude/sessions; but could not read \
             ~/.claude/sessions/90860.json: procStart None is not a start time; the \
             harness's format may have changed"
        );
    }

    #[test]
    fn records_a_running_junie_session_holds_of_unknown_kinds_need_attention() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        write(
            home,
            ".junie/processes/90859-session-261114-215557-ab12-c95cdd197f46.json",
            r#"{"pid": 90859, "sessionId": "session-261114-215557-ab12", "projectPath": "/Users/me/Code/app", "startedAt": 1794693357300}"#,
        );
        write(
            home,
            ".junie/sessions/session-261114-215557-ab12/events.jsonl",
            "{\"kind\": \"UserPromptEvent\", \"timestampMs\": 1794693360000}\n\
             {\"kind\": \"TaskPausedEvent\", \"timestampMs\": 1794693360100}\n",
        );
        let mut junie = source_of(Harness::Junie, home);
        let problem = files(junie.as_mut(), &Running(90859), &Queue::default(), home);
        assert_eq!(
            problem.unwrap_err().to_string(),
            "1 session running (working), read from ~/.junie/processes, ~/.junie/sessions; \
             but could not read ~/.junie/sessions/session-261114-215557-ab12/events.jsonl: \
             records of kinds Junie was not seen to write: TaskPausedEvent (1); the \
             harness's format may have changed"
        );
    }

    #[test]
    fn files_that_start_no_session_count_what_the_hooks_reported() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        std::fs::create_dir_all(home.join(".pi/agent/sessions")).unwrap();
        std::fs::create_dir_all(home.join(".cursor/projects")).unwrap();
        let files = |harness| {
            let mut source = source_of(harness, home);
            files(source.as_mut(), &Running(1), &Queue::default(), home).unwrap()
        };
        assert_eq!(
            files(Harness::Pi),
            "no sessions its plugin reported still running, read from ~/.pi/agent/sessions"
        );
        assert_eq!(
            files(Harness::Cursor),
            "no sessions its hooks reported still running, read from ~/.cursor/projects"
        );
        assert!(hook_bound(Harness::OpenCode));
        assert!(!hook_bound(Harness::Junie));
    }

    #[test]
    fn files_that_cannot_be_read_need_attention() {
        use std::os::unix::fs::PermissionsExt;

        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        write(home, ".grok/active_sessions.json", "not json");
        let mut grok = source_of(Harness::Grok, home);
        let problem = files(grok.as_mut(), &Running(1), &Queue::default(), home).unwrap_err();
        let problem = format!("{problem:#}");
        assert!(
            problem.starts_with("reading its files failed: parsing"),
            "{problem}"
        );

        write(home, ".claude/sessions/90859.json", CLAUDE_SESSION);
        let unreadable = |folder: &str| {
            let folder = home.join(folder);
            let mode = |mode| {
                std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(mode)).unwrap()
            };
            mode(0o000);
            let mut claude = source_of(Harness::Claude, home);
            let problem = files(claude.as_mut(), &Running(90859), &Queue::default(), home);
            mode(0o755);
            format!("{:#}", problem.unwrap_err())
        };
        // The folder itself, and the one it is in, which hides whether it is there.
        for folder in [".claude/sessions", ".claude"] {
            let problem = unreadable(folder);
            assert!(
                problem.starts_with("~/.claude/sessions cannot be read: "),
                "{folder}: {problem}"
            );
        }
    }

    #[test]
    fn a_junie_folder_the_service_does_not_read_needs_attention() {
        let home = Path::new("/Users/me");
        let default = home.join(".junie");
        assert_eq!(elsewhere(&default, &default, home), None);
        assert_eq!(
            elsewhere(&default, &home.join("work/junie"), home).unwrap(),
            "JUNIE_HOME here is ~/work/junie, which the service does not see; run \
             `launchctl setenv JUNIE_HOME /Users/me/work/junie`, then `agent-leds install` to \
             restart it"
        );
    }

    #[test]
    fn a_saved_queue_that_cannot_be_read_needs_attention() {
        let home = crate::config::home();
        let path = tilde(&daemon::queue_path(), &home);
        assert_eq!(
            queue(&Ok(Queue::default()), &home).unwrap(),
            format!("no sessions saved in {path}")
        );
        let problem = queue(&Err(anyhow::anyhow!("missing field `key`")), &home)
            .unwrap_err()
            .to_string();
        assert_eq!(
            problem,
            format!(
                "{path} cannot be read, so the daemon ignores it when it starts, and the \
                 sessions of Cursor, OpenCode and Pi show only once their hooks or plugin \
                 report them again: missing field `key`"
            )
        );
    }

    fn start(id: &str) -> Observation {
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
    }

    fn told(id: &str, signal: Signal, channel: Channel) -> Observation {
        let turn = Some(TurnKey("t1".into()));
        Observation::new(Harness::Claude, id, channel, Event::one(signal), turn)
    }

    #[test]
    fn sessions_count_by_light_and_refuse_what_is_undeclared() {
        let seeded = [
            start("a"),
            start("b"),
            start("c"),
            told("b", Signal::Prompted, Channel::File),
            told("c", Signal::Prompted, Channel::File),
        ];
        let none = Queue::default();
        assert_eq!(
            sessions(&seeded, &none, "running").unwrap(),
            "3 sessions running (2 working, 1 idle)"
        );
        // Claude Code declares no hooks.
        let hooked = [start("a"), told("a", Signal::Prompted, Channel::Hook)];
        assert_eq!(
            sessions(&hooked, &none, "running").unwrap_err().to_string(),
            "its files told Prompted through Hook, which it does not declare; the harness's \
             format may have changed"
        );
    }

    #[test]
    fn sessions_show_the_light_the_daemon_restores() {
        let seeded = [start("a"), told("a", Signal::TurnEnded, Channel::File)];
        assert_eq!(
            sessions(&seeded, &Queue::default(), "running").unwrap(),
            "1 session running (stopped)"
        );
        // Dismissed before the daemon last stopped, on the same turn.
        let mut saved = Queue::default();
        for o in &seeded {
            saved.apply(o, 1);
        }
        for o in saved.dismissed(None) {
            saved.apply(&o, 2);
        }
        assert_eq!(
            sessions(&seeded, &saved, "running").unwrap(),
            "1 session running (idle)"
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
