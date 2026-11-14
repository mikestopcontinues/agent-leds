//! What Claude Code says of the sessions it is running. Each Claude Code
//! process, in a terminal or in the desktop app, keeps a file of its own in
//! `~/.claude/sessions/` with its session and whether it is busy. The files
//! are undocumented.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use serde::Deserialize;

/// What a running Claude Code process says of its session: since when it
/// has been idle, if it is.
pub type Live = HashMap<String, Option<u64>>;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Process {
    pid: i32,
    session_id: String,
    status: String,
    /// When `status` last changed, in Unix milliseconds.
    status_updated_at: u64,
}

impl Process {
    /// Since when the session has been idle, if it is. A session with only
    /// a shell command left running is idle too.
    fn idle_since(&self) -> Option<u64> {
        matches!(self.status.as_str(), "idle" | "shell").then_some(self.status_updated_at)
    }
}

/// The sessions of the Claude Code processes running now, or None if the
/// folder cannot be read. A process that ended without removing its file
/// is not counted.
pub fn sessions(folder: &Path) -> Option<Live> {
    let mut live = Live::new();
    for entry in fs::read_dir(folder).ok()?.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|extension| extension != "json") {
            continue;
        }
        let Some(process) = fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<Process>(&text).ok())
        else {
            continue;
        };
        if running(process.pid) {
            live.insert(process.session_id.clone(), process.idle_since());
        }
    }
    Some(live)
}

fn running(pid: i32) -> bool {
    // SAFETY: signal 0 sends nothing; it only asks whether the process exists.
    let signalled = unsafe { libc::kill(pid, 0) } == 0;
    signalled || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(folder: &Path, pid: i32, session: &str, status: &str, at: u64) {
        let process = serde_json::json!({
            "pid": pid, "sessionId": session, "cwd": "/Users/me/Code/app", "entrypoint": "cli",
            "status": status, "statusUpdatedAt": at, "updatedAt": at
        });
        fs::write(folder.join(format!("{pid}.json")), process.to_string()).unwrap();
    }

    #[test]
    fn running_processes_say_which_sessions_are_idle() {
        let folder = tempfile::tempdir().unwrap();
        let me = std::process::id() as i32;
        write(folder.path(), me, "busy", "busy", 5);
        // No process has this id.
        write(folder.path(), i32::MAX, "gone", "idle", 5);
        fs::write(folder.path().join("12.key"), "x").unwrap();
        fs::write(folder.path().join("13.json"), "not json").unwrap();
        let live = sessions(folder.path()).unwrap();
        assert_eq!(live, Live::from([("busy".to_owned(), None)]));
        write(folder.path(), me, "busy", "shell", 9);
        assert_eq!(sessions(folder.path()).unwrap()["busy"], Some(9));
        assert_eq!(sessions(&folder.path().join("missing")), None);
    }
}
