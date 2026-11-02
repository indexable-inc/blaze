//! The records of a build as a process writes them while it builds, for another to follow: each
//! a frame appended to a file beside the process's socket, named for the process so that one
//! started in another's place writes a file of its own. A record's time there is since the UNIX
//! epoch; whoever hands the records on counts it from when the build was asked for.

use std::{
    fs::File,
    io::{Read as _, Seek as _, SeekFrom},
    path::{Path, PathBuf},
    time::SystemTime,
};

use crate::events::Record;

/// The file the process `pid` that listens at `socket` says what it does in.
pub fn beside(socket: &Path, pid: u32) -> PathBuf {
    // alloc: once per process, at start.
    socket.with_file_name(format!("events.{pid}"))
}

/// Takes away what the processes that listened at `socket` before said.
pub fn forget_beside(socket: &Path) {
    let Some(dir) = socket.parent() else { return };
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with("events.") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// The file the process that listens in `dir` says what it does in, and how long it is: the
/// one written last, of a process that started in the place of another that has not quite
/// gone.
pub fn written_in(dir: &Path) -> Option<(PathBuf, u64)> {
    let entries = std::fs::read_dir(dir).ok()?;
    let written =
        entries.flatten().filter(|it| it.file_name().to_string_lossy().starts_with("events."));
    let written = written.filter_map(|it| {
        let metadata = it.metadata().ok()?;
        Some((metadata.modified().ok()?, it.path(), metadata.len()))
    });
    written.max().map(|(_, path, len)| (path, len))
}

/// Nanoseconds since the UNIX epoch.
pub fn now_ns() -> u64 {
    let since = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default();
    u64::try_from(since.as_nanos()).unwrap_or(u64::MAX)
}

/// The records a process writes to a file, read as it writes them.
#[derive(Default)]
pub struct Follow {
    path: Option<PathBuf>,
    /// `path`, open at `at`, from one read to the next: opened once, not once a read.
    file: Option<File>,
    at: u64,
    /// Bytes read of a frame not written in full yet, kept from one read to the next.
    partial: Vec<u8>,
}

impl Follow {
    /// Follows what the process that listens in `dir` writes from now on: what it said before
    /// was for the commands before.
    pub fn from_now(dir: &Path) -> Follow {
        let (path, at) = written_in(dir).map_or((None, 0), |(path, len)| (Some(path), len));
        Follow { path, at, ..Follow::default() }
    }

    /// The records written to `path`, which is `len` long, since the last read, and why each
    /// frame that is not a record is not. A file other than the last is read from its start,
    /// as a process started in another's place.
    pub fn read(&mut self, path: &Path, len: u64) -> Vec<Result<Record, crate::cbor::Error>> {
        // alloc: the records are handed out; `read_into` fills a list the caller keeps.
        let mut records = Vec::new();
        self.read_into(path, len, &mut records);
        records
    }

    /// [`Follow::read`] onto the end of `records`, a list the caller keeps.
    pub fn read_into(
        &mut self,
        path: &Path,
        len: u64,
        records: &mut Vec<Result<Record, crate::cbor::Error>>,
    ) {
        if self.path.as_deref() != Some(path) {
            // alloc: once per file followed, a process started in another's place.
            self.path = Some(path.to_owned());
            self.file = None;
            self.at = 0;
            self.partial.clear();
        }
        if len <= self.at {
            return;
        }
        let kept = self.partial.len();
        // A file kept open that reads nothing when `path` says there is more is one deleted
        // and made again at the path: it is opened again, once.
        for fresh in [self.file.is_none(), true] {
            if fresh {
                self.file = None;
                let Ok(mut file) = File::open(path) else { return };
                if file.seek(SeekFrom::Start(self.at)).is_err() {
                    return;
                }
                self.file = Some(file);
            }
            let Some(file) = &mut self.file else { return };
            match file.read_to_end(&mut self.partial) {
                Ok(0) if !fresh => continue,
                Ok(read) => {
                    self.at += read as u64;
                    break;
                }
                Err(_) => {
                    // Opened again at `at` next time, nothing of this read taken.
                    self.file = None;
                    self.partial.truncate(kept);
                    return;
                }
            }
        }
        let mut rest = &self.partial[..];
        while rest.len() >= 4 {
            let len = u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]) as usize;
            let Some(item) = rest.get(4..4 + len) else { break };
            records.push(crate::cbor::from_slice::<Record>(item));
            rest = &rest[4 + len..];
        }
        let used = self.partial.len() - rest.len();
        self.partial.drain(..used);
    }
}

