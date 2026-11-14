//! The values of measured items that may only go down (docs/lints.md, "Complexity"): one file,
//! `crates/blaze-lints/baselines/items.tsv`, one `measure<TAB>path<TAB>fingerprint<TAB>value`
//! line per item over its measure's bound when the file was first written, sorted. Where
//! [`super`]'s files hold a count of findings per crate, this holds one number per function,
//! type or file, so a function at 40 may not grow to 41 while another shrinks.
//!
//! A line names its item two ways: by `path` (the crate, its modules and the item's name, as
//! the finding names it) and by `fingerprint`, a hash of the item's tokens with its own name
//! left out ([`fingerprint`]). An item matches a line by both; by its path alone when no line
//! has both (an edit changed its tokens); by its fingerprint alone when no line has its path
//! (a rename, or a move to another module, with no edit). Only a rename together with an edit
//! loses its line, and then its value is new code's and held to the bound.
//!
//! The lints read the file in the build ([`Loaded`], keyed by its bytes): an item over its bound
//! is a finding when no line matches it or its value is over the line's. A bless runs the lints
//! with no file loaded, so each item over its bound says its line in a note ([`note`]), and
//! [`Items::blessed`] keeps, of what the run found, only the items the old file held, each at
//! the lower of its two values: a value never rises and an item is never added after the first
//! bless. The gate's comparison is [`Items::off`].

use std::{
    alloc::Global,
    path::{Path, PathBuf},
    sync::Arc,
};

/// The file of the baselines directory these lines are in: [`super::read`] passes it by.
pub const FILE: &str = "items.tsv";

/// What a finding's note starts with when it says its line ([`note`], [`from_note`]).
pub const NOTE: &str = "item baseline line: ";

/// The file's first lines.
const HEADER: &str = "# Measured items over their bound, each held at most at its value (docs/lints.md, \"Complexity\").\n\
                      # measure<TAB>path<TAB>fingerprint<TAB>value, sorted; a value only goes down, and no item is added\n\
                      # after the first bless. Written by a bless from the lints' notes (`item baseline line: ..`).\n";

/// What names one line: the measure, the item's path and its fingerprint.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Key {
    /// The measure's name ([`crate::blaze`]'s complexity lints: `function_weight`,
    /// `function_lines`, `fields`, ..).
    pub measure: String,
    /// The item: `crate::module::Type::name`, or a file's module.
    pub path: String,
    /// The hash of the item's tokens, its own name left out.
    pub fingerprint: u64,
}

/// One line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub key: Key,
    pub value: u64,
}

/// The lines of the file, sorted by key with no key twice (so a write is one text for one
/// set), and the index [`Items::held`] finds a renamed item by. Read once and written once: a
/// sorted list searched by halves is all a lookup needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Items {
    entries: Vec<(Key, u64), Global>,
    /// By measure then fingerprint, sorted: the highest value, and how many lines have the
    /// pair (a value is found this way only when one does).
    prints: Vec<Print, Global>,
}

/// One entry of [`Items::prints`].
#[derive(Clone, Debug, PartialEq, Eq)]
struct Print {
    measure: String,
    fingerprint: u64,
    value: u64,
    lines: u32,
}

impl Default for Items {
    fn default() -> Items {
        Items { entries: Vec::new_in(Global), prints: Vec::new_in(Global) }
    }
}

/// What is wrong with a file of lines.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ItemsError {
    /// A line that is not `measure<TAB>path<TAB>fingerprint<TAB>value`, by its number from 1.
    Line { line: usize, text: String },
    /// A key a line before already has.
    Twice { line: usize, key: Key },
    /// A line out of order: the file is sorted, so two writes of one set are one text.
    Order { line: usize },
    /// The file could not be read.
    Read { file: String, error: String },
}

impl std::fmt::Display for ItemsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ItemsError::Line { line, text } => write!(
                f,
                "{FILE}:{line}: `{text}` is not `measure<TAB>path<TAB>fingerprint<TAB>value` \
                 (a fingerprint of 16 hex digits, a value of decimal digits)"
            ),
            ItemsError::Twice { line, key } => write!(
                f,
                "{FILE}:{line}: `{}` `{}` `{:016x}` is on an earlier line too: one line per item",
                key.measure, key.path, key.fingerprint
            ),
            ItemsError::Order { line } => {
                write!(f, "{FILE}:{line}: the lines are sorted by measure, path and fingerprint")
            }
            ItemsError::Read { file, error } => write!(f, "{file} can't be read: {error}"),
        }
    }
}

