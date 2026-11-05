//! Which history files to merge, and which a merge left behind: the plan of compaction, a pure
//! function of the files' names, with no IO.
//!
//! A flush writes one file a table, `<build>-<seq:08>.<table>.parquet` (level 0). A merge
//! writes the rows of files of one build name and one table into one file,
//! `<build>-<first:08>-<last:08>-<level>.<table>.parquet`, holding the rows of that build's
//! files of the table whose seqs lie in `first..=last`. Rows are copied as their column
//! chunks: nothing is decoded or encoded again, and each row group stays the size its flush
//! wrote. While a build runs, a level of [`FAN_IN`] files becomes one file a level up; as it
//! ends, each of its tables becomes one file ([`plan`]).
//!
//! The history renames a merge's output into place, then in one commit of the manifest lists
//! it and retires its inputs (unlisted at once, deleted once no reader pins them:
//! [`super::retire`]), so a reader reads the inputs or the output, never both. A history from
//! before manifests may hold both (a crash between a merge's rename and its deletes): the
//! manifest made of its files leaves out the inputs ([`superseded`]).

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use crate::schema;

#[cfg(test)]
mod tests;

/// Files of one level of one build's table that are merged into one file a level up. Each row
/// is copied once a level, so a 52-minute build (about 630 flushes) has its rows copied about
/// 3 times, and 16 keeps a glob small: while a build runs, a (build, table) holds at most
/// `FAN_IN - 1` files a level, and its levels grow as log16 of its flushes (a 629-flush build:
/// at most about 32 files a table; after its end, 1 a table).
pub(crate) const FAN_IN: usize = 16;

/// What a history file's name says: the build it is named for, its table (an index into
/// [`schema::TABLES`]), the flushes it holds the rows of (`first..=last`), and its level (0
/// for a flush's file).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Name {
    pub(crate) build: u64,
    pub(crate) table: usize,
    pub(crate) first: u64,
    pub(crate) last: u64,
    pub(crate) level: u32,
}

impl Name {
    /// The name of a history file, `<build>-<seq:08>.<table>.parquet` or
    /// `<build>-<first:08>-<last:08>-<level>.<table>.parquet`; `None` for any other file.
    pub(crate) fn parse(name: &str) -> Option<Name> {
        let (stem, table) = name.strip_suffix(".parquet")?.split_once('.')?;
        let table = schema::TABLES.iter().position(|it| it.name == table)?;
        let mut parts = stem.split('-');
        let build = parts.next()?.parse().ok()?;
        let first = parts.next()?.parse().ok()?;
        match (parts.next(), parts.next(), parts.next()) {
            (None, _, _) => Some(Name { build, table, first, last: first, level: 0 }),
            (Some(last), Some(level), None) => {
                let last = last.parse().ok()?;
                let level = level.parse().ok()?;
                (level >= 1 && first <= last).then_some(Name { build, table, first, last, level })
            }
            _ => None,
        }
    }

    /// The file's name in the history's directory.
    pub(crate) fn file_name(&self) -> String {
        let Name { build, first, last, level, .. } = *self;
        let table = schema::TABLES[self.table].name;
        match level {
            // alloc: a file's name, once a file
            0 => format!("{build}-{first:08}.{table}.parquet"),
            // alloc: a file's name, once a file
            _ => format!("{build}-{first:08}-{last:08}-{level}.{table}.parquet"),
        }
    }

    fn group(&self) -> (u64, usize) {
        (self.build, self.table)
    }
}

/// Files to merge into one: `inputs` index the files given to [`plan`], in seq order, and
/// `output` names the file they become.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Merge {
    pub(crate) inputs: Vec<usize>,
    pub(crate) output: Name,
}

