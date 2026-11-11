//! `blaze why`'s ledger file ([`super::LEDGER`]), driven on the `blaze-history` thread: the
//! restore, each build's explaining and append, and the compaction. None of it runs on the
//! daemon's start path or its loop (rule 16: each has its span).
//!
//! - **Restore** ([`File::restore`]): a file never written whole (empty, or part of the magic)
//!   is cut to nothing; a last record cut short (or zeros after the last whole one), as a crash
//!   during an append leaves it, is copied aside (`why.ledger.tail.<n>`) and marks a gap; the
//!   ledger is then written whole over the file with the gap in it, never cut in place first, and
//!   only when that write fails is the file cut back to the records before the tail, once the gap
//!   is in [`GAP`] (a tail with nothing before it is cut in place: it holds no gap); a file that can't be read, is
//!   another version's or has a record that does not decode is moved aside
//!   (`why.ledger.<why>.<n>`, the newest [`ASIDE`] of each kind kept) and never written over,
//!   a gap marked after the records read before the one that does not decode (rule 26). Stale
//!   hidden files of a whole write a crash cut short are removed.
//! - **Append** ([`File::write`]): a build's record, synced; the directory synced too when the
//!   append made the file. One that fails is cut back to the length before it, and the build is
//!   marked a gap in memory and in [`GAP`], a file beside the ledger that survives a restart;
//!   from then on each build writes the whole ledger, until one succeeds and the gap is gone.
//! - **Compaction**: the whole ledger, its oldest subjects shrunk to tombstones past [`super::CAP_BYTES`],
//!   written to a hidden file, synced, renamed over the ledger and the directory synced, once
//!   the file is past [`super::COMPACT_MIN`] and twice its last whole write (or holds appends a
//!   restart found, whose whole size it can't know).

use std::{
    fs,
    path::{Path, PathBuf},
};

use blaze_api::Rebuild;
use blaze_profile::facts::MissRow;

use super::{COMPACT_MIN, Fault, LEDGER, Ledger, compact_due};

/// The file beside the ledger that says a build's record did not reach it: the build's number.
pub const GAP: &str = "why.ledger.gap";

/// How many ledgers of each kind moved aside are kept, the newest.
const ASIDE: u64 = 4;

/// The ledger and its file.
pub struct File {
    dir: PathBuf,
    path: PathBuf,
    ledger: Ledger,
    /// How long the file is now, and was when last written whole.
    len: u64,
    live: u64,
    /// Whether builds' records go to the file: not when it could be neither read nor moved
    /// aside.
    writable: bool,
    /// The first build whose record did not reach the file, while it has not been written
    /// whole since.
    behind: Option<u64>,
    /// Whether the file holds records appended since its last whole write that a restore found:
    /// how big that write was is not known, so it is compacted once past [`COMPACT_MIN`].
    compact: bool,
    /// The length to cut a torn tail back to, found by the restore and not yet cut: the whole
    /// write replaces the file, and only if it fails is the file cut, once its gap is in [`GAP`].
    torn_at: Option<u64>,
}

/// A whole write that failed: why, and whether the file was already replaced (renamed over,
/// a later step failing), so it must not be cut back as the old file.
struct Unwritten {
    why: String,
    replaced: bool,
}

/// What a restore said: for every `blaze why` answer, when what it is compared with is short
/// (`torn`), and for `blaze daemon status`, once (`said`).
pub struct Said {
    pub torn: Option<String>,
    pub said: Option<String>,
}

impl File {
    /// The ledger in the history `dir`, replayed, with what went wrong. Runs on the history's
    /// thread after the daemon has started: builds that end meanwhile wait in its channel.
    pub fn restore(dir: &Path) -> (File, Said) {
        let _span = tracing::info_span!(target: "blaze", "restore blaze why's ledger").entered();
        let path = dir.join(LEDGER);
        let mut file = File {
            dir: dir.to_path_buf(),
            path,
            ledger: Ledger::default(),
            len: 0,
            live: 0,
            writable: true,
            behind: None,
            compact: false,
            torn_at: None,
        };
        let mut said = Said { torn: None, said: None };
        // alloc: once a daemon start, the notes a restore makes
        let mut notes: Vec<String> = Vec::new();
        file.remove_stale(&mut notes);
        let gap = file.read_gap(&mut notes);
        match fs::read(&file.path) {
            Ok(bytes) => file.replay(&bytes, &mut said, &mut notes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                let read = format!("reading blaze why's ledger {}: {error}", file.path.display()); // alloc: error path
                let to = file.aside("unreadable", &read);
                said.torn = Some(to);
            }
        }
        if let Some(gap) = gap {
            file.ledger.behind(gap.unwrap_or_else(|| file.ledger.after_newest()));
            // Written whole with the gap in it, so the gap file can go.
            file.behind = Some(gap.unwrap_or(0));
        }
        // Written whole only for a gap new in this restore: a due compaction waits for the first
        // append ([`File::write`]), so a restart never rewrites the ledger before it answers.
        let torn_at = file.torn_at.take();
        if file.writable && file.behind.is_some() {
            if let Err(Unwritten { why, replaced }) = file.whole() {
                // The gap is new in this restore (`behind` is set; a gap the ledger held before is
                // in its file already): it goes to the gap file, so a restart before a whole write
                // keeps it.
                let (why, marked) = match file.ledger.gap {
                    Some(gap) => file.mark(gap, why),
                    None => (why, true),
                };
                notes.push(why);
                // The torn tail is cut back only now, its gap on disk; until then each build writes
                // the ledger whole, so the bytes left after its records are never appended after.
                match (torn_at, replaced, marked) {
                    (None, ..) | (Some(_), true, _) => {}
                    (Some(keep), false, true) => {
                        if let Err(error) = file.cut(keep) {
                            notes.push(format!("blaze why's ledger {}: its torn tail {error}", file.path.display())); // alloc: error path
                        }
                    }
                    // alloc: error path
                    (Some(keep), false, false) => notes.push(format!(
                        "blaze why's ledger {}: not cut back to its {keep} bytes, its gap being on disk nowhere, so the next restore finds its torn tail again",
                        file.path.display()
                    )),
                }
            }
        }
        if let Some(torn) = &said.torn {
            notes.push(torn.clone()); // clone: error path
        }
        if !notes.is_empty() {
            said.said = Some(notes.join("; "));
        }
        (file, said)
    }

