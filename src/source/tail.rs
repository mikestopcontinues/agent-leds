//! Reading the files harnesses append their records to, and the small files
//! they rewrite whole.
//!
//! The logs can be huge (a Junie log reaches hundreds of gigabytes, a Codex
//! rollout gigabytes), so they are never read from the start unless they
//! began after their session did: a `Tail` reads what was appended since it
//! last stopped, and `tail_back` reads only as much of the end as the state
//! needs.

use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::mem;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::Path;

/// The most a `Tail` reads in one call, so a burst of appends is read in
/// slices between animation steps.
pub const READ_BUDGET: u64 = 8 << 20;

/// The longest line a `Tail` hands over. The records that carry state are
/// small; longer lines are images and terminal output.
pub const MAX_LINE: usize = 4 << 20;

/// The first window `tail_back` reads, and the largest it grows to.
const FIRST_WINDOW: u64 = 256 << 10;
const LAST_WINDOW: u64 = 16 << 20;

/// Reads an append-only file of lines from where it last stopped.
#[derive(Debug)]
pub struct Tail {
    dev: u64,
    ino: u64,
    /// How far the file has been read.
    offset: u64,
    /// The start of a line whose end is not written yet.
    rest: Vec<u8>,
    /// Whether the bytes up to the next newline belong to a line being skipped.
    skipping: bool,
    skipped: u64,
}

/// What a `Tail` found since it last read.
#[derive(Debug, PartialEq)]
pub enum Lines {
    /// Complete lines without their newline, and whether bytes are left
    /// beyond the read budget.
    Read { lines: Vec<Vec<u8>>, more: bool },
    /// The path now holds another file, or the file shrank: the tail's
    /// position means nothing in it.
    Replaced,
}

impl Tail {
    /// Positioned at the file's current end: everything already written is
    /// history, including the start of a line still being written.
    pub fn at_end(path: &Path) -> io::Result<Tail> {
        let file = File::open(path)?;
        let metadata = file.metadata()?;
        let size = metadata.len();
        let mut last = [0];
        let partial = size > 0 && {
            file.read_exact_at(&mut last, size - 1)?;
            last[0] != b'\n'
        };
        Ok(Tail::new(&metadata, size, partial))
    }

    /// From the first byte: the file was created after its session started.
    pub fn at_start(path: &Path) -> io::Result<Tail> {
        Ok(Tail::new(&fs::metadata(path)?, 0, false))
    }

    fn new(metadata: &fs::Metadata, offset: u64, skipping: bool) -> Tail {
        Tail {
            dev: metadata.dev(),
            ino: metadata.ino(),
            offset,
            rest: Vec::new(),
            skipping,
            skipped: 0,
        }
    }

    /// Complete new lines, at most READ_BUDGET bytes. A trailing line without
    /// '\n' is held back until it is finished. A line over MAX_LINE is skipped
    /// up to its '\n' and counted in `skipped`. Replaced when the inode
    /// changed or the file shrank, and the tail is left as it was.
    pub fn read(&mut self, path: &Path) -> io::Result<Lines> {
        let mut file = File::open(path)?;
        let metadata = file.metadata()?;
        let size = metadata.len();
        if (metadata.dev(), metadata.ino()) != (self.dev, self.ino) || size < self.offset {
            return Ok(Lines::Replaced);
        }
        let want = (size - self.offset).min(READ_BUDGET);
        let mut bytes = Vec::with_capacity(want as usize);
        file.seek(SeekFrom::Start(self.offset))?;
        file.take(want).read_to_end(&mut bytes)?;
        self.offset += bytes.len() as u64;
        Ok(Lines::Read {
            lines: self.split(&bytes),
            more: self.offset < size,
        })
    }

    /// Follows a copy of the file at `path`, written on there, from where
    /// this tail stopped. False, and nothing changes, when `path` holds less
    /// than was read, so it cannot be such a copy.
    pub fn moved(&mut self, path: &Path) -> io::Result<bool> {
        let metadata = fs::metadata(path)?;
        if metadata.len() < self.offset {
            return Ok(false);
        }
        (self.dev, self.ino) = (metadata.dev(), metadata.ino());
        Ok(true)
    }

    /// How many lines over MAX_LINE this tail has skipped.
    #[allow(dead_code)]
    pub fn skipped(&self) -> u64 {
        self.skipped
    }

