//! Port of Python's `unicodedata` module for General Category queries,
//! and of the simple case mappings CPython's `re` matches by, for every
//! Unicode version the engine has tables for (see [`UnicodeVersion`]).
//!
//! Backed by text data files generated from the UCD's `UnicodeData.txt`
//! and `SpecialCasing.txt` (the latest version's vendored in
//! `src/unicodedata/`). The files are embedded with `include_str!` and
//! parsed on first use into `Vec`s kept for the program's lifetime,
//! keeping `cargo build` from having to parse ~6000 tuple literals per
//! compile.
//!
//! - `categories.txt`: contiguous category runs covering every codepoint in
//!   `0..=0x10FFFF` in the latest version. Codepoints not listed in
//!   UnicodeData.txt are reported as `Cn` (Unassigned), matching Python.
//! - `nfd_bases.txt`: recursive NFD base for each canonically-decomposable
//!   codepoint. Not versioned: canonical decompositions never change once
//!   assigned, and the shrinker, their only user, takes them as a
//!   preference.
//! - `case.txt`: the lowercase and uppercase form of each codepoint that
//!   has one other than itself in the latest version, as CPython's
//!   `_PyUnicode_ToLowercase` and `_PyUnicode_ToUppercase` map it (see
//!   [`UnicodeVersion::to_lower`]).
//! - `delta-<version>.txt`: for each older version, the categories and
//!   case mappings where it differs from the next newer version, from
//!   which its tables are built.
//!
//! To refresh the data, replace the two vendored UCD files, set the
//! versions in `scripts/generate_unicodedata_tables.py` and run it (it
//! downloads the older versions' UCD files), then regenerate the test
//! reference with `scripts/generate_unicodedata_python_reference.py`
//! (`just unicodedata` does both). The version range named in the
//! `hegel_string_generator_text` and `hegel_string_generator_regex` docs
//! in `lib.rs` is written by hand; update it too.

#[cfg(test)]
use crate::control::{InternalError, hegel_internal_assert};
use crate::native::core::EngineError;
use crate::sys::sync::Lazy;
use alloc::boxed::Box;
use alloc::format;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};
use once_cell::race::OnceBox;

/// Unicode General Category.
///
/// Names match the two-character abbreviations returned by Python's
/// `unicodedata.category(c)`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[allow(non_camel_case_types)]
pub enum Category {
    Lu,
    Ll,
    Lt,
    Lm,
    Lo,
    Mn,
    Mc,
    Me,
    Nd,
    Nl,
    No,
    Pc,
    Pd,
    Ps,
    Pe,
    Pi,
    Pf,
    Po,
    Sm,
    Sc,
    Sk,
    So,
    Zs,
    Zl,
    Zp,
    Cc,
    Cf,
    Cs,
    Co,
    Cn,
}

