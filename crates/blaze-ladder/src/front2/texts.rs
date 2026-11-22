//! The front end's one text table: every text an item of the program is read from (each
//! crate's files, each expansion the names library made), one row each, the user's files
//! first in the caller's order so a refusal's file index is the index the rungs already quote.
//! A row lends the library's text and outline; nothing is copied.
//!
//! The user's crate root is the caller's file as written (D104: nothing is appended; its
//! `fn main` is `std::main`'s input), so every position the caller's file has is the same in it.

use std::alloc::Global;
use std::vec::Vec;

use blaze_file_items::Outline;
use blaze_hash::HashMap;
use blaze_scope::resolve::{At, Names};

use crate::refusal::SourceFile;

/// One text.
#[derive(Clone, Copy)]
pub struct Row<'n> {
    pub krate: usize,
    pub at: At,
    /// The path a refusal prints.
    pub shown: &'n str,
    pub text: &'n str,
    pub outline: &'n Outline<'n>,
}

/// Why the table could not be made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TextsError {
    /// A file of the caller's that the names library did not read as a file of the program's
    /// crate (a file no `mod` reaches is not read: the rungs give only reached files).
    NotRead { path: std::string::String },
}

impl core::fmt::Display for TextsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            TextsError::NotRead { path } => write!(f, "the program's file `{path}` was not read by the names library: no `mod` reaches it"),
        }
    }
}

/// Every text, by index; and the index of each `(crate, At)`.
pub struct Texts<'n> {
    pub rows: Vec<Row<'n>, Global>,
    by_at: HashMap<(usize, At), u32, Global>,
}

/// A path with its leading `/` and `./` taken off, as the caller and the library each write
/// a package-relative path.
fn plain(p: &str) -> &str {
    let p = p.strip_prefix("./").unwrap_or(p);
    p.strip_prefix('/').unwrap_or(p)
}

impl<'n> Texts<'n> {
    /// A table with no row: what `Front2::from_records` starts with. A row holds its text's
    /// outline, which no crate record holds (the file's own `FileItems` record does): filling
    /// this on demand over records is owed with that record's reader.
    pub fn empty() -> Texts<'n> {
        Texts { rows: Vec::new_in(Global), by_at: HashMap::new_in(Global) }
    }

    /// The table: `user` (the program's files, as the caller gave them) first, then every other
    /// file and expansion of every crate in crate order.
    pub fn new(names: &'n Names<'n>, user: &[SourceFile<'_>]) -> Result<Texts<'n>, TextsError> {
        let mut rows: Vec<Option<Row<'n>>, Global> = Vec::new_in(Global);
        rows.resize(user.len(), None);
        let mut rest = Vec::new_in(Global);
        for (krate, c) in names.crates().iter().enumerate() {
            for (i, f) in c.files.iter().enumerate() {
                let row = Row { krate, at: At::File(i as u32), shown: f.shown, text: f.text, outline: &f.outline };
                let slot = if krate == 0 { user.iter().position(|u| plain(u.path) == plain(f.shown)) } else { None };
                match slot {
                    Some(s) => rows[s] = Some(row),
                    None => rest.push(row),
                }
            }
            // Expansions in key order: the table is a function of the names, not of map order.
            let mut calls: Vec<_, Global> = Vec::new_in(Global);
            calls.extend(c.expansions.iter().filter_map(|(k, e)| e.made.map(|m| (*k, m))));
            calls.sort_by_key(|(k, _)| *k);
            for (call, made) in calls {
                rest.push(Row { krate, at: At::Expansion(call), shown: c.name, text: made.text, outline: &made.outline });
            }
        }
        let mut out = Vec::with_capacity_in(rows.len() + rest.len(), Global);
        for (i, r) in rows.into_iter().enumerate() {
            out.push(r.ok_or_else(|| TextsError::NotRead { path: user[i].path.into() })?);
        }
        out.extend(rest);
        let mut by_at = HashMap::new_in(Global);
        for (i, r) in out.iter().enumerate() {
            by_at.insert((r.krate, r.at), i as u32);
        }
        Ok(Texts { rows: out, by_at })
    }

    /// The row of `at` in `krate`.
    pub fn index(&self, krate: usize, at: At) -> Option<u32> {
        self.by_at.get(&(krate, at)).copied()
    }

    pub fn row(&self, i: u32) -> &Row<'n> {
        &self.rows[i as usize]
    }

    /// The rows as the refusal printer's files.
    pub fn files(&self) -> Vec<SourceFile<'n>, Global> {
        let mut out = Vec::with_capacity_in(self.rows.len(), Global);
        out.extend(self.rows.iter().map(|r| SourceFile { path: r.shown, text: r.text }));
        out
    }
}