/// Where a run's values are off the file's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Off {
    /// Over: an item over its bound that no line holds.
    New { key: Key, now: u64 },
    /// Over: an item whose value is above its line's.
    Grew { key: Key, was: u64, now: u64 },
    /// Under: an item whose value is below its line's, stale until a bless lowers it.
    Shrank { key: Key, was: u64, now: u64 },
    /// Under: a line no item of the run matches (it is under its bound now, or gone).
    Gone { key: Key, was: u64 },
}

impl Off {
    /// Whether the run is worse than the file here.
    pub fn over(&self) -> bool {
        matches!(self, Off::New { .. } | Off::Grew { .. })
    }
}

impl std::fmt::Display for Off {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Off::New { key, now } => {
                write!(f, "`{}` {} is {now}, and no line of {FILE} holds it", key.path, key.measure)
            }
            Off::Grew { key, was, now } => {
                write!(f, "`{}` {} is {now}, over its line's {was}", key.path, key.measure)
            }
            Off::Shrank { key, was, now } => {
                write!(f, "`{}` {} is {now}, under its line's {was}: bless", key.path, key.measure)
            }
            Off::Gone { key, was } => write!(
                f,
                "`{}` {} (held at {was}) is under its bound or gone: bless",
                key.path, key.measure
            ),
        }
    }
}

impl Items {
    /// The lines of `entries`; a key given twice keeps its last value.
    pub fn of(entries: impl IntoIterator<Item = Entry>) -> Items {
        let mut all: Vec<(Key, u64), Global> = Vec::new_in(Global);
        all.extend(entries.into_iter().map(|it| (it.key, it.value)));
        // Stable: of equal keys the last given stays last, and is the one kept.
        all.sort_by(|(a, _), (b, _)| a.cmp(b));
        let mut items = Items::default();
        for (key, value) in all {
            match items.entries.last_mut() {
                Some((last, held)) if *last == key => *held = value,
                _ => items.entries.push((key, value)),
            }
        }
        items.index();
        items
    }

    fn index(&mut self) {
        self.prints.clear();
        for (key, value) in &self.entries {
            self.prints.push(Print {
                measure: key.measure.clone(),
                fingerprint: key.fingerprint,
                value: *value,
                lines: 1,
            });
        }
        self.prints.sort_by(|a, b| {
            (a.measure.as_str(), a.fingerprint).cmp(&(b.measure.as_str(), b.fingerprint))
        });
        let mut merged: Vec<Print, Global> = Vec::with_capacity_in(self.prints.len(), Global);
        for print in self.prints.drain(..) {
            match merged.last_mut() {
                Some(last) if last.measure == print.measure && last.fingerprint == print.fingerprint => {
                    last.value = last.value.max(print.value);
                    last.lines += 1;
                }
                _ => merged.push(print),
            }
        }
        self.prints = merged;
    }

    /// The index of the line `key`.
    fn find(&self, key: &Key) -> Option<usize> {
        self.entries.binary_search_by(|(it, _)| it.cmp(key)).ok()
    }

    /// The indices of the lines of `measure` at `path`, any fingerprint.
    fn at_path(&self, measure: &str, path: &str) -> std::ops::Range<usize> {
        fn of(it: &Key) -> (&str, &str) {
            (it.measure.as_str(), it.path.as_str())
        }
        let start = self.entries.partition_point(|(it, _)| of(it) < (measure, path));
        let end = self.entries.partition_point(|(it, _)| of(it) <= (measure, path));
        start..end
    }

    /// The highest value of the lines of `measure` with `fingerprint`, when exactly one line
    /// has the pair.
    fn by_print(&self, measure: &str, fingerprint: u64) -> Option<u64> {
        let at = self
            .prints
            .binary_search_by(|it| (it.measure.as_str(), it.fingerprint).cmp(&(measure, fingerprint)))
            .ok()?;
        let print = &self.prints[at];
        (print.lines == 1).then_some(print.value)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Every line, in the file's order.
    pub fn entries(&self) -> impl Iterator<Item = Entry> + '_ {
        self.entries.iter().map(|(key, value)| Entry { key: key.clone(), value: *value })
    }

    /// Reads the file's `text`. `#` lines and empty lines are comments.
    pub fn parse(text: &str) -> Result<Items, ItemsError> {
        let mut items = Items::default();
        let mut last: Option<Key> = None;
        for (at, line) in text.lines().enumerate() {
            let number = at + 1;
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let entry =
                parse_line(line).ok_or_else(|| ItemsError::Line { line: number, text: line.to_owned() })?;
            if last.as_ref().is_some_and(|it| *it == entry.key) {
                return Err(ItemsError::Twice { line: number, key: entry.key });
            }
            if last.as_ref().is_some_and(|it| *it > entry.key) {
                return Err(ItemsError::Order { line: number });
            }
            last = Some(entry.key.clone());
            // In order and new, as just checked: the list stays sorted.
            items.entries.push((entry.key, entry.value));
        }
        items.index();
        Ok(items)
    }