    /// The complete lines in `bytes`, which follow what was read before.
    fn split(&mut self, bytes: &[u8]) -> Vec<Vec<u8>> {
        let mut lines = Vec::new();
        let mut pieces = bytes.split(|&byte| byte == b'\n').peekable();
        while let Some(piece) = pieces.next() {
            if !self.skipping {
                if self.rest.len() + piece.len() > MAX_LINE {
                    self.skipping = true;
                    self.skipped += 1;
                    self.rest = Vec::new();
                } else {
                    self.rest.extend_from_slice(piece);
                }
            }
            if pieces.peek().is_some() {
                if !self.skipping {
                    lines.push(mem::take(&mut self.rest));
                }
                self.skipping = false;
            }
        }
        lines
    }
}

/// Reads backward from the end in windows of 256 KiB, doubling up to 16 MiB,
/// until `anchored` accepts the window's lines (they reach back to where the
/// state they fold into began), or the window reaches the start of the file.
/// Drops the window's first, partial line. Returns the lines in file order and a Tail at the
/// end, which holds back a last line that is not finished.
pub fn tail_back(
    path: &Path,
    mut anchored: impl FnMut(&[Vec<u8>]) -> bool,
) -> io::Result<(Vec<Vec<u8>>, Tail)> {
    let file = File::open(path)?;
    let metadata = file.metadata()?;
    let size = metadata.len();
    let mut window = FIRST_WINDOW;
    loop {
        let begin = size.saturating_sub(window);
        let mut bytes = vec![0; (size - begin) as usize];
        file.read_exact_at(&mut bytes, begin)?;
        let mut tail = Tail::new(&metadata, size, begin > 0);
        let lines = tail.split(&bytes);
        if anchored(&lines) || begin == 0 || window >= LAST_WINDOW {
            return Ok((lines, tail));
        }
        window *= 2;
    }
}

/// A whole small file, only if (ino, size, mtime_ns) agree before and after
/// the read; None when the file changed while it was read. For the files a
/// harness rewrites in place, such as Cursor's transcripts.
#[allow(dead_code)]
pub fn stable(path: &Path) -> io::Result<Option<Vec<u8>>> {
    stable_read(path, |path| fs::read(path))
}