impl Category {
    /// Two-character category code, e.g. `"Lu"` or `"Cn"`. Matches Python's
    /// `unicodedata.category` string.
    pub fn as_str(self) -> &'static str {
        match self {
            Category::Lu => "Lu",
            Category::Ll => "Ll",
            Category::Lt => "Lt",
            Category::Lm => "Lm",
            Category::Lo => "Lo",
            Category::Mn => "Mn",
            Category::Mc => "Mc",
            Category::Me => "Me",
            Category::Nd => "Nd",
            Category::Nl => "Nl",
            Category::No => "No",
            Category::Pc => "Pc",
            Category::Pd => "Pd",
            Category::Ps => "Ps",
            Category::Pe => "Pe",
            Category::Pi => "Pi",
            Category::Pf => "Pf",
            Category::Po => "Po",
            Category::Sm => "Sm",
            Category::Sc => "Sc",
            Category::Sk => "Sk",
            Category::So => "So",
            Category::Zs => "Zs",
            Category::Zl => "Zl",
            Category::Zp => "Zp",
            Category::Cc => "Cc",
            Category::Cf => "Cf",
            Category::Cs => "Cs",
            Category::Co => "Co",
            Category::Cn => "Cn",
        }
    }

    /// Inverse of [`as_str`]. Returns `None` for unknown codes.
    fn from_code(s: &str) -> Option<Category> {
        Some(match s {
            "Lu" => Category::Lu,
            "Ll" => Category::Ll,
            "Lt" => Category::Lt,
            "Lm" => Category::Lm,
            "Lo" => Category::Lo,
            "Mn" => Category::Mn,
            "Mc" => Category::Mc,
            "Me" => Category::Me,
            "Nd" => Category::Nd,
            "Nl" => Category::Nl,
            "No" => Category::No,
            "Pc" => Category::Pc,
            "Pd" => Category::Pd,
            "Ps" => Category::Ps,
            "Pe" => Category::Pe,
            "Pi" => Category::Pi,
            "Pf" => Category::Pf,
            "Po" => Category::Po,
            "Sm" => Category::Sm,
            "Sc" => Category::Sc,
            "Sk" => Category::Sk,
            "So" => Category::So,
            "Zs" => Category::Zs,
            "Zl" => Category::Zl,
            "Zp" => Category::Zp,
            "Cc" => Category::Cc,
            "Cf" => Category::Cf,
            "Cs" => Category::Cs,
            "Co" => Category::Co,
            "Cn" => Category::Cn,
            _ => return None,
        })
    }

    /// Whether the category is in the category group `group`: a full
    /// two-character code (`"Lu"`, `"Cn"`, ...) matching it exactly, or a
    /// single-letter major class (`"L"`, `"N"`, ...) matching every
    /// category with that prefix. Mirrors Hypothesis's
    /// `as_general_categories` prefix semantics (see
    /// `hypothesis/internal/charmap.py`). Unknown groups match nothing.
    pub fn in_group(self, group: &str) -> bool {
        let cat = self.as_str();
        match group.len() {
            2 => cat == group,
            1 => cat.starts_with(group) && MAJOR_CLASSES.contains(&group),
            _ => false,
        }
    }
}

const MAJOR_CLASSES: &[&str] = &["L", "M", "N", "P", "S", "Z", "C"];

/// A Unicode version the engine has General Category and case tables
/// for.
///
/// Every host language classifies characters by its own Unicode version,
/// so a string generator takes the host's version and draws characters as
/// that version classifies them: a character the host does not know yet is
/// unassigned to it, in no category and no `\w`, and a character the host
/// cases differently folds as the host folds it. The versions are listed
/// newest first in [`VERSIONS`]: the latest has the full tables, and each
/// older one is built from the next newer one by its delta.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct UnicodeVersion(u8);

struct VersionData {
    name: &'static str,
    delta: &'static str,
}

const VERSIONS: [VersionData; 5] = [
    VersionData {
        name: "17.0.0",
        delta: "",
    },
    VersionData {
        name: "16.0.0",
        delta: include_str!("delta-16.0.0.txt"),
    },
    VersionData {
        name: "15.1.0",
        delta: include_str!("delta-15.1.0.txt"),
    },
    VersionData {
        name: "15.0.0",
        delta: include_str!("delta-15.0.0.txt"),
    },
    VersionData {
        name: "14.0.0",
        delta: include_str!("delta-14.0.0.txt"),
    },
];

/// The `(major, minor, update)` of a version number like `"15.1.0"`;
/// `"15.1"` and `"15"` count as `15.1.0` and `15.0.0`.
fn version_number(s: &str) -> Option<(u32, u32, u32)> {
    let mut parts = s.split('.').map(|part| part.parse::<u32>().ok());
    let major = parts.next().flatten()?;
    let minor = parts.next().unwrap_or(Some(0))?;
    let update = parts.next().unwrap_or(Some(0))?;
    parts.next().is_none().then_some((major, minor, update))
}

static HOST: AtomicU32 = AtomicU32::new(0);