    /// The file's text: the header, then one sorted line per item.
    pub fn render(&self) -> String {
        let mut text = String::from(HEADER);
        for (key, value) in &self.entries {
            text.push_str(&line_of(key, *value));
            text.push('\n');
        }
        text
    }

    /// The value the file holds `measure` of the item at `path` with `fingerprint` to: its
    /// line; else, when no line has both, the one line with its path (the highest, for a path
    /// two items share, as two impls' `Type::fmt`); else the one line with its fingerprint.
    pub fn held(&self, measure: &str, path: &str, fingerprint: u64) -> Option<u64> {
        let key = Key { measure: measure.to_owned(), path: path.to_owned(), fingerprint };
        self.matching(&key).map(|at| self.entries[at].1)
    }

    /// Where `now`, a run's lines (every item over its bound, as a bless run reads them), is off
    /// these: the run's items in key order, then the lines no item matched.
    pub fn off(&self, now: &Items) -> Vec<Off, Global> {
        let mut off = Vec::new_in(Global);
        let mut matched = Vec::with_capacity_in(self.entries.len(), Global);
        matched.resize(self.entries.len(), false);
        for (key, value) in &now.entries {
            let value = *value;
            match self.matching(key) {
                None => off.push(Off::New { key: key.clone(), now: value }),
                Some(at) => {
                    matched[at] = true;
                    let was = self.entries[at].1;
                    if value > was {
                        off.push(Off::Grew { key: key.clone(), was, now: value });
                    } else if value < was {
                        off.push(Off::Shrank { key: key.clone(), was, now: value });
                    }
                }
            }
        }
        for ((key, was), matched) in self.entries.iter().zip(matched.iter()) {
            if !matched {
                off.push(Off::Gone { key: key.clone(), was: *was });
            }
        }
        off
    }

    /// The index of the line `key`, an item of a run, matches, as [`Items::held`] says.
    fn matching(&self, key: &Key) -> Option<usize> {
        if let Some(at) = self.find(key) {
            return Some(at);
        }
        let same_path = self.at_path(&key.measure, &key.path);
        if !same_path.is_empty() {
            // The highest; of equal values, the first.
            return same_path.rev().max_by_key(|&at| self.entries[at].1);
        }
        self.by_print(&key.measure, key.fingerprint)?;
        self.entries
            .iter()
            .position(|(old, _)| old.measure == key.measure && old.fingerprint == key.fingerprint)
    }

    /// The lines after a bless of `now`, a run's lines: with no lines yet (the first bless),
    /// all of `now`; else each item of `now` some line matches, under its new key, at the lower
    /// of the two values. Nothing rises, nothing is added, and a line no item matches goes.
    pub fn blessed(&self, now: &Items) -> Items {
        if self.entries.is_empty() {
            return now.clone();
        }
        let kept = now.entries.iter().filter_map(|(key, value)| {
            let was = self.entries[self.matching(key)?].1;
            Some(Entry { key: key.clone(), value: (*value).min(was) })
        });
        Items::of(kept)
    }

    /// The `most` lines furthest over their bound, by value over `bound(measure)` (a measure
    /// `bound` has no number for is left out), the highest first, then by key.
    pub fn worst(&self, most: usize, bound: impl Fn(&str) -> Option<u64>) -> Vec<Entry, Global> {
        let mut all = Vec::new_in(Global);
        for (key, value) in &self.entries {
            let value = *value;
            let Some(bound) = bound(&key.measure) else { continue };
            // A ratio in thousandths: integer, so the order is exact.
            let ratio = value.saturating_mul(1000) / bound.max(1);
            all.push((ratio, Entry { key: key.clone(), value }));
        }
        all.sort_by(|(a, x), (b, y)| b.cmp(a).then_with(|| x.key.cmp(&y.key)));
        let mut worst = Vec::with_capacity_in(most.min(all.len()), Global);
        worst.extend(all.into_iter().take(most).map(|(_, it)| it));
        worst
    }
}