    /// `build`'s misses explained ([`Ledger::explain`]): its answer's items and the history's
    /// rows, and the record to write after the answer is sent ([`File::write`]).
    pub fn explain(
        &mut self,
        build: u64,
        ended_well: bool,
        misses: Vec<blaze_kernel::Miss>,
    ) -> (Vec<Rebuild>, Vec<MissRow>, Vec<u8>) {
        let _span = tracing::info_span!(target: "blaze", "explain a build's misses", build, misses = misses.len()).entered();
        let explained = self.ledger.explain(build, ended_well, misses);
        // A step with no recorded reason is a bug in blaze: counted where the profile sees it.
        let unexplained = explained
            .rebuilt
            .iter()
            .filter(|it| {
                it.ends.iter().any(|end| matches!(end, blaze_api::Reason::Unexplained { .. }))
            })
            .count();
        if unexplained > 0 {
            tracing::warn!(target: "blaze", build, unexplained, "blaze why has no recorded reason for steps this build recomputed, which is a bug in blaze: `blaze why --unexplained` lists them");
        }
        (explained.rebuilt, explained.rows, explained.record)
    }

    /// The newest build a subject dropped from the ledger was last computed in.
    pub fn dropped(&self) -> Option<u64> {
        self.ledger.dropped()
    }

    /// `build`'s `record` to the file: appended, or the whole ledger written while the file is
    /// behind or due a compaction. What failed, for `blaze daemon status`.
    pub fn write(&mut self, build: u64, record: &[u8]) -> Result<(), String> {
        if !self.writable || (record.is_empty() && self.behind.is_none()) {
            return Ok(());
        }
        if self.behind.is_some() || self.due(self.len + record.len() as u64) {
            return self.whole().map_err(|error| self.fell_behind(build, error.why));
        }
        let _span = tracing::info_span!(target: "blaze", "append to blaze why's ledger", build, bytes = record.len()).entered();
        match self.append(record) {
            Ok(len) => {
                self.len = len;
                Ok(())
            }
            Err(error) => Err(self.fell_behind(build, error)),
        }
    }

    /// The daemon stops: a ledger still behind its file is written whole one last time.
    pub fn close(&mut self) -> Result<(), String> {
        match self.writable && self.behind.is_some() {
            true => self.whole().map_err(|error| error.why),
            false => Ok(()),
        }
    }

    /// Whether the file, `len` bytes long, is due to be written whole: past twice its last
    /// whole write, or past [`COMPACT_MIN`] holding appends a restore found.
    fn due(&self, len: u64) -> bool {
        compact_due(len, self.live) || (self.compact && len > COMPACT_MIN)
    }