impl UnicodeVersion {
    /// The newest version the engine has tables for.
    pub const LATEST: UnicodeVersion = UnicodeVersion(0);

    /// The version the string generators follow: the host's, set with
    /// [`set_host`](Self::set_host) (`hegel_set_unicode_version`), or
    /// [`LATEST`](Self::LATEST) until then. Read when a generator is
    /// built, so a change does not reach generators that already exist.
    pub fn host() -> UnicodeVersion {
        UnicodeVersion::from_index(HOST.load(Ordering::Relaxed))
    }

    /// Make `version` the one [`host`](Self::host) returns, for the
    /// whole process.
    pub fn set_host(version: UnicodeVersion) {
        HOST.store(version.index(), Ordering::Relaxed);
    }

    /// Every version, newest first.
    #[cfg(test)]
    pub fn all() -> impl Iterator<Item = UnicodeVersion> {
        (0..VERSIONS.len()).map(|i| UnicodeVersion(i as u8))
    }

    /// The version named `s`, a version number like Python's
    /// `unicodedata.unidata_version` (`"15.1.0"`). A version newer than
    /// [`LATEST`](Self::LATEST) is taken as the latest, the nearest the
    /// engine can come to it; a version it has no tables for is an
    /// invalid argument.
    pub fn parse(s: &str) -> Result<UnicodeVersion, EngineError> {
        let number = version_number(s).ok_or_else(|| {
            EngineError::InvalidArgument(format!(
                "unicode_version {s:?} is not a Unicode version number like \"15.1.0\""
            ))
        })?;
        if let Some(index) = VERSIONS
            .iter()
            .position(|version| version_number(version.name) == Some(number))
        {
            return Ok(UnicodeVersion(index as u8));
        }
        if number > version_number(Self::LATEST.as_str()).unwrap_or_default() {
            return Ok(Self::LATEST);
        }
        let known = VERSIONS
            .iter()
            .rev()
            .map(|version| version.name)
            .collect::<Vec<_>>()
            .join(", ");
        Err(EngineError::InvalidArgument(format!(
            "libhegel has no tables for Unicode {s}: it supports {known} and takes newer versions as {}",
            Self::LATEST.as_str()
        )))
    }