fn parse_line(line: &str) -> Option<Entry> {
    let mut parts = line.split('\t');
    let measure = parts.next().filter(|it| !it.is_empty())?;
    let path = parts.next().filter(|it| !it.is_empty())?;
    let print = parts.next().filter(|it| it.len() == 16)?;
    let value = parts.next().filter(|it| !it.is_empty() && it.bytes().all(|b| b.is_ascii_digit()))?;
    if parts.next().is_some() {
        return None;
    }
    Some(Entry {
        key: Key {
            measure: measure.to_owned(),
            path: path.to_owned(),
            fingerprint: u64::from_str_radix(print, 16).ok()?,
        },
        value: value.parse().ok()?,
    })
}

fn line_of(key: &Key, value: u64) -> String {
    format!("{}\t{}\t{:016x}\t{value}", key.measure, key.path, key.fingerprint)
}

/// The note a finding says its line in.
pub fn note(key: &Key, value: u64) -> String {
    format!("{NOTE}{}", line_of(key, value))
}

/// The line a finding's note says, if it is one ([`note`]).
pub fn from_note(text: &str) -> Option<Entry> {
    parse_line(text.strip_prefix(NOTE)?)
}

/// The fingerprint of an item from its tokens, trivia and its own name left out: the same for
/// the same code under another name, and for the same code laid out or commented otherwise.
pub fn fingerprint<'a>(tokens: impl IntoIterator<Item = &'a str>) -> u64 {
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = blaze_hash::FixedHasher::default();
    for token in tokens {
        token.hash(&mut hasher);
    }
    hasher.finish()
}

/// The lines as the lints read them in a build: the file's lines, compared and hashed by its
/// bytes, so a lint query keyed by the configuration is keyed by them.
#[derive(Clone)]
pub struct Loaded {
    pub path: PathBuf,
    pub id: [u8; 32],
    pub items: Arc<Items>,
}

impl PartialEq for Loaded {
    fn eq(&self, other: &Loaded) -> bool {
        self.path == other.path && self.id == other.id
    }
}

impl Eq for Loaded {}

impl std::hash::Hash for Loaded {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.path.hash(state);
        self.id.hash(state);
    }
}

impl std::fmt::Debug for Loaded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Loaded")
            .field("path", &self.path)
            .field("lines", &self.items.len())
            .finish()
    }
}