    /// The file's `bytes` replayed into the ledger, and the file cut back or moved aside as its
    /// fault asks.
    fn replay(&mut self, bytes: &[u8], said: &mut Said, notes: &mut Vec<String>) {
        let restored = Ledger::restore(bytes);
        let at = self.path.display().to_string(); // alloc: once a daemon start
        self.ledger = restored.ledger;
        (self.len, self.live) = (bytes.len() as u64, bytes.len() as u64);
        self.compact = restored.records > 1;
        match restored.fault {
            None => {}
            Some(Fault::Unbegun { len }) => match self.cut(0) {
                Ok(()) => notes.push(format!("blaze why's ledger {at} was made and never written ({len} bytes of its magic): begun again")), // alloc: error path
                Err(error) => notes.push(format!("blaze why's ledger {at}: {error}")), // alloc: error path
            },
            Some(Fault::Tail { keep, why }) => {
                if !self.ledger.is_empty() {
                    // The cut record was a build's: its subjects' last computes
                    // are lost, so every record before it is compared with nothing. The ledger
                    // is written whole with that gap at the restore, so a second restart keeps it.
                    let after = self.ledger.after_newest();
                    self.ledger.behind(after);
                    self.behind = Some(0);
                }
                // What is cut is copied aside first: a corrupt length in a middle record reads
                // as a cut tail too, and no byte of the file is destroyed unread (rule 26).
                let cut = &bytes[keep..];
                let held = !self.ledger.is_empty();
                let copied = match copy_aside(&self.dir, "tail", cut) {
                    Ok(to) => to,
                    Err(error) => {
                        self.writable = false;
                        // alloc: error path
                        notes.push(format!(
                            "blaze why's ledger {at}: its last record was cut short ({why}); copying its last {} bytes aside: {error}, so it is not cut back and no build's record is written to it",
                            cut.len()
                        ));
                        return;
                    }
                };
                if held {
                    // Not cut in place: the restore writes the ledger whole over the file, and a
                    // crash before that must still find the tail, and so the gap (rule 26).
                    self.torn_at = Some(keep as u64);
                    // alloc: error path
                    return notes.push(format!(
                        "blaze why's ledger {at}: its last record was cut short ({why}), the {} bytes after its first {keep} copied to {}, and it is written whole without them",
                        cut.len(),
                        copied.display()
                    ));
                }
                match self.cut(keep as u64) {
                    // alloc: error path
                    Ok(()) => notes.push(format!(
                        "blaze why's ledger {at}: its last record was cut short ({why}), so it was cut back to the {keep} bytes before it, the {} cut copied to {}",
                        cut.len(),
                        copied.display()
                    )),
                    Err(error) => notes.push(format!("blaze why's ledger {at}: {why}; {error}")), // alloc: error path
                }
            }
            Some(Fault::Version { why }) => {
                self.ledger = Ledger::default();
                let short = format!("blaze why's ledger {at} was not read: {why}"); // alloc: error path
                said.torn = Some(self.aside("version", &short));
            }
            Some(Fault::Corrupt { why }) => {
                if !self.ledger.is_empty() {
                    // The record that does not decode and every one after it are only in the
                    // file moved aside: a subject computed in them is compared with nothing,
                    // never with its last record read.
                    let after = self.ledger.after_newest();
                    self.ledger.behind(after);
                }
                let short = format!("blaze why's ledger {at} restored short: {why}"); // alloc: error path
                said.torn = Some(self.aside("corrupt", &short));
                // A new file begun from what was read of it, with the gap.
                self.behind = Some(0);
            }
        }
    }

    /// The ledger moved aside as `kind`, for `short`: what to say of it. A file that can't be
    /// moved aside is not written to again.
    fn aside(&mut self, kind: &str, short: &str) -> String {
        (self.len, self.live) = (0, 0);
        match aside(&self.dir, &self.path, kind) {
            Ok(to) => format!("{short}; moved to {}, and a new one begun", to.display()), // alloc: error path
            Err(why) => {
                self.writable = false;
                format!("{short}; {why}, so no build's record is written to it") // alloc: error path
            }
        }
    }

    /// The file cut to its first `len` bytes in place, and synced.
    fn cut(&mut self, len: u64) -> Result<(), String> {
        let cut = fs::OpenOptions::new()
            .write(true)
            .open(&self.path)
            // durable: the why ledger cut back to its last whole record: the cut is on disk before the next append
            .and_then(|it| it.set_len(len).and_then(|()| it.sync_all()));
        match cut {
            Ok(()) => {
                (self.len, self.live) = (len, len);
                Ok(())
            }
            Err(error) => {
                self.writable = false;
                // alloc: error path
                Err(format!(
                    "cutting it back to {len} bytes: {error}, so no build's record is written to it"
                ))
            }
        }
    }

    /// `build`'s record did not reach the file (`error`): the ledger and [`GAP`] say so, and
    /// the next build writes it whole. What to tell `blaze daemon status`.
    fn fell_behind(&mut self, build: u64, error: String) -> String {
        self.mark(build, error).0
    }

