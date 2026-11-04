//! When a history file that left the history may leave the disk, and which files a reader
//! reads: the plan of retirement, a pure function of names, generations and pins, with no IO.
//!
//! A merge replaces files by one, and the cap drops the oldest; either way those files leave
//! the history (they are *retired*) before they leave the disk. The writer publishes each
//! retirement in the file [`RETIRED`] (atomically, by rename): the history's *generation*, one
//! more with each retirement, and every file retired and not yet deleted, with the generation
//! that retired it. A reader *pins* the history before it reads anything: in [`PINS`] it makes
//! a lock file it holds for as long as it reads (`<id>.pin`), then reads [`RETIRED`], then says
//! the generation it read (`<id>.<generation>.gen`), then lists the directory. The writer
//! deletes a file retired at generation `r` only once every live pin has said a generation of
//! `r` or more ([`Retired::reapable`]); a pin that has said none yet holds every file. A file
//! is deleted before [`RETIRED`] stops naming it.
//!
//! Why a reader sees each row exactly once ([`Snapshot::resolve`]):
//! - never a file deleted under it: a file it lists was on disk after its pin was made, so it
//!   was either not retired when the writer last looked at the pins (the writer publishes a
//!   retirement before it looks), or retired at a generation above the one the reader read,
//!   which its pin holds;
//! - never a row twice: a file [`RETIRED`] names as of the generation it read is left out, and
//!   so is a file a merged file it lists holds the rows of ([`super::compact::superseded`]),
//!   which covers a merge retired after the generation it read;
//! - never a row lost: a merge's output is renamed into place before its inputs are retired,
//!   so a file left out for its retirement has its rows in a file the reader lists.
//!
//! A reader holds its pin until DuckDB has ended, so the explicit file lists it gives DuckDB
//! (never a glob, which DuckDB expands again at each query) stay on disk. A reader that dies
//! leaves a lock file no one holds, which the writer deletes.

use std::path::{Path, PathBuf};

use super::compact::{self, Name};
use crate::schema;

#[cfg(test)]
mod tests;

/// The file in a history's directory naming its generation and the files retired from it and
/// not yet deleted.
pub const RETIRED: &str = "retired";

/// The directory, in a history's directory, of its readers' pins.
pub const PINS: &str = "pins";

const HEADER: &str = "blaze profile history retired v1";

/// The history's generation and the files retired from it that are still on disk.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Retired {
    /// One more with each retirement published; never smaller, across the writer's restarts.
    pub generation: u64,
    /// Each file retired and not yet deleted, with the generation that retired it, in the
    /// order retired.
    pub files: Vec<(String, u64)>,
}

/// What a live pin holds: `None` while its reader has not said the generation it read (every
/// file), `Some(g)` once it has (the files retired after `g`).
pub type Pinned = Option<u64>;

impl Retired {
    /// Retires `names` (those not retired already) at a new generation; whether any was, and so
    /// whether [`RETIRED`] must be published again before the writer looks at the pins.
    pub fn retire(&mut self, names: impl IntoIterator<Item = String>) -> bool {
        let generation = self.generation + 1;
        let before = self.files.len();
        for name in names {
            if !self.holds(&name) {
                self.files.push((name, generation));
            }
        }
        let retired = self.files.len() > before;
        if retired {
            self.generation = generation;
        }
        retired
    }

    /// Whether `name` is retired and not yet deleted.
    pub fn holds(&self, name: &str) -> bool {
        self.files.iter().any(|(it, _)| it == name)
    }

    /// The positions in [`Retired::files`] of the files no live pin of `pins` may still read:
    /// retired at a generation every pin has said, or all of them when no reader pins.
    pub fn reapable(&self, pins: &[Pinned]) -> Vec<usize> {
        let oldest = pins.iter().map(|it| it.unwrap_or(0)).min();
        // alloc: once a flush that retired files
        (0..self.files.len()).filter(|&at| oldest.is_none_or(|it| self.files[at].1 <= it)).collect()
    }