#[cfg(test)]
mod tests {
    use crate::events::{Event, Level, Step};

    #[test]
    fn records_are_followed_as_they_are_written() {
        let dir = std::env::temp_dir().join(format!("blaze-events-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("events.1");
        let frame = |event: Event| {
            let record = crate::events::Record { at_ns: 5, event };
            let item = crate::cbor::to_vec(&record).unwrap();
            let mut frame = Vec::new();
            crate::write_frame(&mut frame, &item).unwrap();
            frame
        };
        let before = frame(Event::Said { level: Level::Note, text: "before".to_owned() });
        std::fs::write(&path, &before).unwrap();
        let len = |path: &std::path::Path| std::fs::metadata(path).unwrap().len();
        let mut follow = super::Follow::from_now(&dir);
        assert!(follow.read(&path, len(&path)).is_empty(), "what was said before is not read");
        let started = frame(Event::Started { step: Step::Codegen, of: None });
        let ended = frame(Event::Ended { step: Step::Codegen, of: None });
        let mut written = before.clone();
        written.extend(&started);
        written.extend(&ended[..3]);
        std::fs::write(&path, &written).unwrap();
        let read: Vec<_> = follow.read(&path, len(&path)).into_iter().map(Result::unwrap).collect();
        assert_eq!(read.len(), 1, "a frame half written waits for the rest");
        written.extend(&ended[3..]);
        std::fs::write(&path, &written).unwrap();
        let read: Vec<_> = follow.read(&path, len(&path)).into_iter().map(Result::unwrap).collect();
        assert!(matches!(read[..], [crate::events::Record { event: Event::Ended { .. }, .. }]));
        // A process in the place of the last writes another file, read from its start.
        let other = dir.join("events.2");
        std::fs::write(&other, &before).unwrap();
        assert_eq!(follow.read(&other, len(&other)).len(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The records written since the last read, read into a kept list from the file kept open,
    /// allocate nothing; a file made again at the path is read from where the last one was.
    #[test]
    fn a_warm_read_allocates_nothing() {
        use std::io::Write as _;
        let dir = std::env::temp_dir().join(format!("blaze-events-warm-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("events.1");
        let record = crate::events::Record { at_ns: 5, event: Event::Ended { step: Step::Codegen, of: None } };
        let mut ended = Vec::new();
        crate::write_frame(&mut ended, &crate::cbor::to_vec(&record).unwrap()).unwrap();
        std::fs::write(&path, &ended).unwrap();
        let len = |path: &std::path::Path| std::fs::metadata(path).unwrap().len();
        let mut follow = super::Follow::default();
        let mut records = Vec::with_capacity(16);
        follow.read_into(&path, len(&path), &mut records);
        assert_eq!(records.len(), 1, "a new file is read from its start");
        follow.partial.reserve(4096);
        let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        for _ in 0..8 {
            file.write_all(&ended).unwrap();
            let len = len(&path);
            records.clear();
            blaze_alloc_count::assert_at_most(0, "a warm read of the records written since", || {
                follow.read_into(&path, len, &mut records)
            });
            assert!(matches!(records[..], [Ok(ref it)] if *it == record));
        }
        drop(file);
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, ended.repeat(12)).unwrap();
        records.clear();
        follow.read_into(&path, len(&path), &mut records);
        assert_eq!(records.len(), 3, "read on from the ninth frame, as a file opened again is");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