    /// [`File::fell_behind`], and whether [`GAP`] on disk now names `build` or a later gap.
    /// One there already that names an earlier build is replaced whole (a hidden file renamed
    /// over it), so the later gap is the one a restart reads.
    fn mark(&mut self, build: u64, error: String) -> (String, bool) {
        if self.behind.is_none() {
            self.behind = Some(build);
        }
        let gap = self.dir.join(GAP);
        let marked = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&gap)
            .and_then(|mut it| {
                use std::io::Write as _;
                // durable: the gap file says a build's record is missing from the ledger: it must survive a restart (rule 26)
                write!(it, "{build}").and_then(|()| it.sync_all())
            })
            // durable: the gap file's name survives a restart
            .and_then(|()| fs::File::open(&self.dir).and_then(|it| it.sync_all()));
        match marked {
            Ok(()) => (
                format!(
                    "{error}; build {build} is marked a gap in {}, and the next build writes the ledger whole",
                    gap.display()
                ),
                true,
            ), // alloc: error path
            Err(it) if it.kind() == std::io::ErrorKind::AlreadyExists => {
                match fs::read_to_string(&gap).map(|it| it.trim().parse::<u64>().ok()) {
                    Ok(Some(held)) if held < build => {
                        match write_whole(&self.dir, &gap, build.to_string().as_bytes()) {
                            Ok(()) => (
                                format!(
                                    "{error}; build {build} is marked a gap in {}, over build {held}",
                                    gap.display()
                                ),
                                true,
                            ), // alloc: error path
                            Err(failed) => (
                                format!(
                                    "{error}; and marking build {build} a gap in {} over build {held}: {}, so a restart before the ledger is written whole compares with records older than build {build}'s",
                                    gap.display(),
                                    failed.why
                                ),
                                false,
                            ), // alloc: error path
                        }
                    }
                    // One naming no build is a gap after every record, and one naming this build or
                    // a later one is this gap or a later one: either covers this one, once it is on
                    // disk. An earlier process may have died before syncing it, so it is synced
                    // with its directory before the cut trusts it.
                    Ok(_) => match sync_held(&self.dir, &gap) {
                        Ok(()) => (error, true),
                        Err(failed) => (
                            format!(
                                "{error}; and {failed}, so it may not survive a power loss and the ledger is not cut on its word"
                            ),
                            false,
                        ), // alloc: error path
                    },
                    Err(read) => (
                        format!(
                            "{error}; and reading {} to mark build {build} a gap: {read}, so a restart before the ledger is written whole may compare with records older than build {build}'s",
                            gap.display()
                        ),
                        false,
                    ), // alloc: error path
                }
            }
            Err(it) => (
                format!(
                    "{error}; and marking build {build} a gap in {}: {it}, so a restart before the ledger is written whole compares with records older than build {build}'s",
                    gap.display()
                ),
                false,
            ), // alloc: error path
        }
    }

    /// The whole ledger, pruned past its cap, written over the file whole or not at all; the
    /// gap file gone once it is.
    fn whole(&mut self) -> Result<(), Unwritten> {
        let _span = tracing::info_span!(target: "blaze", "compact blaze why's ledger", len = self.len, live = self.live).entered();
        let dropped = self.ledger.prune(super::CAP_BYTES);
        if dropped > 0 {
            tracing::info!(target: "blaze", dropped, "blaze why's ledger shrank its oldest subjects to tombstones, or removed its oldest tombstones, to stay under its cap");
        }
        let bytes = self.ledger.file();
        let written = write_whole(&self.dir, &self.path, &bytes);
        if written.as_ref().map_or_else(|it| it.replaced, |()| true) {
            (self.len, self.live, self.compact) = (bytes.len() as u64, bytes.len() as u64, false);
        }
        written?;
        self.behind = None;
        match fs::remove_file(self.dir.join(GAP)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            // alloc: error path
            Err(error) => Err(Unwritten {
                why: format!(
                    "removing {} after writing the ledger whole: {error}",
                    self.dir.join(GAP).display()
                ),
                replaced: true,
            }),
        }
    }

    /// Appends `record`, with the magic first when the file is empty, synced (the directory
    /// too when this made the file). A write that fails is cut back to the length before it,
    /// so no torn record sits before the next one (rule 26). The file's length after.
    fn append(&self, record: &[u8]) -> Result<u64, String> {
        use std::io::Write as _;
        let path = &self.path;
        let fail = |doing: &str, error: std::io::Error| {
            format!("{doing} blaze why's ledger {}: {error}", path.display())
        }; // alloc: error path
        let mut file = fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(path)
            .map_err(|it| fail("opening", it))?;
        let len = file.metadata().map_err(|it| fail("reading", it))?.len();
        let magic = super::magic();
        let mut wrote = Ok(());
        if len == 0 {
            wrote = file.write_all(&magic);
        }
        // durable: a build's record in the why ledger: `blaze why` after a crash answers from it
        let wrote = wrote.and_then(|()| file.write_all(record)).and_then(|()| file.sync_data());
        let wrote = match len {
            // durable: the append made the ledger: its name survives a crash
            0 => wrote.and_then(|()| fs::File::open(&self.dir).and_then(|it| it.sync_all())),
            _ => wrote,
        };
        let Err(error) = wrote else {
            let magic = if len == 0 { magic.len() as u64 } else { 0 };
            return Ok(len + magic + record.len() as u64);
        };
        // durable: a failed append cut back: the ledger on disk ends at a whole record
        match file.set_len(len).and_then(|()| file.sync_data()) {
            Ok(()) => Err(format!("{}; cut back to its {len} bytes", fail("appending to", error))), // alloc: error path
            Err(cut) => Err(format!(
                "{}; and cutting it back to its {len} bytes: {cut}, so its next restore cuts the torn record",
                fail("appending to", error)
            )), // alloc: error path
        }
    }

    /// The gap [`GAP`] names: `Some(None)` when it is there and its build can't be read, which
    /// is a gap after every record.
    fn read_gap(&self, notes: &mut Vec<String>) -> Option<Option<u64>> {
        let at = self.dir.join(GAP);
        match fs::read_to_string(&at) {
            Ok(text) => {
                let build = text.trim().parse().ok();
                notes.push(match build {
                    Some(build) => format!("build {build}'s record did not reach blaze why's ledger ({})", at.display()), // alloc: error path
                    None => format!("a build's record did not reach blaze why's ledger ({}, which names no build)", at.display()), // alloc: error path
                });
                Some(build)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                notes.push(format!("reading {}: {error}", at.display())); // alloc: error path
                Some(None)
            }
        }
    }

    /// Removes the hidden files of whole writes a crash cut short (`.why.ledger.<pid>.tmp`):
    /// the daemon owns the history, so none is another's in flight.
    fn remove_stale(&self, notes: &mut Vec<String>) {
        let entries = match fs::read_dir(&self.dir) {
            Ok(it) => it,
            Err(error) => {
                // alloc: error path
                notes.push(format!(
                    "listing {} for hidden files of a whole write of blaze why's ledger: {error}",
                    self.dir.display()
                ));
                return;
            }
        };
        let prefix = format!(".{LEDGER}."); // alloc: once a daemon start
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.starts_with(&prefix)
                && name.ends_with(".tmp")
                && let Err(error) = fs::remove_file(entry.path())
            {
                notes.push(format!("removing {}: {error}", entry.path().display())); // alloc: error path
            }
        }
    }
}

