//! The lint tables numbered and indexed as blaze is built: every lint with its [`LintId`], a
//! perfect hash of the lints' names and one of the other names of rustc's store (the groups,
//! `rustc::internal`, the ignored, renamed and removed names), the first group of each builtin
//! lint, and the tools the lints are of. Nothing here is built at run time, and a table that
//! names a key twice fails to compile.
//!
//! Cost: finding a name is one `blaze_phf` lookup (one hash of its bytes, two dependent loads,
//! one compare); a lint by id, its first group and a store entry by index are one load each.
//! The two perfect hashes take about 4 KiB together.

use edition::Edition;

use super::{Level, Lint, LintId, lints, names};

/// The lint tables in the order of [`LintId`]s: that order is rustc's registration order, and
/// is what the groups, the notes and the order of emission see.
const TABLES: [&[Lint]; 4] = [lints::BUILTIN, lints::BLAZE, lints::INTERNAL, lints::UNREGISTERED];

pub(super) const BUILTIN_END: usize = lints::BUILTIN.len();
pub(super) const BLAZE_END: usize = BUILTIN_END + lints::BLAZE.len();
pub(super) const INTERNAL_END: usize = BLAZE_END + lints::INTERNAL.len();
pub(super) const LINT_COUNT: usize = INTERNAL_END + lints::UNREGISTERED.len();

const _: () = assert!(LINT_COUNT < u16::MAX as usize, "a `LintId` is a `u16` below `u16::MAX`");

/// Every lint, by [`LintId`], each carrying its id.
pub(super) static ALL: [Lint; LINT_COUNT] = {
    let mut out = [Lint::new("", Level::Allow, ""); LINT_COUNT];
    let mut at = 0;
    let mut table = 0;
    while table < TABLES.len() {
        let mut i = 0;
        while i < TABLES[table].len() {
            let mut lint = TABLES[table][i];
            lint.id = LintId(at as u16);
            out[at] = lint;
            i += 1;
            at += 1;
        }
        table += 1;
    }
    out
};

/// [`ALL`] as references: the one-lint slices [`super::Checked`] and
/// [`super::Store::find_lints`] give point into it.
pub(super) static REFS: [&Lint; LINT_COUNT] = {
    let mut out = [&ALL[0]; LINT_COUNT];
    let mut at = 0;
    while at < LINT_COUNT {
        out[at] = &ALL[at];
        at += 1;
    }
    out
};

/// Every lint's name, by [`LintId`].
const LINT_NAMES: [&str; LINT_COUNT] = {
    let mut out = [""; LINT_COUNT];
    let mut at = 0;
    let mut table = 0;
    while table < TABLES.len() {
        let mut i = 0;
        while i < TABLES[table].len() {
            out[at] = TABLES[table][i].name;
            i += 1;
            at += 1;
        }
        table += 1;
    }
    out
};

pub(super) const LINT_KEYS: &[&str] = &LINT_NAMES;

blaze_phf::phf_table!(
    /// Every lint's name to its [`LintId`]: the index of a key is the id.
    pub(super) static LINTS_BY_NAME = LINT_KEYS;
);