    /// Forgets the files at `at` (ascending positions, as [`Retired::reapable`] gives them):
    /// deleted, so [`RETIRED`] stops naming them. The generation stays.
    pub fn reaped(&mut self, at: &[usize]) {
        let mut position = 0;
        self.files.retain(|_| {
            let keep = at.binary_search(&position).is_err();
            position += 1;
            keep
        });
    }

    /// The text of [`RETIRED`].
    pub fn encode(&self) -> String {
        // alloc: the file's text, once a retirement or reap
        let mut out = String::with_capacity(
            64 + self.files.iter().map(|(it, _)| it.len() + 24).sum::<usize>(),
        );
        out.push_str(HEADER);
        out.push('\n');
        out.push_str("generation ");
        out.push_str(&self.generation.to_string());
        out.push('\n');
        for (name, generation) in &self.files {
            out.push_str(&generation.to_string());
            out.push(' ');
            out.push_str(name);
            out.push('\n');
        }
        out
    }

    /// [`RETIRED`]'s text read back; what is wrong with it when it is not one [`Retired::encode`]
    /// writes.
    pub fn decode(text: &str) -> Result<Retired, &'static str> {
        let mut lines = text.lines();
        if lines.next() != Some(HEADER) {
            return Err("its first line is not `blaze profile history retired v1`");
        }
        let generation = lines
            .next()
            .and_then(|it| it.strip_prefix("generation "))
            .and_then(|it| it.parse().ok())
            .ok_or("its second line is not `generation <number>`")?;
        // alloc: once a history opens
        let mut files = Vec::new();
        for line in lines {
            let (at, name) = line.split_once(' ').ok_or("a line is not `<generation> <file>`")?;
            let at: u64 = at.parse().map_err(|_| "a line's generation is not a number")?;
            if at == 0 || at > generation {
                return Err("a file is retired at a generation the history has not reached");
            }
            if name.is_empty() || name.contains(['/', '\\']) || name.starts_with('.') {
                return Err("a line names no file of the history's directory");
            }
            // alloc: a file's name, once a history opens
            files.push((name.to_owned(), at));
        }
        Ok(Retired { generation, files })
    }
}

