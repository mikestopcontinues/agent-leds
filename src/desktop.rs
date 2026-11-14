//! The Claude desktop app: which sessions the user has looked at.
//!
//! Claude Code has no hook for a session being read. The desktop app does
//! keep a file per session that records when the session was last opened.
//! The files are the app's own and undocumented, so everything here treats a
//! file it cannot read as a session nobody has looked at.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use serde::Deserialize;

use crate::config::home;

const APP: &str = "com.anthropic.claudefordesktop";
/// The user is at the Mac if they touched it this recently.
const ACTIVE: Duration = Duration::from_secs(5);

/// What the app records about a session that matters here.
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Session {
    /// The session id that Claude Code's hooks report, once the session has one.
    cli_session_id: Option<String>,
    /// When the session was last opened in the app, in Unix milliseconds.
    last_focused_at: Option<u64>,
}

/// The app's session files as last read. A file is some hundreds of
/// kilobytes, so each is parsed once per time the app writes it.
#[derive(Default)]
pub struct Desktop {
    read: HashMap<PathBuf, (SystemTime, Session)>,
}

impl Desktop {
    /// The sessions the user has looked at since `since`, with when: those
    /// opened in the app since then, and the one that is open now if the user
    /// is using the app. Being in front is not enough for that: the user may
    /// be away from the screen.
    pub fn looked_at(&mut self, since: u64, now: u64) -> HashMap<String, u64> {
        let sessions = home().join("Library/Application Support/Claude/claude-code-sessions");
        let mut looked = self.opened(&sessions, since);
        if idle() < ACTIVE
            && app_in_front()
            && let Some(open) = self.open(&sessions)
        {
            looked.insert(open, now);
        }
        looked
    }

    /// When each session was last opened, for those opened at or after `since`.
    fn opened(&mut self, sessions: &Path, since: u64) -> HashMap<String, u64> {
        let mut opened = HashMap::new();
        for (written, file) in files(sessions) {
            // The app writes a session's file when it is opened, so one
            // written before `since` cannot say the session was opened after it.
            if ms(written) < since {
                break;
            }
            if let Some(session) = self.session(written, &file)
                && let (Some(id), Some(at)) = (session.cli_session_id, session.last_focused_at)
                && at >= since
            {
                let latest = opened.entry(id).or_default();
                *latest = at.max(*latest);
            }
        }
        opened
    }

    /// The session that is open in the app: the one opened last. None if that
    /// is not a Claude Code session, or cannot be told.
    fn open(&mut self, sessions: &Path) -> Option<String> {
        let mut last: Option<(u64, Option<String>)> = None;
        for (written, file) in files(sessions) {
            // No file written before the latest opening found can hold a later one.
            if last.as_ref().is_some_and(|(at, _)| ms(written) < *at) {
                break;
            }
            // A file that cannot be read may be the open session's, caught
            // while the app writes it. Naming another session as the open
            // one would mark it read unseen.
            let session = self.session(written, &file)?;
            if let Some(at) = session.last_focused_at
                && last.as_ref().is_none_or(|(latest, _)| at > *latest)
            {
                last = Some((at, session.cli_session_id));
            }
        }
        last?.1
    }

    /// What a session file says, read again only if it was written since.
    fn session(&mut self, written: SystemTime, file: &Path) -> Option<Session> {
        if let Some((read, session)) = self.read.get(file)
            && *read == written
        {
            return Some(session.clone());
        }
        let session: Session = serde_json::from_slice(&fs::read(file).ok()?).ok()?;
        self.read
            .insert(file.to_owned(), (written, session.clone()));
        Some(session)
    }
}

fn ms(time: SystemTime) -> u64 {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

/// The session files with when each was written, most recently written
/// first. They sit at `<account>/<organization>/local_<id>.json`.
fn files(sessions: &Path) -> Vec<(SystemTime, PathBuf)> {
    let inside = |folder: &Path| -> Vec<PathBuf> {
        fs::read_dir(folder)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .collect()
    };
    let mut files: Vec<(SystemTime, PathBuf)> = inside(sessions)
        .iter()
        .flat_map(|account| inside(account))
        .flat_map(|organization| inside(&organization))
        .filter(|file| {
            file.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("local_") && name.ends_with(".json"))
        })
        .filter_map(|file| Some((fs::metadata(&file).ok()?.modified().ok()?, file)))
        .collect();
    files.sort_unstable_by(|a, b| b.cmp(a));
    files
}