/// What a name of rustc's store that is not a lint's stands for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Entry {
    /// The group of this index in [`super::groups`].
    Group(u16),
    /// `rustc::internal`, a group under `-Z unstable-options` only.
    InternalGroup,
    /// A rustdoc lint of old, which rustc takes and does nothing with.
    Ignored,
    /// Renamed: the new name.
    Renamed(&'static str),
    /// Removed: why.
    Removed(&'static str),
}

/// The name of the internal group.
pub(super) const INTERNAL_GROUP: &str = "rustc::internal";

/// The groups rustc makes of the future-incompatible lints: each edition's, then
/// `future_incompatible`, as [`fcw_group`] numbers them.
const FCW_GROUPS: [&str; 5] = [
    super::edition_group(Edition::Edition2015),
    super::edition_group(Edition::Edition2018),
    super::edition_group(Edition::Edition2021),
    super::edition_group(Edition::Edition2024),
    "future_incompatible",
];

/// The future-incompatible group `lint` is of, as an index of [`FCW_GROUPS`]: its edition's
/// compatibility group, or `future_incompatible`.
pub(super) const fn fcw_group(lint: &Lint) -> Option<usize> {
    match lint.future_incompatible {
        Some(future) => match future.reason.edition() {
            Some(edition) => Some(edition as usize),
            None => Some(FCW_GROUPS.len() - 1),
        },
        None => None,
    }
}

/// The future-incompatible groups in the order rustc adds them (as it registers the first
/// builtin lint of each), as indices of [`FCW_GROUPS`], and how many there are.
const FCW_ORDER: ([u8; FCW_GROUPS.len()], usize) = {
    let mut order = [0u8; FCW_GROUPS.len()];
    let mut seen = [false; FCW_GROUPS.len()];
    let mut count = 0;
    let mut at = 0;
    while at < lints::BUILTIN.len() {
        if let Some(group) = fcw_group(&lints::BUILTIN[at])
            && !seen[group]
        {
            seen[group] = true;
            order[count] = group as u8;
            count += 1;
        }
        at += 1;
    }
    (order, count)
};

static FCW_ORDERED: [u8; FCW_GROUPS.len()] = FCW_ORDER.0;

/// The future-incompatible groups, in rustc's order (those of [`super::groups`] before
/// `add_lint_group!`'s): each name and its index for [`fcw_group`].
pub(super) fn fcw_groups() -> impl Iterator<Item = (&'static str, usize)> {
    FCW_ORDERED[..FCW_ORDER.1].iter().map(|&it| (FCW_GROUPS[usize::from(it)], usize::from(it)))
}

/// The groups: the future-incompatible ones, `add_lint_group!`'s, the aliases, then blaze's.
pub(super) const GROUP_COUNT: usize =
    FCW_ORDER.1 + names::GROUPS.len() + names::ALIASES.len() + names::BLAZE_GROUPS.len();

const _: () = assert!(GROUP_COUNT < u16::MAX as usize);

const OTHER_COUNT: usize =
    GROUP_COUNT + 1 + names::IGNORED.len() + names::RENAMED.len() + names::REMOVED.len();

/// The names of the store that are not lints', and what each stands for: the groups (a
/// group's index among them is its index in [`super::groups`]), `rustc::internal`, the
/// ignored, renamed and removed names. No name is two of these, which the perfect hash checks:
/// rustc's store has one entry a name.
const OTHERS: ([&str; OTHER_COUNT], [Entry; OTHER_COUNT]) = {
    let mut keys = [""; OTHER_COUNT];
    let mut entries = [Entry::Ignored; OTHER_COUNT];
    let mut at = 0;
    let mut i = 0;
    while i < FCW_ORDER.1 {
        keys[at] = FCW_GROUPS[FCW_ORDER.0[i] as usize];
        entries[at] = Entry::Group(at as u16);
        at += 1;
        i += 1;
    }
    i = 0;
    while i < names::GROUPS.len() {
        keys[at] = names::GROUPS[i].0;
        entries[at] = Entry::Group(at as u16);
        at += 1;
        i += 1;
    }
    i = 0;
    while i < names::ALIASES.len() {
        keys[at] = names::ALIASES[i].0;
        entries[at] = Entry::Group(at as u16);
        at += 1;
        i += 1;
    }
    i = 0;
    while i < names::BLAZE_GROUPS.len() {
        keys[at] = names::BLAZE_GROUPS[i].0;
        entries[at] = Entry::Group(at as u16);
        at += 1;
        i += 1;
    }
    keys[at] = INTERNAL_GROUP;
    entries[at] = Entry::InternalGroup;
    at += 1;
    i = 0;
    while i < names::IGNORED.len() {
        keys[at] = names::IGNORED[i];
        entries[at] = Entry::Ignored;
        at += 1;
        i += 1;
    }
    i = 0;
    while i < names::RENAMED.len() {
        keys[at] = names::RENAMED[i].0;
        entries[at] = Entry::Renamed(names::RENAMED[i].1);
        at += 1;
        i += 1;
    }
    i = 0;
    while i < names::REMOVED.len() {
        keys[at] = names::REMOVED[i].0;
        entries[at] = Entry::Removed(names::REMOVED[i].1);
        at += 1;
        i += 1;
    }
    assert!(at == OTHER_COUNT);
    (keys, entries)
};

pub(super) const OTHER_KEYS: &[&str] = &OTHERS.0;

/// What each of [`OTHER_KEYS`] stands for, by its index.
pub(super) static OTHER_ENTRIES: [Entry; OTHER_COUNT] = OTHERS.1;

blaze_phf::phf_table!(
    /// The names of [`OTHER_KEYS`] to their index.
    pub(super) static OTHERS_BY_NAME = OTHER_KEYS;
);

/// What `name` stands for in the store if it is no lint's.
#[inline]
pub(super) fn other(name: &[u8]) -> Option<Entry> {
    OTHERS_BY_NAME.index(name).map(|at| OTHER_ENTRIES[at])
}

/// The longest name of either table: a longer one is none of them.
pub(super) const LONGEST: usize = {
    let mut longest = 0;
    let mut at = 0;
    while at < LINT_KEYS.len() {
        assert!(!LINT_KEYS[at].is_empty(), "no lint has an empty name");
        if LINT_KEYS[at].len() > longest {
            longest = LINT_KEYS[at].len();
        }
        at += 1;
    }
    at = 0;
    while at < OTHER_KEYS.len() {
        assert!(!OTHER_KEYS[at].is_empty(), "no name of the store is empty");
        if OTHER_KEYS[at].len() > longest {
            longest = OTHER_KEYS[at].len();
        }
        at += 1;
    }
    longest
};

/// `tool::name` on the stack, for the lookups of a tool's lint: every name of the tables fits.
pub(super) struct Joined {
    bytes: [u8; LONGEST],
    /// Zero when `tool::name` is longer than any name of the tables: no key is empty, so the
    /// empty name finds nothing, as the long one would.
    len: usize,
}

impl Joined {
    pub(super) fn new(tool: &str, name: &str) -> Joined {
        let len = tool.len() + 2 + name.len();
        let mut joined = Joined { bytes: [0; LONGEST], len: 0 };
        if len <= LONGEST {
            joined.bytes[..tool.len()].copy_from_slice(tool.as_bytes());
            joined.bytes[tool.len()..tool.len() + 2].copy_from_slice(b"::");
            joined.bytes[tool.len() + 2..len].copy_from_slice(name.as_bytes());
            joined.len = len;
        }
        joined
    }

    pub(super) fn bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

/// The index of the lint named `name` among the first `end` of [`LINT_NAMES`], by a scan: for
/// the tables built here and the named [`LintId`]s, at compile time.
pub(super) const fn scan(name: &str, end: usize) -> Option<usize> {
    let mut at = 0;
    while at < end {
        if blaze_phf::bytes_eq(LINT_NAMES[at].as_bytes(), name.as_bytes()) {
            return Some(at);
        }
        at += 1;
    }
    None
}

/// No group, in [`FIRST_GROUP`].
pub(super) const NO_GROUP: u16 = u16::MAX;

/// For each builtin lint, by [`LintId`]: the first group that has it, aliases left out, as its
/// index among the groups, or [`NO_GROUP`]. A group naming no builtin lint fails to compile.
pub(super) static FIRST_GROUP: [u16; BUILTIN_END] = {
    let mut first = [NO_GROUP; BUILTIN_END];
    let mut i = 0;
    while i < FCW_ORDER.1 {
        let kind = FCW_ORDER.0[i] as usize;
        let mut at = 0;
        while at < BUILTIN_END {
            if matches!(fcw_group(&lints::BUILTIN[at]), Some(it) if it == kind) && first[at] == NO_GROUP {
                first[at] = i as u16;
            }
            at += 1;
        }
        i += 1;
    }
    let mut group = 0;
    while group < names::GROUPS.len() {
        let members = names::GROUPS[group].1;
        let mut m = 0;
        while m < members.len() {
            let Some(at) = scan(members[m], BUILTIN_END) else {
                panic!("a group of `names::GROUPS` names no builtin lint");
            };
            if first[at] == NO_GROUP {
                first[at] = (FCW_ORDER.1 + group) as u16;
            }
            m += 1;
        }
        group += 1;
    }
    first
};

/// The length of the tool of `name`, `tool::`, if it is a tool's.
const fn tool_len(name: &str) -> Option<usize> {
    let bytes = name.as_bytes();
    let mut at = 0;
    while at + 1 < bytes.len() {
        if bytes[at] == b':' && bytes[at + 1] == b':' {
            return Some(at);
        }
        at += 1;
    }
    None
}

const MOST_TOOLS: usize = 8;

/// The tools of the registered lints' names, `tool::`, each once: the [`LintId`] of the first
/// lint of it (the builtin and blaze's before the internal ones) and the length of its name;
/// then how many there are.
const TOOLS: ([(u16, u8); MOST_TOOLS], usize) = {
    let mut tools = [(0u16, 0u8); MOST_TOOLS];
    let mut count = 0;
    let mut at = 0;
    while at < INTERNAL_END {
        if let Some(len) = tool_len(LINT_NAMES[at]) {
            let (name, _) = LINT_NAMES[at].as_bytes().split_at(len);
            let mut known = false;
            let mut t = 0;
            while t < count {
                let (first, first_len) = tools[t];
                let (other, _) = LINT_NAMES[first as usize].as_bytes().split_at(first_len as usize);
                known = known || blaze_phf::bytes_eq(name, other);
                t += 1;
            }
            if !known {
                assert!(count < MOST_TOOLS && len <= u8::MAX as usize);
                tools[count] = (at as u16, len as u8);
                count += 1;
            }
        }
        at += 1;
    }
    (tools, count)
};

static TOOL_LIST: [(u16, u8); MOST_TOOLS] = TOOLS.0;

/// Whether some lint of the builtin and blaze's tables, or with `internal` of the internal
/// one, is `tool`'s.
pub(super) fn has_lints_of(tool: &str, internal: bool) -> bool {
    TOOL_LIST[..TOOLS.1].iter().any(|&(first, len)| {
        let first = usize::from(first);
        (first < BLAZE_END || internal) && ALL[first].name.as_bytes()[..usize::from(len)] == *tool.as_bytes()
    })
}

/// The tools of lint names every store has: `rustc`, `rustdoc`, `blaze` and those of
/// [`super::PREDEFINED_TOOLS`].
const KNOWN_TOOL_NAMES: [&str; 3 + super::PREDEFINED_TOOLS.len()] = {
    let mut tools = [""; 3 + super::PREDEFINED_TOOLS.len()];
    tools[0] = "rustc";
    tools[1] = "rustdoc";
    tools[2] = super::BLAZE_TOOL;
    let mut at = 0;
    while at < super::PREDEFINED_TOOLS.len() {
        tools[3 + at] = super::PREDEFINED_TOOLS[at];
        at += 1;
    }
    tools
};

const KNOWN_TOOL_KEYS: &[&str] = &KNOWN_TOOL_NAMES;

blaze_phf::phf_table!(
    /// [`KNOWN_TOOL_NAMES`].
    pub(super) static KNOWN_TOOLS = KNOWN_TOOL_KEYS;
);
