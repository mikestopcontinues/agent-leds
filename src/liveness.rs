//! Whether the process behind a session still runs. Each harness names its
//! process in a file of its own: a pid with when it started, or a file the
//! process keeps locked. The checks only look up what those files name; they
//! never list the process table and never take a lock.

use std::ffi::{CStr, CString};
use std::fs::File;
use std::io;
use std::mem;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// How to tell that a session's process still runs.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Liveness {
    /// A process, told apart from a later one with the same pid by when it started.
    Process { pid: i32, start: StartCheck },
    /// A file its process holds a lock on while it runs (Codex).
    Lock { path: PathBuf },
}

/// What a harness recorded of when its process started, and how closely the
/// kernel's start time must match it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StartCheck {
    /// The start in Unix microseconds, exactly: Grok's `pid_start`, and pids
    /// our hook measured.
    ExactUs(u64),
    /// The start's Unix second, from `ps` lstart text: Claude's `procStart`
    /// and Cursor's `.running` markers.
    Second(i64),
    /// A stamp in Unix milliseconds the process wrote after it started
    /// (Junie's `startedAt`). A later process with the same pid starts after it.
    NotAfterMs(u64),
}

/// What liveness and the sources ask of the operating system, so tests can
/// stand in for it.
pub trait Kernel {
    /// When a running process started, in Unix microseconds, or None if no
    /// process has the pid.
    fn started_us(&self, pid: i32) -> Option<u64>;
    /// Whether another process holds a lock on the file. Err(NotFound) if
    /// the file is gone.
    fn locked(&self, path: &Path) -> io::Result<bool>;
    /// The wall clock, in Unix milliseconds.
    fn now_ms(&self) -> u64;
}

/// The operating system itself.
pub struct System;

impl Kernel for System {
    fn started_us(&self, pid: i32) -> Option<u64> {
        bsd_info(pid).as_ref().map(start_us)
    }

    /// Asks with F_GETLK for a write lock over the whole file, which on macOS
    /// also reports a flock (as pid -1). It only asks, so it cannot race the
    /// harness taking the lock.
    fn locked(&self, path: &Path) -> io::Result<bool> {
        let file = File::open(path)?;
        // SAFETY: flock is plain data, for which all zeroes is a valid value.
        let mut lock: libc::flock = unsafe { mem::zeroed() };
        lock.l_type = libc::F_WRLCK as libc::c_short;
        lock.l_whence = libc::SEEK_SET as libc::c_short;
        // SAFETY: the descriptor is open for the call, and F_GETLK writes
        // only into the flock it is given.
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETLK, &mut lock) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(lock.l_type != libc::F_UNLCK as libc::c_short)
    }

    fn now_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_millis() as u64)
    }
}

/// Whether the process a liveness names still runs. A lock that cannot be
/// checked for any reason but the file being gone counts as held, since
/// only an unlocked or deleted file proves the process ended.
pub fn alive(k: &dyn Kernel, liveness: &Liveness) -> bool {
    match liveness {
        Liveness::Process { pid, start } => k.started_us(*pid).is_some_and(|us| match *start {
            StartCheck::ExactUs(expected) => us == expected,
            StartCheck::Second(second) => (us / 1_000_000) as i64 == second,
            StartCheck::NotAfterMs(stamp) => us <= stamp * 1_000,
        }),
        Liveness::Lock { path } => k
            .locked(path)
            .unwrap_or_else(|error| error.kind() != io::ErrorKind::NotFound),
    }
}

/// The Unix second of a `ps` lstart text such as "Sat Nov 14 21:55:57 2026",
/// read as UTC whatever the local time zone is.
pub fn lstart(text: &str) -> Option<i64> {
    utc(text.trim(), c"%a %b %e %H:%M:%S %Y", false)
}

/// The Unix second of an ISO 8601 UTC time such as
/// "2026-11-14T21:56:09.103Z", as Claude Code stamps its records, to set
/// beside a process's lstart.
pub fn iso_second(text: &str) -> Option<i64> {
    utc(text, c"%Y-%m-%dT%H:%M:%S", true)
}

/// The Unix millisecond of an ISO 8601 UTC time such as
/// "2026-11-14T21:56:09.103Z", as Grok stamps its events.
pub fn iso_ms(text: &str) -> Option<u64> {
    let second = u64::try_from(iso_second(text)?).ok()?;
    let fraction = text.split_once('.').map_or("", |(_, fraction)| fraction);
    let digits: String = fraction
        .chars()
        .take_while(char::is_ascii_digit)
        .take(3)
        .collect();
    let ms = format!("{digits:0<3}").parse::<u64>().ok()?;
    Some(second * 1000 + ms)
}