    /// The version number, as given to [`parse`](Self::parse).
    pub fn as_str(self) -> &'static str {
        VERSIONS[usize::from(self.0)].name
    }

    /// How many versions older than the latest this one is: `0` for the
    /// latest. Round-trips through [`from_index`](Self::from_index).
    pub fn index(self) -> u32 {
        u32::from(self.0)
    }

    /// The version [`index`](Self::index) versions older than the latest,
    /// or the oldest when there is none that old.
    pub fn from_index(index: u32) -> UnicodeVersion {
        UnicodeVersion(index.min(VERSIONS.len() as u32 - 1) as u8)
    }

    fn newer(self) -> Option<UnicodeVersion> {
        self.0.checked_sub(1).map(UnicodeVersion)
    }

    fn tables(self) -> &'static Tables {
        static TABLES: PerVersion<Tables> = PerVersion::new();
        TABLES.get(self, || match self.newer() {
            None => Tables::latest(),
            Some(newer) => newer
                .tables()
                .with_delta(VERSIONS[usize::from(self.0)].delta),
        })
    }

    /// The `(codepoint, lowercase, uppercase)` of every codepoint with a
    /// lowercase or uppercase form other than itself, sorted by codepoint.
    pub fn case_mappings(self) -> &'static [(u32, u32, u32)] {
        &self.tables().case
    }

    fn case_mapping(self, c: char) -> Option<(u32, u32)> {
        let mappings = self.case_mappings();
        let i = mappings
            .binary_search_by_key(&(c as u32), |&(cp, _, _)| cp)
            .ok()?;
        let (_, lower, upper) = mappings[i];
        Some((lower, upper))
    }

    /// The lowercase form of `c`, or `c` itself when it has none:
    /// CPython's `_PyUnicode_ToLowercase`, which `re.IGNORECASE` compares
    /// characters by. It is the simple lowercase mapping, except for a
    /// character whose full lowercase mapping is special-cased, which maps
    /// to that mapping's first codepoint (U+0130 to `i`).
    pub fn to_lower(self, c: char) -> char {
        self.case_mapping(c)
            .map_or(c, |(lower, _)| char_of(lower, c))
    }

    /// The uppercase form of `c`, or `c` itself when it has none:
    /// CPython's `_PyUnicode_ToUppercase`, the first codepoint of a
    /// special-cased full mapping as [`to_lower`](Self::to_lower) takes it
    /// (`ß` to `S`).
    pub fn to_upper(self, c: char) -> char {
        self.case_mapping(c)
            .map_or(c, |(_, upper)| char_of(upper, c))
    }

    /// The General Category table as `(start, end, category)` runs of
    /// equal category, in increasing order and together covering every
    /// codepoint in `0..=0x10FFFF`.
    pub fn category_runs(self) -> impl Iterator<Item = (u32, u32, Category)> {
        let mut start = 0;
        self.tables().ranges.iter().map(move |&(end, cat)| {
            let run = (start, end, cat);
            start = end + 1;
            run
        })
    }

    /// The runs of [`category_runs`](Self::category_runs) restricted to
    /// the codepoints `lo..=hi`: the first and last runs are clipped to
    /// that range, codepoints above `0x10FFFF` are left out, and an empty
    /// range yields nothing. Walking these answers a category question
    /// about every codepoint of a range without a table lookup per
    /// codepoint.
    pub fn category_runs_in(self, lo: u32, hi: u32) -> impl Iterator<Item = (u32, u32, Category)> {
        let table = &self.tables().ranges;
        let hi = hi.min(0x10FFFF);
        let first = if lo > hi {
            table.len()
        } else {
            table.partition_point(|&(end, _)| end < lo)
        };
        let mut start = if first == 0 {
            0
        } else {
            table[first - 1].0 + 1
        };
        table[first..].iter().map_while(move |&(end, cat)| {
            let run_start = start;
            start = end + 1;
            (run_start <= hi).then(|| (run_start.max(lo), end.min(hi), cat))
        })
    }

    /// Return the Unicode General Category for `cp`.
    ///
    /// Matches Python's `unicodedata.category(chr(cp))` for every
    /// codepoint in `0..=0x10FFFF`. Codepoints outside that range are an
    /// internal error, mirroring Python's `chr(cp)` which rejects `cp >
    /// 0x10FFFF`.
    #[cfg(test)]
    pub fn general_category(self, cp: u32) -> Result<Category, InternalError> {
        hegel_internal_assert!(cp <= 0x10FFFF, "codepoint {:#x} out of range", cp);
        Ok(self.category_lookup(cp))
    }

    /// [`general_category`](Self::general_category) for a `char`, whose
    /// type already guarantees a codepoint in range — so the lookup cannot
    /// fail.
    #[cfg(test)]
    pub fn general_category_char(self, c: char) -> Category {
        self.category_lookup(c as u32)
    }

    /// Table lookup behind [`general_category`](Self::general_category).
    /// Callers must pass `cp <= 0x10FFFF` (the binary search indexes past
    /// the table otherwise).
    #[cfg(test)]
    fn category_lookup(self, cp: u32) -> Category {
        let table = &self.tables().ranges;
        let idx = table
            .binary_search_by(|&(end, _)| {
                if end < cp {
                    core::cmp::Ordering::Less
                } else {
                    core::cmp::Ordering::Greater
                }
            })
            .unwrap_err();
        table[idx].1
    }

    /// Test whether `cp` belongs to the category group `group` (see
    /// [`Category::in_group`]).
    #[cfg(test)]
    pub fn is_in_group(self, cp: u32, group: &str) -> Result<bool, InternalError> {
        Ok(self.general_category(cp)?.in_group(group))
    }
}

