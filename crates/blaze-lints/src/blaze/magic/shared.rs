//! The strings two crates must agree on: a writer in one and a reader in another (an
//! environment variable's name, a map, config or JSON key, a file name or path component, a
//! name matched or compared against). A crate's own findings depend only on it and its
//! dependencies (`magic_string`'s "already names this" against a dependency's `const`); what
//! sibling crates (neither depending on the other) share is the workspace pass's, from two
//! pieces here:
//!
//! - [`string_rows`]: each crate's key-like strings as sorted rows, a tracked query, so an edit
//!   of one crate changes one crate's rows;
//! - [`merge_strings`]: a pure k-way merge of the crates' sorted rows into the strings two or
//!   more crates write ([`SharedString::Same`]) and the spellings two crates write that are one
//!   name spelled twice ([`SharedString::NearMiss`]: `"build_id"` written, `"buildId"` read).
//!
//! Only the use classes `magic_string` already computes are rows ([`UseClass`]): never prose,
//! never a message, never a plain argument (a gap: a span's or a counter's name passed to a
//! function is an argument, not a lookup).

use std::alloc::Global;

use base_db::Db;
use blaze_hash::HashMap;
use hir_def::DefWithBodyId;
use intern::Symbol;
use syntax::TextRange;

use super::{NEAR_LEN, Value, const_value, literals, near, one_edit};
use crate::{
    blaze::{dup::index::is_test_target, test_code},
    query,
};

/// What a key-like string is used as.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum UseClass {
    /// The value of a `const`: the string's home in its crate.
    Const,
    /// An operand of `==` with a value.
    Compared,
    /// A pattern a `match` matches a value against.
    Matched,
    /// A key a map, a set or an index is looked up or filled by.
    Key,
    /// An environment variable's name.
    Env,
    /// A file name or a path component.
    Path,
}

/// Which side of a name a use is on, where the call says.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum Access {
    /// Fills or sets what the name names (`insert`, `set_var`, `fs::write`).
    Writes,
    /// Looks it up (`get`, `var`, `fs::read`, a compare, a match).
    Reads,
    /// Neither said: the name itself (a `const`, `Path::join`).
    Names,
}

/// One key-like string of a crate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StringRow {
    /// [`normalise`]d: what spellings of one name share.
    pub(crate) norm: Symbol,
    /// As written, trimmed.
    pub(crate) exact: Symbol,
    pub(crate) class: UseClass,
    pub(crate) access: Access,
    /// The path of the item it is in (`blaze_store::pool::open`).
    pub(crate) item: Symbol,
    pub(crate) file: span::FileId,
    /// In the file.
    pub(crate) range: TextRange,
}

impl StringRow {
    /// The order every row list is in: by normalised spelling, then exact, then place.
    fn order(&self) -> (&str, &str, u32, u32, &str) {
        (self.norm.as_str(), self.exact.as_str(), self.file.index(), self.range.start().into(), self.item.as_str())
    }
}

/// Whether `text` is for a person, not a name: empty, no letter or digit (punctuation the code
/// writes, `", "`), or holding whitespace (a sentence). The one test `magic_string` and the
/// merge both ask.
pub(crate) fn is_prose(text: &str) -> bool {
    text.is_empty() || !text.chars().any(char::is_alphanumeric) || text.chars().any(char::is_whitespace)
}

/// The spelling the spellings of one name share: ASCII letters in lower case, `-` and `_`
/// dropped, and one trailing `s` dropped from what stays [`PLURAL_FLOOR`] long or more:
/// `"build_ids"`, `"buildId"` and `"Build-ID"` are `"buildid"`.
pub(crate) fn normalise(text: &str) -> Symbol {
    let mut out: Vec<u8, Global> = Vec::with_capacity_in(text.len(), Global);
    // Only ASCII bytes are changed or dropped, so what stays is UTF-8.
    out.extend(text.bytes().filter(|it| !matches!(it, b'-' | b'_')).map(|it| it.to_ascii_lowercase()));
    if out.len() > PLURAL_FLOOR && out.last() == Some(&b's') {
        out.pop();
    }
    Symbol::intern(core::str::from_utf8(&out).unwrap_or(text))
}

/// The shortest spelling a trailing `s` is taken as a plural of: `"bus"` and `"gas"` keep it.
const PLURAL_FLOOR: usize = 3;