/// The lines of `dir`'s [`FILE`] (the ratchet's baselines directory), the look at it kept in
/// `looks` so the build reruns when it changes; none when the directory has no such file.
pub fn load(dir: &Path, looks: &blaze_vfs::outside::Looks) -> Result<Option<Loaded>, ItemsError> {
    let path = dir.join(FILE);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // The path, absent: a file made there later is a change the build sees.
            if looks.keeps() {
                looks.push(&path, blaze_vfs::outside::LookKind::Entry, None);
            }
            return Ok(None);
        }
        Err(error) => {
            return Err(ItemsError::Read { file: path.display().to_string(), error: error.to_string() });
        }
    };
    let id = blaze_ids::b3::hash(&bytes);
    if looks.keeps() {
        looks.push(&path, blaze_vfs::outside::LookKind::File, Some(id));
    }
    let text = std::str::from_utf8(&bytes).map_err(|error| ItemsError::Read {
        file: path.display().to_string(),
        error: error.to_string(),
    })?;
    let items = Items::parse(text)?;
    Ok(Some(Loaded { path, id, items: Arc::new(items) }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(measure: &str, path: &str, fingerprint: u64, value: u64) -> Entry {
        Entry { key: Key { measure: measure.into(), path: path.into(), fingerprint }, value }
    }

    #[test]
    fn a_file_reads_back_as_it_was_written() {
        let items = Items::of([
            entry("function_weight", "a::f", 1, 40),
            entry("function_weight", "a::g", 2, 31),
            entry("fields", "a::S", 3, 22),
        ]);
        let text = items.render();
        assert_eq!(Items::parse(&text), Ok(items.clone()));
        assert_eq!(Items::parse(&text).map(|it| it.render()), Ok(text));
    }

    #[test]
    fn a_bad_line_is_refused_naming_it() {
        assert!(matches!(Items::parse("function_weight\ta::f\t1\t40\n"), Err(ItemsError::Line { line: 1, .. })));
        let twice = "f\ta\t0000000000000001\t3\nf\ta\t0000000000000001\t4\n";
        assert!(matches!(Items::parse(twice), Err(ItemsError::Twice { line: 2, .. })));
        let unsorted = "f\tb\t0000000000000001\t3\nf\ta\t0000000000000001\t4\n";
        assert_eq!(Items::parse(unsorted), Err(ItemsError::Order { line: 2 }));
    }

    #[test]
    fn an_item_is_found_by_path_and_print_then_either() {
        let items = Items::of([entry("w", "a::f", 1, 40), entry("w", "a::g", 2, 31)]);
        assert_eq!(items.held("w", "a::f", 1), Some(40));
        assert_eq!(items.held("w", "a::f", 9), Some(40), "an edit keeps the path's line");
        assert_eq!(items.held("w", "a::renamed", 2), Some(31), "a rename keeps the print's line");
        assert_eq!(items.held("w", "a::renamed", 9), None, "a rename and an edit lose it");
        assert_eq!(items.held("other", "a::f", 1), None, "a line is of one measure");
    }

    /// The invariant: a bless never raises a line's value and never adds an item once the file
    /// has lines.
    #[test]
    fn a_bless_only_lowers_and_never_adds() {
        let old = Items::of([entry("w", "a::f", 1, 40), entry("w", "a::g", 2, 31), entry("w", "a::h", 3, 20)]);
        let now = Items::of([
            entry("w", "a::f", 1, 45), // grew: held at 40
            entry("w", "a::g", 2, 25), // shrank: lowered to 25
            entry("w", "a::new", 7, 50), // new: not added
            entry("w", "a::moved", 3, 18), // a::h renamed: kept under its new path
        ]);
        let blessed = old.blessed(&now);
        let lines: Vec<(String, u64)> = blessed.entries().map(|it| (it.key.path, it.value)).collect();
        assert_eq!(
            lines,
            [("a::f".to_owned(), 40), ("a::g".to_owned(), 25), ("a::moved".to_owned(), 18)]
        );
        assert_eq!(Items::default().blessed(&now), now, "the first bless takes the run");
    }

    #[test]
    fn no_bless_raises_a_value() {
        blaze_sim::check(blaze_sim::Config::DEFAULT, |tape| {
            let draw = |tape: &blaze_sim::Tape| {
                let rows: Vec<(u64, u64, u64)> = tape.vec(0..=12, "rows", |tape| {
                    (
                        tape.int_in(0..=5u64, "path"),
                        tape.int_in(0..=5u64, "print"),
                        tape.int_in(0..=99u64, "value"),
                    )
                });
                Items::of(rows.into_iter().map(|(path, print, value)| entry("w", &format!("p{path}"), print, value)))
            };
            let old = draw(tape);
            let now = draw(tape);
            let blessed = old.blessed(&now);
            if old.is_empty() {
                return;
            }
            for line in blessed.entries() {
                let was = old.held(&line.key.measure, &line.key.path, line.key.fingerprint);
                assert!(was.is_some_and(|was| line.value <= was), "{line:?} over the old file's");
            }
            assert!(blessed.off(&blessed).is_empty(), "a blessed file is not off itself");
        });
    }

    #[test]
    fn the_gate_says_what_grew_and_what_is_stale() {
        let old = Items::of([entry("w", "a::f", 1, 40), entry("w", "a::g", 2, 31)]);
        let now = Items::of([entry("w", "a::f", 1, 41), entry("w", "a::n", 5, 30)]);
        let off = old.off(&now);
        assert_eq!(
            off,
            [
                Off::Grew { key: entry("w", "a::f", 1, 0).key, was: 40, now: 41 },
                Off::New { key: entry("w", "a::n", 5, 0).key, now: 30 },
                Off::Gone { key: entry("w", "a::g", 2, 0).key, was: 31 },
            ]
        );
        assert_eq!(off.iter().filter(|it| it.over()).count(), 2);
    }

    #[test]
    fn a_note_says_its_line() {
        let it = entry("function_weight", "a::f", 0xabc, 40);
        assert_eq!(from_note(&note(&it.key, it.value)), Some(it));
        assert_eq!(from_note("something else"), None);
    }

    #[test]
    fn the_worst_are_by_value_over_bound() {
        // Over their bounds: `h` 50/15 (3.33), `g` 300/100 (3), `f` 30/15 (2).
        let items = Items::of([entry("w", "a::f", 1, 30), entry("l", "a::g", 2, 300), entry("w", "a::h", 3, 50)]);
        let bound = |measure: &str| match measure {
            "w" => Some(15),
            "l" => Some(100),
            _ => None,
        };
        let worst: Vec<String> = items.worst(2, bound).into_iter().map(|it| it.key.path).collect();
        assert_eq!(worst, ["a::h", "a::g"]);
        // A tie (`h` at 45/15, 3, with `g`) goes by key: measure `l` before `w`.
        let tied = Items::of([entry("l", "a::g", 2, 300), entry("w", "a::h", 3, 45)]);
        let worst: Vec<String> = tied.worst(2, bound).into_iter().map(|it| it.key.path).collect();
        assert_eq!(worst, ["a::g", "a::h"]);
    }
}
