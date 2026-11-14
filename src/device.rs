//! The SidePulse Pro: a small mounted volume whose LEDS.LED file is the display.

use std::fs::File;
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;

/// What the name of the card's volume starts with.
pub const NAME: &str = "SidePulse";

const MAX_BYTES: usize = 512;
const MAX_LINES: usize = 20;

/// The firmware rejects anything larger and blinks red instead.
pub fn check(program: &str) -> anyhow::Result<()> {
    let lines = program.lines().count();
    if program.is_empty() || program.len() > MAX_BYTES || lines > MAX_LINES {
        anyhow::bail!(
            "program is {} bytes and {lines} lines; the device accepts 1-{MAX_BYTES} bytes and {MAX_LINES} lines",
            program.len()
        );
    }
    Ok(())
}

pub fn volume() -> Option<PathBuf> {
    std::fs::read_dir("/Volumes")
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .find(|path| {
            let named = path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with(NAME));
            named && path.join("LEDS.LED").exists()
        })
}

fn not_mounted() -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, "no SidePulse volume is mounted")
}

pub fn write(program: &str) -> io::Result<()> {
    let mut file = File::create(volume().ok_or_else(not_mounted)?.join("LEDS.LED"))?;
    file.write_all(program.as_bytes())?;
    file.sync_all()
}

/// Milliseconds since the device powered up, read past the filesystem cache.
///
/// The read doubles as a keepalive: the MacBook's SD reader powers the card
/// down after a few idle minutes. A value lower than the last one means the
/// device restarted and has forgotten its program.
pub fn uptime_ms() -> io::Result<u64> {
    #[repr(align(4096))]
    struct Sector([u8; 512]);

    let file = File::open(volume().ok_or_else(not_mounted)?.join("STATUS.TXT"))?;
    let descriptor = file.as_raw_fd();
    // SAFETY: the descriptor is open, and these commands take a number and
    // touch no memory. A read served from the cache neither keeps the card
    // awake nor shows its uptime, so that one must take.
    if unsafe { libc::fcntl(descriptor, libc::F_NOCACHE, 1) } == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: as above. Reading ahead only costs time.
    unsafe { libc::fcntl(descriptor, libc::F_RDAHEAD, 0) };
    let mut sector = Sector([0; 512]);
    let read = file.read_at(&mut sector.0, 0)?;
    uptime_in(&String::from_utf8_lossy(&sector.0[..read]))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "STATUS.TXT has no uptime_ms"))
}

/// The uptime that the card's STATUS.TXT reports.
fn uptime_in(status: &str) -> Option<u64> {
    status
        .lines()
        .find_map(|line| line.strip_prefix("uptime_ms ")?.trim().parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_program_must_fit_the_firmware() {
        assert!(check("#FF0000").is_ok());
        assert!(check("").is_err());
        assert!(check(&"x".repeat(MAX_BYTES + 1)).is_err());
        assert!(check(&"#000000\n".repeat(MAX_LINES)).is_ok());
        let error = check(&"#000000\n".repeat(MAX_LINES + 1)).unwrap_err();
        assert!(error.to_string().contains("21 lines"));
    }

    #[test]
    fn the_uptime_is_read_from_its_own_line() {
        assert_eq!(uptime_in("version 1.2\nuptime_ms 123456\n"), Some(123456));
        assert_eq!(uptime_in("uptime_ms soon"), None);
        assert_eq!(uptime_in(""), None);
    }
}