/// The Unix second `text` gives in `format`, read as UTC, with whatever
/// follows the format left unread only if `rest` allows it.
fn utc(text: &str, format: &CStr, rest: bool) -> Option<i64> {
    let text = CString::new(text).ok()?;
    // SAFETY: tm is plain data, for which all zeroes is a valid value.
    let mut tm: libc::tm = unsafe { mem::zeroed() };
    // SAFETY: both strings are NUL-terminated and outlive the call, and
    // strptime writes only into the tm it is given.
    let unread = unsafe { libc::strptime(text.as_ptr(), format.as_ptr(), &mut tm) };
    // SAFETY: a non-null result points into `text`, at what was not parsed.
    if unread.is_null() || (!rest && unsafe { *unread } != 0) {
        return None;
    }
    // SAFETY: tm was filled by strptime; timegm reads no time zone.
    Some(unsafe { libc::timegm(&mut tm) })
}

/// A process found by walking up from our own.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Proc {
    pub pid: i32,
    /// When it started, in Unix microseconds.
    pub started_us: u64,
    /// Its command name, cut to 16 bytes by the kernel.
    pub name: String,
}

/// The calling process's ancestors, nearest first, at most `depth` of them.
pub fn ancestry(depth: usize) -> Vec<Proc> {
    let mut ancestors = Vec::new();
    let Some(mut info) = bsd_info(std::process::id() as i32) else {
        return ancestors;
    };
    while ancestors.len() < depth {
        let pid = info.pbi_ppid as i32;
        if pid <= 0 {
            break;
        }
        let Some(parent) = bsd_info(pid) else {
            break;
        };
        // The name fills all 16 bytes, with no NUL, when it is that long.
        let comm = parent.pbi_comm.map(|byte| byte as u8);
        let name = match CStr::from_bytes_until_nul(&comm) {
            Ok(name) => name.to_string_lossy().into_owned(),
            Err(_) => String::from_utf8_lossy(&comm).into_owned(),
        };
        ancestors.push(Proc {
            pid,
            started_us: start_us(&parent),
            name,
        });
        info = parent;
    }
    ancestors
}

/// The kernel's record of a running process, or None if no process has the
/// pid or it has exited and only waits to be reaped.
fn bsd_info(pid: i32) -> Option<libc::proc_bsdinfo> {
    // SAFETY: proc_bsdinfo is plain data, for which all zeroes is a valid value.
    let mut info: libc::proc_bsdinfo = unsafe { mem::zeroed() };
    let size = mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: the buffer is a proc_bsdinfo of the size given, which is what
    // PROC_PIDTBSDINFO writes.
    let written =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size) };
    (written == size && info.pbi_status != libc::SZOMB).then_some(info)
}