/// One value per [`UnicodeVersion`], each computed on first use and kept
/// for the lifetime of the program, like a [`Lazy`] per version.
pub(crate) struct PerVersion<T> {
    cells: [OnceBox<T>; VERSIONS.len()],
}

impl<T> PerVersion<T> {
    pub(crate) const fn new() -> PerVersion<T> {
        PerVersion {
            cells: [const { OnceBox::new() }; VERSIONS.len()],
        }
    }

    /// The value for `version`, computing it with `init` if this is the
    /// first access.
    pub(crate) fn get(&self, version: UnicodeVersion, init: impl FnOnce() -> T) -> &T {
        self.cells[usize::from(version.0)].get_or_init(|| Box::new(init()))
    }
}

/// Embedded raw data; parsed lazily into the [`Tables`] of
/// [`UnicodeVersion::LATEST`] and [`nfd_bases`].
const CATEGORIES_DATA: &str = include_str!("categories.txt");
const NFD_BASES_DATA: &str = include_str!("nfd_bases.txt");
const CASE_DATA: &str = include_str!("case.txt");

/// The General Category and case tables of one Unicode version.
struct Tables {
    /// Run-length-encoded General Category table covering all codepoints
    /// in `0..=0x10FFFF`. Entries are non-overlapping, contiguous, and
    /// sorted by `end`; lookup is binary search for the first entry with
    /// `end >= cp`.
    ranges: Vec<(u32, Category)>,
    /// The `(codepoint, lowercase, uppercase)` of every codepoint with a
    /// lowercase or uppercase form other than itself, sorted by codepoint.
    case: Vec<(u32, u32, u32)>,
}

fn hex(field: &str, file: &str) -> u32 {
    u32::from_str_radix(field, 16).unwrap_or_else(|_| panic!("bad hex in {file}: {field:?}"))
}

fn category(code: &str, file: &str) -> Category {
    Category::from_code(code).unwrap_or_else(|| panic!("unknown category in {file}: {code:?}"))
}

/// The fields of a data line, which must have exactly `N` of them.
fn fields<'a, const N: usize>(line: &'a str, file: &str) -> [&'a str; N] {
    let fields: Vec<&str> = line.split(' ').collect();
    <[&str; N]>::try_from(fields).unwrap_or_else(|_| panic!("malformed {file} line: {line:?}"))
}

/// Whether a delta line is a category run (`C`) rather than a case
/// mapping (`M`).
fn is_category_line(kind: &str, line: &str) -> bool {
    match kind {
        "C" => true,
        "M" => false,
        _ => panic!("malformed delta line: {line:?}"),
    }
}

impl Tables {
    fn latest() -> Tables {
        let ranges = CATEGORIES_DATA
            .lines()
            .map(|line| {
                let [end, cat] = fields(line, "categories.txt");
                (hex(end, "categories.txt"), category(cat, "categories.txt"))
            })
            .collect();
        let case = CASE_DATA
            .lines()
            .map(|line| {
                let [cp, lower, upper] = fields(line, "case.txt");
                (
                    hex(cp, "case.txt"),
                    hex(lower, "case.txt"),
                    hex(upper, "case.txt"),
                )
            })
            .collect();
        Tables { ranges, case }
    }

    /// The tables of the next older version, which `delta` (a
    /// `delta-<version>.txt`) describes as the categories and case
    /// mappings where it differs from these.
    fn with_delta(&self, delta: &str) -> Tables {
        let file = "delta";
        let mut category_runs: Vec<(u32, u32, Category)> = Vec::new();
        let mut case_changes: Vec<(u32, u32, u32)> = Vec::new();
        for line in delta.lines() {
            let [kind, first, second, third] = fields(line, file);
            let (first, second) = (hex(first, file), hex(second, file));
            if is_category_line(kind, line) {
                category_runs.push((first, second, category(third, file)));
            } else {
                case_changes.push((first, second, hex(third, file)));
            }
        }
        Tables {
            ranges: overlay_runs(&self.ranges, &category_runs),
            case: overlay_case(&self.case, &case_changes),
        }
    }
}