/// The key-like strings of `krate`, sorted ([`StringRow::order`]): the strings of its bodies
/// that [`literals`] classes, the strings its `match`es match against, and its string
/// `const`s. Empty for a test, bench or example target.
#[blaze_query::tracked(returns(ref))]
pub(crate) fn string_rows(db: &Db, krate: base_db::Crate) -> Box<[StringRow]> {
    let mut out: Vec<StringRow, Global> = Vec::new_in(Global);
    if is_test_target(db, krate) {
        return out.into_boxed_slice();
    }
    for body in query::lint_plan(db, krate).bodies.iter() {
        let def = body.def;
        let file = body.file.file_id(db);
        let item = item_path(db, def);
        let row = |exact: Symbol, class, access, range: TextRange| StringRow {
            norm: normalise(exact.as_str()),
            exact,
            class,
            access,
            item,
            file,
            range,
        };
        if let DefWithBodyId::ConstId(konst) = def
            && !test_code::is_test_def(db, def)
            && let Some(Value::Str(text)) = const_value(db, konst)
            && !is_prose(text.as_str())
        {
            out.push(row(text, UseClass::Const, Access::Names, body.item));
            continue;
        }
        let found = literals(db, def);
        for lit in &found.lits {
            if let (Value::Str(text), Some(class)) = (lit.value, lit.lookup) {
                out.push(row(text, class, lit.access, lit.range + body.offset));
            }
        }
        for matched in &found.matched {
            for &text in &matched.values {
                out.push(row(text, UseClass::Matched, Access::Reads, matched.range + body.offset));
            }
        }
    }
    out.sort_unstable_by(|a, b| a.order().cmp(&b.order()));
    out.into_boxed_slice()
}

/// The path of the item `def` is: its crate's name, its modules and its own name.
fn item_path(db: &Db, def: DefWithBodyId) -> Symbol {
    let body = hir::DefWithBody::from(def);
    let module = body.module(db);
    let mut out: Vec<u8, Global> = Vec::new_in(Global);
    if let Some(name) = module.krate(db).display_name(db) {
        out.extend_from_slice(name.crate_name().symbol().as_str().as_bytes());
    }
    let mut push = |segment: &str| {
        if !out.is_empty() {
            out.extend_from_slice(b"::");
        }
        out.extend_from_slice(segment.as_bytes());
    };
    for segment in module.path_segments(db) {
        push(segment.as_str());
    }
    push(body.name(db).as_ref().map_or("_", |it| it.as_str()));
    Symbol::intern(core::str::from_utf8(&out).unwrap_or("_"))
}

/// One place of a string the merge reports: the crate's name and its row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SharedSite {
    pub(crate) krate: Symbol,
    pub(crate) row: StringRow,
}

/// A string sibling crates share, as the workspace pass reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SharedString {
    /// One exact spelling written in two or more crates: `string_shared_across_crates`.
    Same { spelling: Symbol, sites: Vec<SharedSite, Global> },
    /// Two spellings of one name in two crates that differ only by `how`:
    /// `string_near_miss_across_crates`.
    NearMiss {
        how: &'static str,
        one: Symbol,
        one_at: Vec<SharedSite, Global>,
        other: Symbol,
        other_at: Vec<SharedSite, Global>,
    },
}

impl SharedString {
    /// The lint the workspace pass reports this under.
    pub(crate) fn lint(&self) -> super::Lint {
        match self {
            SharedString::Same { .. } => super::Lint::StringSharedAcrossCrates,
            SharedString::NearMiss { .. } => super::Lint::StringNearMissAcrossCrates,
        }
    }

