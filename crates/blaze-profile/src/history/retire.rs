//! When a file that left the history may leave the disk: the plan of retirement, a pure
//! function of names, generations and pins, with no IO (its IO is [`super::pin`]).
//!
//! A merge replaces files by one, and the cap drops the oldest; either way those files leave
//! the history (they are *retired*) before they leave the disk. Retiring a file is part of the
//! manifest's commit that unlists it ([`super::manifest`]): the manifest says the history's
//! *generation*, one more with each commit that retires files, and every file retired and not
//! yet deleted, with the generation that retired it ([`Retired`]). A reader *pins* the history
//! before it reads anything: in [`PINS`] it makes a lock file it holds for as long as it reads
//! (`<id>.pin`), then reads the manifest, then says the generation that manifest is of
//! (`<id>.<generation>.gen`). The writer deletes a file retired at generation `r` only once
//! every live pin has said a generation of `r` or more ([`Retired::reapable`]); a pin that has
//! said none yet holds every file. A file is deleted before the manifest stops naming it.
//!
//! Why a reader never has a file deleted under it: each file it reads is one the manifest it
//! read lists. A file retired at `r` after that manifest was written has `r` above the
//! generation the reader says, and the writer looks at the pins only after it committed the
//! retirement: the reader's lock file was there by then, so it holds the file (it said a lower
//! generation, or none yet), or it was made after, and the manifest the reader then read lists
//! the file no more. Each row is read exactly once because the manifest is: a merge lists its
//! output and retires its inputs in one commit.
//!
//! A reader holds its pin until DuckDB has ended, so the explicit file lists it gives DuckDB
//! (never a glob, which DuckDB expands again at each query) stay on disk. A reader that dies
//! leaves a lock file no one holds, which the writer deletes. Until 2026-11-04 a merge deleted
//! its inputs at once, and a `blaze slow` racing it failed on DuckDB's "Cannot open file".

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use serde_derive::{Deserialize, Serialize};

#[cfg(test)]
mod tests;

/// The directory, in a history's directory, of its readers' pins.
pub const PINS: &str = "pins";

/// The history's generation and the files retired from it that are still on disk.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Retired {
    /// One more with each commit that retires files; never smaller, across the writer's
    /// restarts.
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
    /// whether the manifest must be committed again before the writer looks at the pins.
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

    /// Whether nothing was ever retired: generation 0.
    pub fn never(&self) -> bool {
        self.generation == 0 && self.files.is_empty()
    }

    /// Whether `name` is retired and not yet deleted.
    pub fn holds(&self, name: &str) -> bool {
        self.files.iter().any(|(it, _)| it == name)
    }

    /// The positions in [`Retired::files`] of the files no live pin of `pins` may still read:
    /// retired at a generation every pin has said, or all of them when no reader pins.
    ///
    /// A pin that said a generation above the history's holds every file too: it read a history
    /// before this one's generations (one adopted, [`super::manifest`]), so its number says
    /// nothing of them.
    pub fn reapable(&self, pins: &[Pinned]) -> Vec<usize> {
        let oldest =
            pins.iter().map(|it| it.filter(|&said| said <= self.generation).unwrap_or(0)).min();
        // alloc: once a flush that retired files
        (0..self.files.len()).filter(|&at| oldest.is_none_or(|it| self.files[at].1 <= it)).collect()
    }

    /// Forgets the files at `at` (ascending positions, as [`Retired::reapable`] gives them):
    /// deleted, so the manifest stops naming them. The generation stays.
    pub fn reaped(&mut self, at: &[usize]) {
        let mut position = 0;
        self.files.retain(|_| {
            let keep = at.binary_search(&position).is_err();
            position += 1;
            keep
        });
    }

    /// Whether every file is retired at a generation it has reached and is named as a file of
    /// the history's directory: what a manifest read back must hold. Why not, when not.
    pub fn check(&self) -> Result<(), &'static str> {
        for (name, at) in &self.files {
            if *at == 0 || *at > self.generation {
                return Err("a file is retired at a generation the history has not reached");
            }
            if name.is_empty() || name.contains(['/', '\\']) || name.starts_with('.') {
                return Err("a retired file is named as no file of the history's directory");
            }
        }
        Ok(())
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