/// Moves the ledger at `path` aside to `why.ledger.<kind>.<n>` in `dir`, one past the highest
/// `n` there, and removes all but the newest [`ASIDE`] of that kind: where it went.
fn aside(dir: &Path, path: &Path, kind: &str) -> Result<PathBuf, String> {
    let (to, n, taken) = next_aside(dir, kind)?;
    fs::rename(path, &to).map_err(|error| format!("moving it to {}: {error}", to.display()))?; // alloc: error path
    prune_aside(dir, kind, n, taken);
    Ok(to)
}

/// Copies `bytes` (the ledger's that a restore cuts off) to `why.ledger.<kind>.<n>` in `dir`,
/// synced with the directory before the cut, and removes all but the newest [`ASIDE`] of that
/// kind: where it went.
fn copy_aside(dir: &Path, kind: &str, bytes: &[u8]) -> Result<PathBuf, String> {
    use std::io::Write as _;
    let (to, n, taken) = next_aside(dir, kind)?;
    // Only a file this made is removed when the copy fails, never one that was there.
    let mut made = false;
    let copied = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&to)
        .and_then(|mut it| {
            made = true;
            // durable: a ledger's torn tail copied aside before the ledger is written over: the copy is the only one
            it.write_all(bytes).and_then(|()| it.sync_all())
        })
        // durable: the copy's name survives a crash
        .and_then(|()| fs::File::open(dir).and_then(|it| it.sync_all()));
    if let Err(error) = copied {
        if made {
            _ = fs::remove_file(&to);
        }
        return Err(format!("writing {}: {error}", to.display())); // alloc: error path
    }
    prune_aside(dir, kind, n, taken);
    Ok(to)
}

/// Where the next ledger set aside as `kind` goes in `dir` (one past the highest `n` there),
/// its `n`, and the `n`s taken. One past the highest `u64` is an error naming the file.
fn next_aside(dir: &Path, kind: &str) -> Result<(PathBuf, u64, Vec<u64>), String> {
    let prefix = format!("{LEDGER}.{kind}."); // alloc: error path
    // alloc: error path, the ledgers set aside before
    let taken: Vec<u64> = match fs::read_dir(dir) {
        Ok(entries) => entries
            .flatten()
            .filter_map(|it| {
                it.file_name().to_str().and_then(|name| name.strip_prefix(&prefix)?.parse().ok())
            })
            .collect(),
        Err(error) => {
            return Err(format!("listing {} to set the ledger aside: {error}", dir.display()));
        } // alloc: error path
    };
    let n = match taken.iter().max() {
        None => 1,
        Some(&max) => max.checked_add(1).ok_or_else(|| {
            // alloc: error path
            format!("{} is numbered the highest a ledger set aside can be, so no more can be set aside as {kind}: remove it", dir.join(format!("{prefix}{max}")).display())
        })?,
    };
    Ok((dir.join(format!("{prefix}{n}")), n, taken)) // alloc: error path
}

/// Removes the ledgers set aside as `kind` in `dir` but the newest [`ASIDE`], one more having
/// just been as `n`: `taken` are the `n`s there before it.
fn prune_aside(dir: &Path, kind: &str, n: u64, mut taken: Vec<u64>) {
    // `n - ASIDE` and below go; no sum, so no `n` near the top of `u64` overflows.
    let Some(edge) = n.checked_sub(ASIDE) else { return };
    taken.retain(|it| *it <= edge);
    for old in taken {
        let old = dir.join(format!("{LEDGER}.{kind}.{old}")); // alloc: error path
        if let Err(error) = fs::remove_file(&old) {
            tracing::warn!(target: "blaze", "removing an old ledger set aside, {}: {error}", old.display());
        }
    }
}

/// The step of [`write_whole`] after its rename: failing there, the file was already replaced.
const SYNC_DIR: &str = "syncing its directory";