/// The merges that bound the files of each (build, table) of `files`: while a level holds
/// [`FAN_IN`] files or more, its oldest `FAN_IN` (by `first`) become one file a level up; and
/// as the build `ended` ends, each of its tables of 2 files or more becomes one file, a level
/// above its highest. A merge that would take another's output takes that output's inputs
/// instead, so each file is in at most one merge and each row is copied once a plan. Groups in
/// (build, table) order; a single file is never merged.
pub(crate) fn plan(files: &[Name], ended: Option<u64>) -> Vec<Merge> {
    // alloc: once a flush that compacts
    let mut order: Vec<usize> = (0..files.len()).collect();
    order.sort_unstable_by_key(|&at| {
        (files[at].group(), files[at].first, files[at].last, files[at].level)
    });
    // alloc: once a flush that compacts
    let mut merges = Vec::new();
    for group in order.chunk_by(|&a, &b| files[a].group() == files[b].group()) {
        plan_group(files, group, ended, &mut merges);
    }
    merges
}

/// A file of a group as the plan goes: one of the files given, or the output of merges.
struct Item {
    name: Name,
    /// The files given that it holds the rows of: one for a file given.
    covers: Vec<usize>,
}

fn plan_group(files: &[Name], group: &[usize], ended: Option<u64>, merges: &mut Vec<Merge>) {
    // alloc: a group's files, once a flush that compacts
    let mut items: Vec<Item> =
        group.iter().map(|&at| Item { name: files[at], covers: vec![at] }).collect();
    // Each merge takes `FAN_IN` items and gives back one, so a group of n files merges fewer
    // than n times.
    for _ in 0..group.len() {
        let Some(level) = lowest_full(&items) else { break };
        // alloc: a merge's files, once a merge
        let mut at: Vec<usize> =
            (0..items.len()).filter(|&it| items[it].name.level == level).collect();
        at.sort_unstable_by_key(|&it| (items[it].name.first, items[it].name.last));
        at.truncate(FAN_IN);
        fold(&mut items, &mut at, level + 1);
    }
    let build = files[group[0]].build;
    if ended == Some(build) && items.len() >= 2 {
        let level = items.iter().map(|it| it.name.level).max().unwrap_or(0) + 1;
        // alloc: a merge's files, once a merge
        let mut at: Vec<usize> = (0..items.len()).collect();
        fold(&mut items, &mut at, level);
    }
    for item in items.into_iter().filter(|it| it.covers.len() >= 2) {
        let mut inputs = item.covers;
        inputs.sort_unstable_by_key(|&at| (files[at].first, files[at].last));
        merges.push(Merge { inputs, output: item.name });
    }
}

/// The lowest level of `items` holding [`FAN_IN`] files or more.
fn lowest_full(items: &[Item]) -> Option<u32> {
    let mut levels: Vec<u32> = items.iter().map(|it| it.name.level).collect(); // alloc: a group's levels, once a merge
    levels.sort_unstable();
    levels.chunk_by(|a, b| a == b).find(|it| it.len() >= FAN_IN).map(|it| it[0])
}

/// Replaces the items at the positions `at` with one item of `level` holding all their rows.
fn fold(items: &mut Vec<Item>, at: &mut [usize], level: u32) {
    at.sort_unstable_by(|a, b| b.cmp(a));
    let mut name = items[at[0]].name;
    name.level = level;
    // alloc: a merge's files, once a merge
    let mut covers = Vec::new();
    for &it in at.iter() {
        let item = items.remove(it);
        name.first = name.first.min(item.name.first);
        name.last = name.last.max(item.name.last);
        covers.extend(item.covers);
    }
    items.push(Item { name, covers });
}

/// The files of `files` a merge already holds the rows of: of the same build and table as a
/// merged file, of a lower level, and with their seqs inside its `first..=last`. A crash
/// between a merge's rename and its deletes, before manifests, left them; read, their rows
/// would count twice. In ascending order.
pub(crate) fn superseded(files: &[Name]) -> Vec<usize> {
    // alloc: once a history opens
    let mut order: Vec<usize> = (0..files.len()).collect();
    order.sort_unstable_by_key(|&at| files[at].group());
    // alloc: once a history opens
    let mut gone = Vec::new();
    for group in order.chunk_by(|&a, &b| files[a].group() == files[b].group()) {
        for &merged in group.iter().filter(|&&it| files[it].level >= 1) {
            let outer = files[merged];
            gone.extend(group.iter().copied().filter(|&it| {
                let inner = files[it];
                inner.level < outer.level && outer.first <= inner.first && inner.last <= outer.last
            }));
        }
    }
    gone.sort_unstable();
    gone.dedup();
    gone
}