/// What a file of [`PINS`] is, by its name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PinFile<'a> {
    /// `.<id>.tmp`: a pin's lock file being made, locked before it gets its name.
    Making { id: &'a str },
    /// `<id>.pin`: a reader's lock file, held as long as it reads.
    Lock { id: &'a str },
    /// `<id>.<generation>.gen`: the generation the reader `id` read.
    Said { id: &'a str, generation: u64 },
}

impl<'a> PinFile<'a> {
    pub fn parse(name: &'a str) -> Option<PinFile<'a>> {
        if let Some(id) = name.strip_prefix('.').and_then(|it| it.strip_suffix(".tmp")) {
            return valid(id).then_some(PinFile::Making { id });
        }
        if let Some(id) = name.strip_suffix(".pin") {
            return valid(id).then_some(PinFile::Lock { id });
        }
        let (id, generation) = name.strip_suffix(".gen")?.rsplit_once('.')?;
        let generation = generation.parse().ok()?;
        valid(id).then_some(PinFile::Said { id, generation })
    }

    /// The file's name in [`PINS`].
    pub fn file_name(&self) -> String {
        match *self {
            // alloc: a file's name, once a pin
            PinFile::Making { id } => format!(".{id}.tmp"),
            // alloc: a file's name, once a pin
            PinFile::Lock { id } => format!("{id}.pin"),
            // alloc: a file's name, once a pin
            PinFile::Said { id, generation } => format!("{id}.{generation}.gen"),
        }
    }

    pub fn id(&self) -> &'a str {
        match *self {
            PinFile::Making { id } | PinFile::Lock { id } | PinFile::Said { id, .. } => id,
        }
    }

    /// The process that made it: an id is `<pid>-<n>`.
    pub fn pid(&self) -> Option<u32> {
        self.id().split_once('-')?.0.parse().ok()
    }
}

/// An id is `<pid>-<n>`, digits and one dash.
fn valid(id: &str) -> bool {
    id.split_once('-').is_some_and(|(pid, n)| {
        !pid.is_empty()
            && !n.is_empty()
            && pid.bytes().all(|it| it.is_ascii_digit())
            && n.bytes().all(|it| it.is_ascii_digit())
    })
}

/// What the live pins of `live` (the ids whose lock files are held) hold, given the
/// generations `said` (each `(id, generation)` read from [`PINS`]): one entry a live pin, in
/// the order of `live`. A pin whose generation was not seen holds every file.
pub fn pinned(live: &[&str], said: &[(&str, u64)]) -> Vec<Pinned> {
    // alloc: once a flush that retired files
    live.iter()
        .map(|id| said.iter().filter(|(it, _)| it == id).map(|&(_, generation)| generation).max())
        .collect()
}

/// The files of each table a reader reads from one directory: [`Snapshot::resolve`] of its
/// listing, each table's names in byte order, so a reader's script is the same for the same
/// directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    /// The directory the files are in.
    pub(crate) dir: PathBuf,
    /// One list a table of [`schema::TABLES`], in its order: the files' names in `dir`.
    pub(crate) files: Vec<Vec<String>>,
}

impl Snapshot {
    /// The files of `listing` (the names in `dir`) a reader reads, given what the directory's
    /// [`RETIRED`] said as the reader pinned it: each `<anything>.<table>.parquet` of a table of
    /// [`schema::TABLES`] that is not hidden, not retired by then, and not superseded by a merged
    /// file of the listing.
    pub fn resolve(dir: &Path, listing: &[String], retired: &Retired) -> Snapshot {
        // alloc: once a reader pins a directory
        let mut files: Vec<Vec<String>> = schema::TABLES.iter().map(|_| Vec::new()).collect();
        // alloc: once a reader pins a directory
        let mut taken: Vec<(usize, &str)> = Vec::new();
        for name in listing {
            if name.starts_with('.') || retired.holds(name) {
                continue;
            }
            let Some(table) = table_of(name) else { continue };
            taken.push((table, name.as_str()));
        }
        // alloc: once a reader pins a directory
        let named: Vec<(usize, Name)> = taken
            .iter()
            .enumerate()
            .filter_map(|(at, (_, name))| Some((at, Name::parse(name)?)))
            .collect();
        // alloc: once a reader pins a directory
        let names: Vec<Name> = named.iter().map(|(_, it)| *it).collect();
        // alloc: once a reader pins a directory
        let mut gone: Vec<usize> =
            compact::superseded(&names).into_iter().map(|it| named[it].0).collect();
        gone.sort_unstable();
        for (at, (table, name)) in taken.iter().enumerate() {
            if gone.binary_search(&at).is_err() {
                // alloc: a file's name, once a reader pins a directory
                files[*table].push((*name).to_owned());
            }
        }
        for it in &mut files {
            it.sort_unstable();
        }
        // alloc: once a reader pins a directory
        Snapshot { dir: dir.to_owned(), files }
    }

    /// The directory the files are in.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The names, in [`Snapshot::dir`], of the files of `table` (an index into
    /// [`schema::TABLES`]) a reader reads.
    pub fn files(&self, table: usize) -> &[String] {
        &self.files[table]
    }
}

/// The table of [`schema::TABLES`] a file `<anything>.<table>.parquet` is of.
fn table_of(name: &str) -> Option<usize> {
    let stem = name.strip_suffix(".parquet")?;
    schema::TABLES.iter().position(|table| {
        stem.strip_suffix(table.name).is_some_and(|rest| rest.ends_with('.') && rest.len() > 1)
    })
}