/// Writes `bytes` as the file at `path` in `dir` (the ledger or [`GAP`]) whole or not at all:
/// to a hidden file (`.<name>.<pid>.tmp`, which [`File::remove_stale`] removes), synced,
/// renamed over it, and the directory synced.
fn write_whole(dir: &Path, path: &Path, bytes: &[u8]) -> Result<(), Unwritten> {
    use std::io::Write as _;
    let name = path.file_name().map_or_else(|| LEDGER.into(), |it| it.to_string_lossy()); // alloc: once a whole write
    let hidden = dir.join(format!(".{name}.{}.tmp", std::process::id())); // alloc: once a whole write
    // Each step's failure says which step it was.
    let step = |doing: &'static str| move |error: std::io::Error| (doing, error);
    let written = fs::File::create(&hidden)
        .map_err(step("making the hidden file"))
        .and_then(|mut file| {
            file.write_all(bytes).map_err(step("writing the hidden file"))?;
            // durable: the whole ledger, on disk before the rename that replaces the old one
            file.sync_all().map_err(step("syncing the hidden file"))
        })
        .and_then(|()| fs::rename(&hidden, path).map_err(step("renaming the hidden file over it")))
        // durable: the rename that replaces the ledger survives a crash
        .and_then(|()| fs::File::open(dir).and_then(|it| it.sync_all()).map_err(step(SYNC_DIR)));
    written.map_err(|(doing, error)| {
        _ = fs::remove_file(&hidden);
        // alloc: error path
        let why = format!(
            "writing {} whole through {}, {doing}: {error}",
            path.display(),
            hidden.display()
        );
        Unwritten { why, replaced: doing == SYNC_DIR }
    })
}

/// Makes the [`GAP`] file at `gap`, which an earlier process may have made and not synced,
/// durable: the file synced, then `dir`. The failure names the file and the step.
fn sync_held(dir: &Path, gap: &Path) -> Result<(), String> {
    fs::File::open(gap)
        // durable: a gap file an earlier process made and may not have synced
        .and_then(|it| it.sync_all())
        .map_err(|error| format!("syncing the gap file {} already there: {error}", gap.display())) // alloc: error path
        .and_then(|()| {
            // durable: the gap file's name survives a restart
            fs::File::open(dir).and_then(|it| it.sync_all()).map_err(|error| {
                format!(
                    "syncing {}, the directory of the gap file {} already there: {error}",
                    dir.display(),
                    gap.display()
                )
            }) // alloc: error path
        })
}

#[cfg(test)]
mod tests {
    use blaze_api::RebuildCause;
    use blaze_ids::ContentId;
    use blaze_kernel::{Miss, Part};

    use super::*;

    fn miss(subject: u8, code: u8) -> Miss {
        let parts = vec![Part::new("its code", Some(ContentId([code; 32])))];
        Miss::of_parts(
            "codegen dev template",
            ContentId([subject; 32]),
            "app::f".to_owned(),
            ContentId([code; 32]),
            parts,
        )
    }

    /// One build's record written, as a daemon writes it.
    fn built(file: &mut File, build: u64, code: u8) -> Vec<Rebuild> {
        let (rebuilt, _, record) = file.explain(build, true, vec![miss(1, code)]);
        file.write(build, &record).unwrap();
        rebuilt
    }

    fn names(dir: &Path) -> Vec<String> {
        // alloc: a test's listing
        let mut out: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|it| it.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        out.sort();
        out
    }

    /// The faults a ledger file can have at a start, each as it should be met: an empty file
    /// or part of the magic is begun again with nothing said lost; a record cut short is copied
    /// aside and cut off in place, and what came before it is compared with nothing, across a
    /// second restart too; another version's file is moved aside and every answer says so; and
    /// a failed append is a gap a restart sees.
    #[test]
    fn a_restore_cuts_a_torn_tail_moves_aside_what_it_cant_read_and_keeps_a_gap() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        let path = dir.join(LEDGER);

        // An empty file, and one with part of its magic, are a ledger never written.
        for len in [0, 9] {
            fs::write(&path, &super::super::magic()[..len]).unwrap();
            let (file, said) = File::restore(dir);
            assert!(said.torn.is_none(), "{:?}", said.torn);
            assert!(file.ledger.is_empty() && file.writable);
            assert_eq!(fs::metadata(&path).unwrap().len(), 0);
            assert_eq!(names(dir), [LEDGER], "nothing was moved aside");
        }