/// `base` with the categories of the codepoints in `runs` replaced by the
/// runs' categories, re-run-length-encoded; `runs` is sorted and
/// non-overlapping.
fn overlay_runs(base: &[(u32, Category)], runs: &[(u32, u32, Category)]) -> Vec<(u32, Category)> {
    let mut out: Vec<(u32, Category)> = Vec::with_capacity(base.len() + 2 * runs.len());
    let mut push = |end: u32, cat: Category| match out.last_mut() {
        Some(last) if last.1 == cat => last.0 = end,
        _ => out.push((end, cat)),
    };
    let mut runs = runs.iter().peekable();
    let mut start = 0;
    for &(end, cat) in base {
        let mut pos = start;
        while let Some(&&(run_start, run_end, run_cat)) = runs.peek() {
            if run_start > end {
                break;
            }
            if run_start > pos {
                push(run_start - 1, cat);
            }
            let covered_end = run_end.min(end);
            push(covered_end, run_cat);
            pos = covered_end + 1;
            if run_end > end {
                break;
            }
            runs.next();
        }
        if pos <= end {
            push(end, cat);
        }
        start = end + 1;
    }
    out
}

/// `base` with the mappings of the codepoints in `changes` replaced by the
/// changes' mappings, dropping a codepoint mapped to itself both ways;
/// both are sorted by codepoint.
fn overlay_case(base: &[(u32, u32, u32)], changes: &[(u32, u32, u32)]) -> Vec<(u32, u32, u32)> {
    let mut out = Vec::with_capacity(base.len() + changes.len());
    let mut changes = changes.iter().peekable();
    for &entry in base {
        while let Some(&&change) = changes.peek() {
            if change.0 >= entry.0 {
                break;
            }
            out.push(change);
            changes.next();
        }
        match changes.peek() {
            Some(&&change) if change.0 == entry.0 => {
                out.push(change);
                changes.next();
            }
            _ => out.push(entry),
        }
    }
    out.extend(changes);
    out.retain(|&(cp, lower, upper)| lower != cp || upper != cp);
    out
}

/// Recursive NFD base for codepoints that have a canonical decomposition,
/// sorted by codepoint. Codepoints not in this table either have no
/// canonical decomposition or are already their own base.
fn nfd_bases() -> &'static [(u32, u32)] {
    static BASES: Lazy<Vec<(u32, u32)>> = Lazy::new(|| {
        NFD_BASES_DATA
            .lines()
            .map(|line| {
                let [cp, base] = fields(line, "nfd_bases.txt");
                (hex(cp, "nfd_bases.txt"), hex(base, "nfd_bases.txt"))
            })
            .collect()
    });
    &BASES
}

fn char_of(cp: u32, c: char) -> char {
    char::from_u32(cp).unwrap_or(c)
}

/// Return the recursive NFD base codepoint for `cp`, if it has a canonical
/// decomposition that resolves to a different starting codepoint.
///
/// For example, `À` (U+00C0) → `A` (U+0041), `Ǻ` (U+01FA) → `A`, but `A`
/// itself returns `None`. The returned codepoint is itself non-decomposable
/// (i.e. applying `nfd_base` to the result yields `None`).
///
/// Codepoints with no canonical decomposition (including emoji, CJK
/// ideographs, and ASCII) return `None`. Compatibility decompositions
/// (NFKD, e.g. `Ⅰ` → `I`) are *not* applied — they're not part of NFD.
pub fn nfd_base(cp: u32) -> Option<u32> {
    let table = nfd_bases();
    table
        .binary_search_by_key(&cp, |&(c, _)| c)
        .ok()
        .map(|idx| table[idx].1)
}

#[cfg(test)]
#[path = "../../tests/embedded/native/unicodedata_tests.rs"]
mod tests;