/// When a process started, in Unix microseconds.
fn start_us(info: &libc::proc_bsdinfo) -> u64 {
    info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};

    fn process(pid: i32, start: StartCheck) -> Liveness {
        Liveness::Process { pid, start }
    }

    #[test]
    fn our_own_process_is_alive_with_its_own_start() {
        let me = std::process::id() as i32;
        let started = System.started_us(me).unwrap();
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        assert!(started <= now.as_micros() as u64);
        assert!(alive(&System, &process(me, StartCheck::ExactUs(started))));
        // The second `ps` prints in UTC is the one the kernel records.
        let ps = Command::new("ps")
            .args(["-o", "lstart=", "-p", &me.to_string()])
            .env("TZ", "UTC")
            .output()
            .unwrap();
        let second = lstart(&String::from_utf8(ps.stdout).unwrap()).unwrap();
        assert!(alive(&System, &process(me, StartCheck::Second(second))));
        assert!(alive(
            &System,
            &process(me, StartCheck::NotAfterMs(now.as_millis() as u64))
        ));
    }

    #[test]
    fn a_wrong_start_or_a_missing_pid_is_dead() {
        let me = std::process::id() as i32;
        let started = System.started_us(me).unwrap();
        let second = (started / 1_000_000) as i64;
        assert!(!alive(
            &System,
            &process(me, StartCheck::ExactUs(started + 1))
        ));
        assert!(!alive(
            &System,
            &process(me, StartCheck::Second(second - 1))
        ));
        assert_eq!(System.started_us(i32::MAX), None);
        assert!(!alive(
            &System,
            &process(i32::MAX, StartCheck::ExactUs(started))
        ));
        assert!(!alive(
            &System,
            &process(i32::MAX, StartCheck::Second(second))
        ));
    }

    #[test]
    fn a_stamp_earlier_than_the_start_is_a_later_process() {
        let me = std::process::id() as i32;
        let started_ms = System.started_us(me).unwrap() / 1_000;
        assert!(alive(
            &System,
            &process(me, StartCheck::NotAfterMs(started_ms + 1))
        ));
        assert!(!alive(
            &System,
            &process(me, StartCheck::NotAfterMs(started_ms - 1))
        ));
    }

    #[test]
    fn lstart_reads_the_probe_strings_as_utc() {
        // Claude's procStart, and the startedAt it wrote a second later.
        assert_eq!(
            lstart("Sat Nov 14 22:07:05 2026"),
            Some(1_794_694_026_354 / 1_000 - 1)
        );
        assert_eq!(lstart("Sat Nov 14 21:55:57 2026"), Some(1_794_693_357));
        // A Cursor .running marker's text after `cursor-agent|`.
        assert_eq!(lstart("Sat Nov 14 21:54:52 2026\n"), Some(1_794_693_292));
        // ps pads a one-digit day with a space.
        assert_eq!(lstart("Sun Nov  1 09:05:03 2026"), Some(1_793_523_903));
        assert_eq!(lstart("cursor-agent|Sat Nov 14 21:54:52 2026"), None);
        assert_eq!(lstart("Sat Nov 14 21:54:52 2026 extra"), None);
        assert_eq!(lstart(""), None);
    }

    #[test]
    fn a_record_s_time_reads_as_the_second_of_a_process_start() {
        assert_eq!(
            iso_second("2026-11-14T21:55:57.103Z"),
            lstart("Sat Nov 14 21:55:57 2026")
        );
        assert_eq!(iso_second("2026-11-01T09:05:03Z"), Some(1_793_523_903));
        assert_eq!(iso_second("Sat Nov 14 21:55:57 2026"), None);
        assert_eq!(iso_second(""), None);
    }

    #[test]
    fn a_record_s_time_reads_to_the_millisecond() {
        assert_eq!(
            iso_ms("2026-11-14T21:55:57.103Z"),
            Some(iso_second("2026-11-14T21:55:57Z").unwrap() as u64 * 1000 + 103)
        );
        assert_eq!(iso_ms("2026-11-01T09:05:03Z"), Some(1_793_523_903_000));
        assert_eq!(iso_ms("2026-11-01T09:05:03.5Z"), Some(1_793_523_903_500));
        assert_eq!(
            iso_ms("2026-11-01T09:05:03.123456Z"),
            Some(1_793_523_903_123)
        );
        assert_eq!(iso_ms(""), None);
    }

    #[test]
    fn lstart_ignores_the_local_time_zone() {
        // The environment is the process's, so the parse runs again in a
        // child of this test binary with the zone set.
        let child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "liveness::tests::lstart_reads_the_probe_strings_as_utc",
            ])
            .env("TZ", "America/Chicago")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&child.stdout);
        assert!(child.status.success(), "{stdout}");
        assert!(stdout.contains("1 passed"), "{stdout}");
    }

    #[test]
    fn a_lock_held_by_another_process_is_seen_until_it_is_killed() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("thread.lock");
        std::fs::write(&path, "").unwrap();
        let lock = Liveness::Lock { path: path.clone() };
        assert!(!System.locked(&path).unwrap());
        let mut holder = Command::new("perl")
            .args(["-e", r#"use Fcntl ":flock"; open(my $f, "<", $ARGV[0]) or die; flock($f, LOCK_EX) or die; $| = 1; print "locked\n"; sleep 60"#])
            .arg(&path)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(holder.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        assert_eq!(line, "locked\n");
        assert!(System.locked(&path).unwrap());
        assert!(alive(&System, &lock));
        holder.kill().unwrap();
        holder.wait().unwrap();
        assert!(!System.locked(&path).unwrap());
        assert!(!alive(&System, &lock));
        std::fs::remove_file(&path).unwrap();
        assert_eq!(
            System.locked(&path).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        assert!(!alive(&System, &lock));
    }

    #[test]
    fn ancestry_finds_the_test_runners_parent() {
        let ancestors = ancestry(2);
        assert!(!ancestors.is_empty() && ancestors.len() <= 2);
        // SAFETY: getppid has no preconditions.
        let parent = unsafe { libc::getppid() };
        assert_eq!(ancestors[0].pid, parent);
        assert_eq!(Some(ancestors[0].started_us), System.started_us(parent));
        assert!(!ancestors[0].name.is_empty());
        assert!(ancestry(0).is_empty());
    }
}