        // Two builds, the second's record cut short by a crash.
        let (mut file, _) = File::restore(dir);
        built(&mut file, 3, 7);
        let keep = fs::metadata(&path).unwrap().len() as usize;
        built(&mut file, 4, 8);
        let whole = fs::read(&path).unwrap();
        let torn = &whole[..whole.len() - 2];
        fs::write(&path, torn).unwrap();
        let (_, said) = File::restore(dir);
        assert!(said.torn.is_none());
        assert!(said.said.unwrap().contains("cut short"));
        assert_eq!(
            names(dir),
            [LEDGER, "why.ledger.tail.1"],
            "a torn tail is cut in place, not moved aside"
        );
        assert_eq!(
            fs::read(dir.join("why.ledger.tail.1")).unwrap().as_slice(),
            &torn[keep..],
            "what was cut is copied aside first"
        );
        // A restart before the next build: the gap the cut marked went into the file with the
        // ledger, so the second restore still compares build 3's record with nothing.
        let (mut file, said) = File::restore(dir);
        assert!(said.said.is_none() && said.torn.is_none(), "{:?} {:?}", said.said, said.torn);
        let rebuilt = built(&mut file, 5, 9);
        assert!(
            matches!(&rebuilt[0].cause, RebuildCause::NotExplained { what } if what.contains("build 3's") && what.contains("before build 4")),
            "{rebuilt:?}"
        );
        // Recomputed since, it is compared again.
        let rebuilt = built(&mut file, 6, 10);
        assert!(matches!(rebuilt[0].cause, RebuildCause::Parts { since: 5, .. }), "{rebuilt:?}");

        // Another version's file is moved aside, and every answer says so.
        let mut other = fs::read(&path).unwrap();
        other[20] ^= 1;
        fs::write(&path, &other).unwrap();
        let (file, said) = File::restore(dir);
        assert!(file.ledger.is_empty());
        assert!(said.torn.unwrap().contains("why.ledger.version.1"));
        assert_eq!(
            fs::read(dir.join("why.ledger.version.1")).unwrap(),
            other,
            "moved aside untouched"
        );
        assert!(!path.exists() || fs::metadata(&path).unwrap().len() <= 32);