/// `stable` with the read given, so a test can change the file during it.
#[allow(dead_code)]
fn stable_read(
    path: &Path,
    read: impl FnOnce(&Path) -> io::Result<Vec<u8>>,
) -> io::Result<Option<Vec<u8>>> {
    let stamp = |metadata: fs::Metadata| {
        (
            metadata.ino(),
            metadata.len(),
            metadata.mtime(),
            metadata.mtime_nsec(),
        )
    };
    let before = stamp(fs::metadata(path)?);
    let bytes = read(path)?;
    let after = stamp(fs::metadata(path)?);
    Ok((before == after).then_some(bytes))
}

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::path::PathBuf;

    use super::*;

    fn append(path: &Path, bytes: &[u8]) {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap()
            .write_all(bytes)
            .unwrap();
    }

    fn lines(read: Lines) -> Vec<Vec<u8>> {
        match read {
            Lines::Read { lines, more: false } => lines,
            other => panic!("expected the lines to the end, got {other:?}"),
        }
    }

    fn log() -> (tempfile::TempDir, PathBuf) {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("events.jsonl");
        append(&path, b"");
        (folder, path)
    }

    #[test]
    fn a_partial_line_is_held_back_until_it_is_finished() {
        let (_folder, path) = log();
        let mut tail = Tail::at_start(&path).unwrap();
        append(&path, b"one\ntw");
        assert_eq!(lines(tail.read(&path).unwrap()), [b"one".to_vec()]);
        assert_eq!(lines(tail.read(&path).unwrap()), Vec::<Vec<u8>>::new());
        append(&path, b"o\nthree\n");
        assert_eq!(
            lines(tail.read(&path).unwrap()),
            [b"two".to_vec(), b"three".to_vec()]
        );
    }

    #[test]
    fn a_tail_at_the_end_reads_only_lines_begun_after_it() {
        let (_folder, path) = log();
        append(&path, b"old\nhalf of a");
        let mut tail = Tail::at_end(&path).unwrap();
        append(&path, b" line\nnew\n");
        assert_eq!(lines(tail.read(&path).unwrap()), [b"new".to_vec()]);
        assert_eq!(tail.skipped(), 0);
    }

    #[test]
    fn a_huge_line_is_skipped_and_counted() {
        let (_folder, path) = log();
        let mut tail = Tail::at_start(&path).unwrap();
        append(&path, &vec![b'x'; MAX_LINE]);
        append(&path, b"\n");
        append(&path, &vec![b'y'; MAX_LINE - 1]);
        assert_eq!(lines(tail.read(&path).unwrap()), [vec![b'x'; MAX_LINE]]);
        append(&path, b"yy");
        assert_eq!(lines(tail.read(&path).unwrap()), Vec::<Vec<u8>>::new());
        append(&path, b"yyy\nsmall\n");
        assert_eq!(lines(tail.read(&path).unwrap()), [b"small".to_vec()]);
        assert_eq!(tail.skipped(), 1);
    }

    #[test]
    fn a_shrunk_or_replaced_file_is_replaced() {
        let (folder, path) = log();
        append(&path, b"one\ntwo\n");
        let mut tail = Tail::at_end(&path).unwrap();
        fs::write(&path, b"one\n").unwrap();
        assert_eq!(tail.read(&path).unwrap(), Lines::Replaced);

        let mut tail = Tail::at_end(&path).unwrap();
        let staged = folder.path().join("staged");
        fs::write(&staged, b"one\ntwo\n").unwrap();
        fs::rename(&staged, &path).unwrap();
        assert_eq!(tail.read(&path).unwrap(), Lines::Replaced);
    }

    #[test]
    fn a_moved_tail_reads_the_copy_on_from_where_it_stopped() {
        let (folder, path) = log();
        append(&path, b"one\ntw");
        let mut tail = Tail::at_start(&path).unwrap();
        assert_eq!(lines(tail.read(&path).unwrap()), [b"one".to_vec()]);
        let copy = folder.path().join("moved.jsonl");
        fs::copy(&path, &copy).unwrap();
        append(&copy, b"o\nthree\n");
        assert!(tail.moved(&copy).unwrap());
        assert_eq!(
            lines(tail.read(&copy).unwrap()),
            [b"two".to_vec(), b"three".to_vec()]
        );
        // The file it left holds less than has been read.
        assert!(!tail.moved(&path).unwrap());
        append(&copy, b"four\n");
        assert_eq!(lines(tail.read(&copy).unwrap()), [b"four".to_vec()]);
    }

    #[test]
    fn the_budget_leaves_more_to_read() {
        let (_folder, path) = log();
        let line = [b'z'; 1023];
        let count = (READ_BUDGET / 1024 + 10) as usize;
        let mut text = Vec::with_capacity(count * 1024);
        for _ in 0..count {
            text.extend_from_slice(&line);
            text.push(b'\n');
        }
        append(&path, &text);
        let mut tail = Tail::at_start(&path).unwrap();
        let Lines::Read { lines: first, more } = tail.read(&path).unwrap() else {
            panic!("the file is the same");
        };
        assert!(more);
        assert_eq!(first.len(), (READ_BUDGET / 1024) as usize);
        assert_eq!(lines(tail.read(&path).unwrap()).len(), 10);
    }

    #[test]
    fn tail_back_drops_its_first_partial_line_and_doubles_until_anchored() {
        let (_folder, path) = log();
        let record = |n: usize| format!("{n:0999}");
        let mut text = String::new();
        for n in 0..1500 {
            text.push_str(&record(n));
            text.push('\n');
        }
        text.push_str("unfinished");
        append(&path, text.as_bytes());

        // The anchor, record 1000, is 500 KB from the end: past the first
        // window and inside the second.
        let mut windows = Vec::new();
        let (found, mut tail) = tail_back(&path, |lines| {
            windows.push(lines.len());
            lines.iter().any(|line| *line == record(1000).as_bytes())
        })
        .unwrap();
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[1], found.len());
        let expected: Vec<Vec<u8>> = (1500 - found.len()..1500)
            .map(|n| record(n).into_bytes())
            .collect();
        assert_eq!(found, expected);
        assert!(windows[0] < windows[1]);

        append(&path, b" now\n");
        assert_eq!(
            lines(tail.read(&path).unwrap()),
            [b"unfinished now".to_vec()]
        );
    }

    #[test]
    fn tail_back_stops_at_the_start_of_a_short_file() {
        let (_folder, path) = log();
        append(&path, b"first\nsecond\n");
        let mut asked = 0;
        let (found, _) = tail_back(&path, |_| {
            asked += 1;
            false
        })
        .unwrap();
        assert_eq!(asked, 1);
        assert_eq!(found, [b"first".to_vec(), b"second".to_vec()]);
    }

    #[test]
    fn stable_refuses_a_read_whose_stat_changed() {
        let (_folder, path) = log();
        append(&path, b"one\n");
        assert_eq!(stable(&path).unwrap(), Some(b"one\n".to_vec()));
        let torn = stable_read(&path, |path| {
            let bytes = fs::read(path);
            append(path, b"two\n");
            bytes
        })
        .unwrap();
        assert_eq!(torn, None);
    }
}
