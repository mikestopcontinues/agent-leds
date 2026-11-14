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
use std::time::UNIX_EPOCH;

use serde::Deserialize;

use crate::config::home;

const APP: &str = "com.anthropic.claudefordesktop";

/// What the app records about a session that matters here.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Session {
    /// The session id that Claude Code's hooks report.
    cli_session_id: String,
    /// When the session was last opened in the app, in Unix milliseconds.
    last_focused_at: u64,
}

/// The sessions the user has looked at since `since`, with when: those opened
/// in the app since then, and the one that is open now if the app is in front.
pub fn looked_at(since: u64, now: u64) -> HashMap<String, u64> {
    let sessions = home().join("Library/Application Support/Claude/claude-code-sessions");
    let mut looked = opened(&sessions, since);
    if app_in_front()
        && let Some(open) = open(&sessions)
    {
        looked.insert(open, now);
    }
    looked
}

/// When each session was last opened, for those opened at or after `since`.
fn opened(sessions: &Path, since: u64) -> HashMap<String, u64> {
    let mut opened = HashMap::new();
    // The app writes a session's file when it is opened, so one written
    // before `since` cannot say the session was opened after it.
    let written_since = files(sessions)
        .into_iter()
        .take_while(|(written, _)| *written >= since);
    for session in written_since.filter_map(|(_, file)| read(&file)) {
        if session.last_focused_at >= since {
            let at = opened.entry(session.cli_session_id).or_default();
            *at = session.last_focused_at.max(*at);
        }
    }
    opened
}

/// The session that is open in the app: the one opened last.
fn open(sessions: &Path) -> Option<String> {
    let mut last: Option<Session> = None;
    for (written, file) in files(sessions) {
        // No file written before the latest opening found can hold a later one.
        if last
            .as_ref()
            .is_some_and(|last| written < last.last_focused_at)
        {
            break;
        }
        if let Some(session) = read(&file)
            && last
                .as_ref()
                .is_none_or(|last| session.last_focused_at > last.last_focused_at)
        {
            last = Some(session);
        }
    }
    last.map(|session| session.cli_session_id)
}

/// The session files, with when each was written in Unix milliseconds, most
/// recently written first. They sit at `<account>/<organization>/local_<id>.json`.
fn files(sessions: &Path) -> Vec<(u64, PathBuf)> {
    let inside = |folder: &Path| -> Vec<PathBuf> {
        fs::read_dir(folder)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .collect()
    };
    let mut files: Vec<(u64, PathBuf)> = inside(sessions)
        .iter()
        .flat_map(|account| inside(account))
        .flat_map(|organization| inside(&organization))
        .filter(|file| {
            file.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("local_") && name.ends_with(".json"))
        })
        .filter_map(|file| {
            let written = fs::metadata(&file).ok()?.modified().ok()?;
            let written = written.duration_since(UNIX_EPOCH).ok()?.as_millis() as u64;
            Some((written, file))
        })
        .collect();
    files.sort_unstable_by(|a, b| b.cmp(a));
    files
}

fn read(file: &Path) -> Option<Session> {
    serde_json::from_slice(&fs::read(file).ok()?).ok()
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
            let root = std::env::temp_dir().join(format!("leds-{name}-{}", std::process::id()));
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
    fn a_session_opened_since_then_was_looked_at() {
        let sessions = Sessions::new("opened");
        sessions.session("before", 100);
        sessions.session("after", 300);
        assert_eq!(
            opened(&sessions.0, 200),
            HashMap::from([("after".to_owned(), 300)])
        );
        // A file written before then is not even read.
        let later = files(&sessions.0)[0].0 + 1;
        assert!(opened(&sessions.0, later).is_empty());
    }

    #[test]
    fn the_open_session_is_the_one_opened_last() {
        let sessions = Sessions::new("open");
        assert_eq!(open(&sessions.0), None);
        sessions.session("first", 100);
        sessions.session("last", 300);
        sessions.session("middle", 200);
        assert_eq!(open(&sessions.0).as_deref(), Some("last"));
    }

    #[test]
    fn files_that_are_not_sessions_are_ignored() {
        let sessions = Sessions::new("ignored");
        sessions.write("local_broken.json", "{");
        sessions.write("local_other.json", r#"{"title":"never opened"}"#);
        sessions.write("archived-sessions.idx", "x");
        assert!(opened(&sessions.0, 0).is_empty());
        assert_eq!(open(&sessions.0), None);
        // A folder that is not there is no sessions at all.
        assert!(opened(&sessions.0.join("missing"), 0).is_empty());
    }
}