    /// The finding's message, one sentence that says what to do; a near-miss says which crate
    /// writes and which reads where the uses tell.
    pub(crate) fn message(&self) -> String {
        match self {
            SharedString::Same { spelling, sites } => {
                let crates = crate_list(sites);
                let home = sites
                    .iter()
                    .find(|it| it.row.class == UseClass::Const)
                    .map(|it| format!(", which `{}` already names", it.row.item.as_str()));
                format!(
                    "`\"{}\"` is written in {crates}{}: name it once, a `const` of the crate that owns what it names (or one both depend on), and use it from the others",
                    spelling.as_str(),
                    home.unwrap_or_default(),
                )
            }
            SharedString::NearMiss { how, one, one_at, other, other_at } => {
                let writes = |sites: &[SharedSite]| sites.iter().all(|it| it.row.access == Access::Writes);
                let reads = |sites: &[SharedSite]| sites.iter().any(|it| it.row.access == Access::Reads);
                let (writer, written, reader, read) = match (writes(one_at), writes(other_at)) {
                    (true, false) if reads(other_at) => (one_at, one, other_at, other),
                    (false, true) if reads(one_at) => (other_at, other, one_at, one),
                    _ => {
                        return format!(
                            "`\"{}\"` in {} and `\"{}\"` in {} differ only by {how}: one name spelled two ways; make them one `const` both crates use",
                            one.as_str(),
                            crate_list(one_at),
                            other.as_str(),
                            crate_list(other_at),
                        );
                    }
                };
                format!(
                    "{} writes `\"{}\"` and {} reads `\"{}\"`, which differ only by {how}: the reader never finds what the writer wrote; make them one `const` both crates use",
                    crate_list(writer),
                    written.as_str(),
                    crate_list(reader),
                    read.as_str(),
                )
            }
        }
    }
}

/// The crates of `sites`, distinct, in order: "`a`", "`a` and `b`", "`a`, `b` and `c`".
fn crate_list(sites: &[SharedSite]) -> String {
    let mut names: Vec<&str, Global> = Vec::new_in(Global);
    for site in sites {
        if !names.contains(&site.krate.as_str()) {
            names.push(site.krate.as_str());
        }
    }
    let mut out = String::new();
    for (at, name) in names.iter().enumerate() {
        match at {
            0 => {}
            _ if at + 1 == names.len() => out.push_str(" and "),
            _ => out.push_str(", "),
        }
        out.push('`');
        out.push_str(name);
        out.push('`');
    }
    out
}

/// The strings the crates share: each crate's name and its rows, sorted as [`string_rows`]
/// sorts them. A k-way merge by normalised spelling: a group whose exact spelling is in two or
/// more crates is [`SharedString::Same`]; two exact spellings of one group in two crates are a
/// [`SharedString::NearMiss`]; and two groups one non-digit character apart, both
/// [`NEAR_LEN`] long or more and in two crates, are one too (found by the deletion
/// neighbourhood of each spelling: two spellings one edit apart share one). Prose rows are
/// passed over. The output is in the merge's order, which the input's order fixes.
///
/// Caller: `crates/blaze-findings` runs it over the workspace's crates and reports each as
/// [`SharedString::lint`] (`string_shared_across_crates`, `string_near_miss_across_crates`).
pub(crate) fn merge_strings(crates: &[(Symbol, &[StringRow])]) -> Vec<SharedString, Global> {
    let mut out = Vec::new_in(Global);
    let mut cursors: Vec<usize, Global> = Vec::new_in(Global);
    cursors.resize(crates.len(), 0);
    // The groups past the floor, for the one-character pass: the spelling and its sites.
    let mut long: Vec<(Symbol, Vec<SharedSite, Global>), Global> = Vec::new_in(Global);
    let mut group: Vec<SharedSite, Global> = Vec::new_in(Global);
    loop {
        // The least normalised spelling at the heads of the crates' lists.
        let mut least: Option<Symbol> = None;
        for (at, (_, rows)) in crates.iter().enumerate() {
            let Some(row) = rows.get(cursors[at]) else { continue };
            if least.is_none_or(|it| row.norm.as_str() < it.as_str()) {
                least = Some(row.norm);
            }
        }
        let Some(norm) = least else { break };
        group.clear();
        for (at, (krate, rows)) in crates.iter().enumerate() {
            while let Some(row) = rows.get(cursors[at])
                && row.norm == norm
            {
                cursors[at] += 1;
                if !is_prose(row.exact.as_str()) {
                    group.push(SharedSite { krate: *krate, row: *row });
                }
            }
        }
        same_and_spellings(&group, &mut out);
        if !group.is_empty() && norm.as_str().chars().count() >= NEAR_LEN {
            let mut sites = Vec::new_in(Global);
            sites.extend(group.iter().copied());
            long.push((norm, sites));
        }
    }
    one_character(&long, &mut out);
    out
}