        // An append that fails (the ledger's path is a directory) marks a gap that a restart
        // sees: the record before it is compared with nothing.
        let (mut file, _) = File::restore(dir);
        built(&mut file, 7, 1);
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        let (_, _, record) = file.explain(8, true, vec![miss(1, 2)]);
        let error = file.write(8, &record).unwrap_err();
        assert!(error.contains("marked a gap"), "{error}");
        assert_eq!(fs::read_to_string(dir.join(GAP)).unwrap(), "8");
        fs::remove_dir(&path).unwrap();
        fs::write(&path, super::super::magic()).unwrap();
        let (mut file, said) = File::restore(dir);
        assert!(said.said.unwrap().contains("build 8's record did not reach"));
        assert!(!dir.join(GAP).exists(), "the gap is in the ledger written whole, its file gone");
        let (mut again, _) = File::restore(dir);
        for file in [&mut file, &mut again] {
            file.ledger.items.insert(
                ("codegen dev template".to_owned(), ContentId([1; 32])),
                super::super::Last {
                    key: ContentId([1; 32]),
                    ends: Vec::new(),
                    item: "app::f".to_owned(),
                    kept: None,
                    step: "codegen dev template".to_owned(),
                    tomb: false,
                    build: 7,
                    parts: Vec::new(),
                    beyond: None,
                    unfinished: false,
                    subject: ContentId([1; 32]),
                },
            );
            let (rebuilt, _, _) = file.explain(9, true, vec![miss(1, 3)]);
            assert!(
                matches!(&rebuilt[0].cause, RebuildCause::NotExplained { what } if what.contains("before build 8")),
                "{rebuilt:?}"
            );
        }
    }

    /// A record whole by its length that does not decode is not cut as a torn tail: the file is
    /// moved aside untouched. With nothing read before it, every subject reads as first; with a
    /// record read before it, a gap is marked after that record and survives a restart, so a
    /// subject whose newer record was lost with the file is compared with nothing, never with
    /// the older record as if nothing came between.
    #[test]
    fn a_corrupt_record_moves_the_file_aside_and_marks_a_gap_after_what_was_read() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        let path = dir.join(LEDGER);
        let magic = super::super::magic().len();

        // Record 0's body flipped by a bit, record 1 whole after it.
        let (mut file, _) = File::restore(dir);
        built(&mut file, 3, 7);
        built(&mut file, 4, 8);
        let mut bytes = fs::read(&path).unwrap();
        // The first byte of its body: its map's major type flipped to a simple value.
        bytes[magic + 4] ^= 0x40;
        fs::write(&path, &bytes).unwrap();
        let (mut file, said) = File::restore(dir);
        let torn = said.torn.unwrap();
        assert!(torn.contains("record 0") && torn.contains("why.ledger.corrupt.1"), "{torn}");
        assert_eq!(
            fs::read(dir.join("why.ledger.corrupt.1")).unwrap(),
            bytes,
            "moved aside untouched"
        );
        assert!(file.ledger.is_empty() && file.ledger.gap.is_none());
        assert_eq!(built(&mut file, 5, 9)[0].cause, RebuildCause::First);

        // Three records, the middle one's body flipped: build 5's is read, build 6's and 7's
        // are lost, and the gap after build 5 is written with the ledger.
        let at = fs::metadata(&path).unwrap().len() as usize;
        built(&mut file, 6, 10);
        built(&mut file, 7, 11);
        let mut bytes = fs::read(&path).unwrap();
        bytes[at + 4] ^= 0x40;
        fs::write(&path, &bytes).unwrap();
        let (_, said) = File::restore(dir);
        let torn = said.torn.unwrap();
        assert!(torn.contains("record 1") && torn.contains("why.ledger.corrupt.2"), "{torn}");
        let (mut file, said) = File::restore(dir);
        assert!(said.torn.is_none() && said.said.is_none(), "{:?} {:?}", said.torn, said.said);
        assert_eq!(file.ledger.gap, Some(6));
        let rebuilt = built(&mut file, 8, 12);
        assert!(
            matches!(&rebuilt[0].cause, RebuildCause::NotExplained { what } if what.contains("build 5's") && what.contains("before build 6")),
            "{rebuilt:?}"
        );
    }

    /// Two builds' records, the second's cut short as a crash during its append leaves it:
    /// the torn file, and how long the record before the torn one ends.
    fn torn(dir: &Path) -> (Vec<u8>, usize) {
        let path = dir.join(LEDGER);
        let (mut file, _) = File::restore(dir);
        built(&mut file, 3, 7);
        let keep = fs::metadata(&path).unwrap().len() as usize;
        built(&mut file, 4, 8);
        let whole = fs::read(&path).unwrap();
        let torn = whole[..whole.len() - 2].to_vec(); // alloc: a test's bytes
        fs::write(&path, &torn).unwrap();
        (torn, keep)
    }

    /// Build 5 after a torn build 4: build 3's record is compared with nothing.
    fn sees_the_gap(dir: &Path) {
        let (mut file, _) = File::restore(dir);
        let rebuilt = built(&mut file, 5, 9);
        assert!(
            matches!(&rebuilt[0].cause, RebuildCause::NotExplained { what } if what.contains("build 3's") && what.contains("before build 4")),
            "{rebuilt:?}"
        );
    }

    /// A torn tail is never cut in place before its gap is on disk: the restore's whole write
    /// replaces the file, and when that write fails the file is cut only once [`GAP`] names the
    /// gap (a stale one naming an earlier build replaced), so the next restore still sees it.
    /// When the gap file can't be written either, the file is left as it was, and the next
    /// restore finds the torn tail, and the gap, again.
    #[test]
    fn a_torn_tail_is_cut_only_once_its_gap_is_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        let path = dir.join(LEDGER);
        // A directory where the whole write's hidden file goes makes that write fail.
        let hidden = dir.join(format!(".{LEDGER}.{}.tmp", std::process::id()));

        // The whole write fails, a stale gap file names build 2: it is replaced by build 4,
        // and only then is the file cut.
        let (_, keep) = torn(dir);
        fs::write(dir.join(GAP), "2").unwrap();
        fs::create_dir(&hidden).unwrap();
        let (file, said) = File::restore(dir);
        let said = said.said.unwrap();
        assert!(said.contains("making the hidden file") && said.contains("over build 2"), "{said}");
        assert_eq!(
            fs::read_to_string(dir.join(GAP)).unwrap(),
            "4",
            "the later gap is the one on disk"
        );
        assert_eq!(fs::metadata(&path).unwrap().len(), keep as u64, "cut once the gap is on disk");
        assert!(file.writable && file.behind.is_some());
        fs::remove_dir(&hidden).unwrap();
        sees_the_gap(dir);

        // Neither the whole write nor the gap file (a directory there) can be written: the
        // file is not cut, and the next restore finds the tail again.
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        let path = dir.join(LEDGER);
        let hidden = dir.join(format!(".{LEDGER}.{}.tmp", std::process::id()));
        let (torn, _) = torn(dir);
        fs::create_dir(&hidden).unwrap();
        fs::create_dir(dir.join(GAP)).unwrap();
        let (_, said) = File::restore(dir);
        let said = said.said.unwrap();
        assert!(said.contains("not cut back"), "{said}");
        assert_eq!(fs::read(&path).unwrap(), torn, "not cut while its gap is on disk nowhere");
        fs::remove_dir(&hidden).unwrap();
        fs::remove_dir(dir.join(GAP)).unwrap();
        sees_the_gap(dir);
    }

    /// A torn tail whose copy aside fails is not cut, and no record is written to the file:
    /// here the copy can't be numbered (one set aside before is numbered `u64::MAX`), which is
    /// a named error, and the file already there is not removed.
    #[test]
    fn a_torn_tail_is_not_cut_when_its_copy_fails() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        let path = dir.join(LEDGER);
        let (torn, _) = torn(dir);
        let top = dir.join(format!("{LEDGER}.tail.{}", u64::MAX));
        fs::write(&top, "planted").unwrap();
        let (mut file, said) = File::restore(dir);
        let said = said.said.unwrap();
        assert!(said.contains("numbered the highest") && said.contains("not cut back"), "{said}");
        assert!(!file.writable);
        assert_eq!(fs::read(&path).unwrap(), torn, "not cut");
        assert_eq!(fs::read_to_string(&top).unwrap(), "planted", "a file it did not make is kept");
        let (_, _, record) = file.explain(5, true, vec![miss(1, 9)]);
        file.write(5, &record).unwrap();
        assert_eq!(fs::read(&path).unwrap(), torn, "no record written to it");
    }
}
