//! The SidePulse Pro: a small mounted volume whose LEDS.LED file is the display.

use std::fs::File;
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::path::PathBuf;

/// What the name of the card's volume starts with.
pub const NAME: &str = "SidePulse";

const MAX_BYTES: usize = 512;
const MAX_LINES: usize = 20;

/// The firmware rejects anything larger and blinks red instead.
pub fn check(program: &str) -> Result<(), String> {
    let lines = program.lines().count();
    if program.is_empty() || program.len() > MAX_BYTES || lines > MAX_LINES {
        return Err(format!(
            "program is {} bytes and {lines} lines; the device accepts 1-{MAX_BYTES} bytes and {MAX_LINES} lines",
            program.len()
        ));
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
    let mut sector = Sector([0; 512]);
    // SAFETY: the descriptor is open, and the buffer is valid for the 512 bytes requested.
    let read = unsafe {
        libc::fcntl(file.as_raw_fd(), libc::F_NOCACHE, 1);
        libc::fcntl(file.as_raw_fd(), libc::F_RDAHEAD, 0);
        libc::pread(
            file.as_raw_fd(),
            sector.0.as_mut_ptr().cast(),
            sector.0.len(),
            0,
        )
    };
    if read < 0 {
        return Err(io::Error::last_os_error());
    }
    String::from_utf8_lossy(&sector.0[..read as usize])
        .lines()
        .find_map(|line| line.strip_prefix("uptime_ms ")?.trim().parse().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "STATUS.TXT has no uptime_ms"))
}