/// The exact spellings of one group: one in two crates is [`SharedString::Same`], two in two
/// crates a [`SharedString::NearMiss`].
fn same_and_spellings(group: &[SharedSite], out: &mut Vec<SharedString, Global>) {
    // The group's exact spellings, distinct, in order of first site.
    let mut spellings: Vec<Symbol, Global> = Vec::new_in(Global);
    for site in group {
        if !spellings.contains(&site.row.exact) {
            spellings.push(site.row.exact);
        }
    }
    let sites_of = |spelling: Symbol| {
        let mut sites = Vec::new_in(Global);
        sites.extend(group.iter().filter(|it| it.row.exact == spelling).copied());
        sites
    };
    let crates_of = |spelling: Symbol| {
        let mut crates: Vec<Symbol, Global> = Vec::new_in(Global);
        for site in group.iter().filter(|it| it.row.exact == spelling) {
            if !crates.contains(&site.krate) {
                crates.push(site.krate);
            }
        }
        crates
    };
    for &spelling in &spellings {
        if crates_of(spelling).len() >= 2 {
            out.push(SharedString::Same { spelling, sites: sites_of(spelling) });
        }
    }
    for (at, &one) in spellings.iter().enumerate() {
        for &other in &spellings[at + 1..] {
            let (a, b) = (crates_of(one), crates_of(other));
            // Two crates, one each: a crate with both is the crate's own near-miss.
            if !a.iter().any(|it| b.iter().any(|other| other != it)) {
                continue;
            }
            let how = near(one.as_str(), other.as_str()).unwrap_or("case, separators and a plural `s`");
            out.push(SharedString::NearMiss {
                how,
                one,
                one_at: sites_of(one),
                other,
                other_at: sites_of(other),
            });
        }
    }
}

/// The groups one non-digit character apart, in two crates: each spelling's deletion
/// neighbourhood (itself and each spelling one character shorter) is a key, and two spellings
/// one edit apart share a key.
fn one_character(long: &[(Symbol, Vec<SharedSite, Global>)], out: &mut Vec<SharedString, Global>) {
    let mut by_key: HashMap<Symbol, Vec<usize, Global>, Global> = HashMap::default();
    let mut key: Vec<u8, Global> = Vec::new_in(Global);
    for (at, (norm, _)) in long.iter().enumerate() {
        let text = norm.as_str();
        let mut keys: Vec<Symbol, Global> = Vec::new_in(Global);
        keys.push(*norm);
        for (cut, c) in text.char_indices() {
            key.clear();
            key.extend_from_slice(&text.as_bytes()[..cut]);
            key.extend_from_slice(&text.as_bytes()[cut + c.len_utf8()..]);
            keys.push(Symbol::intern(core::str::from_utf8(&key).unwrap_or(text)));
        }
        for it in keys {
            match by_key.get_mut(&it) {
                Some(members) => {
                    if !members.contains(&at) {
                        members.push(at);
                    }
                }
                None => {
                    let mut members = Vec::new_in(Global);
                    members.push(at);
                    by_key.insert(it, members);
                }
            }
        }
    }
    // Each pair once, in the merge's order.
    let mut pairs: Vec<(usize, usize), Global> = Vec::new_in(Global);
    for members in by_key.values() {
        for (at, &a) in members.iter().enumerate() {
            for &b in &members[at + 1..] {
                pairs.push((a.min(b), a.max(b)));
            }
        }
    }
    // The map's order is the hasher's: sorted, the pairs are in the merge's.
    pairs.sort_unstable();
    pairs.dedup();
    for (a, b) in pairs {
        let ((one, one_at), (other, other_at)) = (&long[a], &long[b]);
        let two_crates = one_at.iter().any(|x| other_at.iter().any(|y| x.krate != y.krate));
        if !two_crates || !one_edit(one.as_str(), other.as_str()) {
            continue;
        }
        let mut one_sites = Vec::new_in(Global);
        one_sites.extend(one_at.iter().copied());
        let mut other_sites = Vec::new_in(Global);
        other_sites.extend(other_at.iter().copied());
        out.push(SharedString::NearMiss {
            how: "one character",
            one: one_at[0].row.exact,
            one_at: one_sites,
            other: other_at[0].row.exact,
            other_at: other_sites,
        });
    }
}