/// How long ago the user last touched the keyboard, mouse or trackpad.
fn idle() -> Duration {
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGEventSourceSecondsSinceLastEventType(state: i32, event: u32) -> f64;
    }
    // The login session's combined event state.
    const COMBINED_SESSION_STATE: i32 = 0;
    const ANY_INPUT: u32 = u32::MAX;
    // SAFETY: the function takes two plain values and returns one.
    let seconds =
        unsafe { CGEventSourceSecondsSinceLastEventType(COMBINED_SESSION_STATE, ANY_INPUT) };
    Duration::try_from_secs_f64(seconds).unwrap_or(Duration::MAX)
}

/// Whether the desktop app is the frontmost app. A daemon has no window, so
/// it asks the system's own tool.
fn app_in_front() -> bool {
    let lsappinfo = |args: &[&str]| {
        let output = Command::new("/usr/bin/lsappinfo")
            .args(args)
            .output()
            .ok()?;
        Some(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    };
    lsappinfo(&["front"])
        .and_then(|app| lsappinfo(&["info", "-only", "bundleid", &app]))
        .is_some_and(|info| info.contains(APP))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A folder of session files as the app lays them out, removed when dropped.
    struct Sessions(PathBuf);

    impl Sessions {
        fn new(name: &str) -> Sessions {
            let root =
                std::env::temp_dir().join(format!("agent-leds-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(root.join("account/organization")).unwrap();
            Sessions(root)
        }

        fn write(&self, name: &str, text: &str) {
            fs::write(self.0.join("account/organization").join(name), text).unwrap();
        }

        fn session(&self, id: &str, focused: u64) {
            self.write(
                &format!("local_{id}.json"),
                &format!(r#"{{"cliSessionId":"{id}","lastFocusedAt":{focused},"title":"x"}}"#),
            );
        }
    }

    impl Drop for Sessions {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn the_system_says_how_long_the_user_has_been_idle() {
        assert!(idle() < Duration::MAX);
    }

    #[test]
    fn a_session_opened_since_then_was_looked_at() {
        let sessions = Sessions::new("opened");
        sessions.session("before", 100);
        sessions.session("after", 300);
        let mut desktop = Desktop::default();
        assert_eq!(
            desktop.opened(&sessions.0, 200),
            HashMap::from([("after".to_owned(), 300)])
        );
        // A file written before then is not even read.
        let later = ms(files(&sessions.0)[0].0) + 1;
        assert!(desktop.opened(&sessions.0, later).is_empty());
    }

    #[test]
    fn the_open_session_is_the_one_opened_last() {
        let sessions = Sessions::new("open");
        let mut desktop = Desktop::default();
        assert_eq!(desktop.open(&sessions.0), None);
        sessions.session("first", 100);
        sessions.session("last", 300);
        sessions.session("middle", 200);
        assert_eq!(desktop.open(&sessions.0).as_deref(), Some("last"));
        // The app writes a file again when its session is opened.
        sessions.session("first", 400);
        assert_eq!(desktop.open(&sessions.0).as_deref(), Some("first"));
    }

    #[test]
    fn files_that_say_nothing_are_ignored() {
        let sessions = Sessions::new("ignored");
        sessions.session("opened", 100);
        sessions.write("local_other.json", r#"{"title":"never opened"}"#);
        sessions.write("archived-sessions.idx", "x");
        let mut desktop = Desktop::default();
        assert_eq!(desktop.opened(&sessions.0, 0).len(), 1);
        assert_eq!(desktop.open(&sessions.0).as_deref(), Some("opened"));
        // A folder that is not there is no sessions at all.
        assert!(desktop.opened(&sessions.0.join("missing"), 0).is_empty());
    }

    #[test]
    fn no_session_is_named_open_when_that_cannot_be_told() {
        let sessions = Sessions::new("unknown");
        sessions.session("opened", 100);
        let mut desktop = Desktop::default();
        // The session opened last is not a Claude Code session.
        sessions.write("local_chat.json", r#"{"lastFocusedAt":200}"#);
        assert_eq!(desktop.open(&sessions.0), None);
        // A file caught half written might be the open session's.
        sessions.write("local_chat.json", r#"{"lastFocusedAt":2"#);
        assert_eq!(desktop.open(&sessions.0), None);
        assert_eq!(desktop.opened(&sessions.0, 0).len(), 1);
    }
}
