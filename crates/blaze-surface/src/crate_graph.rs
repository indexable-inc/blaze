//! The `CrateGraph` record: the crates of one build, each with its dependencies, its edition
//! and flags, and two bit matrices, the crates each one loads and the crates that load each
//! one, so a closure, a dependency test and the set of crates that can hold an impl are a row
//! read or an AND of rows.
//!
//! The crates are in dependency order: every crate after each crate it depends on, the ties
//! broken by [`CrateItemKey`]. That order is a function of the graph alone, so two builders of
//! one graph write equal bytes, and a walk in index order meets a crate's dependencies first.
//!
//! What a crate loads (its closure) is rustc's: itself, and every crate reached through its
//! dependencies, but never into a crate that loads only macros ([`CrateFlags::PROC_MACRO`])
//! nor through one. rustc resolves none of a proc-macro crate's dependencies into its
//! dependent's graph, and its metadata holds only its macros, so its impls, its lang items and
//! its own `std` are never the dependent's (`base_db::Crate::loads_only_macros`). The edge to
//! it is kept, with its name.
//!
//! The layout (`GraphRoot`, version 1):
//!
//! | field | holds |
//! |---|---|
//! | `crates` | `CrateRow` per crate in index order: key, its edges' run, edition (its year), flags |
//! | `edges` | `EdgeRow` per edge, each crate's run sorted by extern name: name's range, the dependency's index, flags |
//! | `names` | the extern names' bytes, UTF-8, back to back |
//! | `prefixes` | `PrefixRow` per crate sorted by key prefix: the prefix and the crate's index |
//! | `closure` | row `i`, `words` words: bit `j` when crate `i` loads crate `j` (`i` loads itself) |
//! | `dependents` | the transpose: bit `i` of row `j` when crate `i` loads crate `j` |
//!
//! The record is built here and read here; the lookups across it (`blaze_impls::candidates`,
//! `blaze_impls::lang_item`) are blaze-impls'.

use core::fmt;
use core::mem::offset_of;

use blaze_format::{FormatError, Part, Record, SchemaHash, Slice, Writer, WriterOpts, schema_of};
use blaze_id::{ContentId, CrateItemKey};
use blaze_own::{AVec, Alloc, Vec};

use crate::column::RecordBytes;
use crate::kind::Edition;

/// The most crates a graph holds: each bit matrix is `n * n` bits, 8 MiB at this bound.
pub const MAX_CRATES: usize = 8192;

/// The longest extern name an edge holds, in bytes.
pub const MAX_EXTERN_NAME: usize = 255;

/// The most edges a graph holds.
const MAX_EDGES: usize = 1 << 20;

/// An edition's year, as a crate row holds it.
const fn year_of(edition: Edition) -> u16 {
    match edition {
        Edition::E2015 => 2015,
        Edition::E2018 => 2018,
        Edition::E2021 => 2021,
        Edition::E2024 => 2024,
    }
}

/// The edition of a year a crate row holds; `None` for a year no edition is.
const fn edition_of_year(year: u16) -> Option<Edition> {
    match year {
        2015 => Some(Edition::E2015),
        2018 => Some(Edition::E2018),
        2021 => Some(Edition::E2021),
        2024 => Some(Edition::E2024),
        _ => None,
    }
}

/// A crate's place in one graph record: meaningful only with the record that gave it. A
/// view's methods index by it and panic on one past the graph's end, which only an index of
/// another graph can be.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct CrateIndex(u32);

impl CrateIndex {
    pub const fn get(self) -> u32 {
        self.0
    }

    const fn at(self) -> usize {
        self.0 as usize
    }

    /// The index at bit `at` of a row of this graph's ([`CrateGraphView::closure_row`],
    /// [`CrateGraphView::dependents_row`]): what a walk over a row gives.
    pub const fn of_bit(at: usize) -> CrateIndex {
        CrateIndex(at as u32)
    }
}

/// What a crate is, as far as the graph's readers ask.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct CrateFlags(u16);

impl CrateFlags {
    pub const NONE: CrateFlags = CrateFlags(0);
    /// A proc-macro crate: its dependents load its macros alone.
    pub const PROC_MACRO: CrateFlags = CrateFlags(1);
    /// A crate of the language (`core`, `alloc`, `std`, `proc_macro`, `test`: base-db's
    /// `CrateOrigin::Lang`): the crates that may hold incoherent inherent impls
    /// (`#[rustc_has_incoherent_inherent_impls]` types' and primitives').
    pub const LANG: CrateFlags = CrateFlags(1 << 1);
    /// A crate that may define lang items: a crate of the language, or one with
    /// `#![feature(lang_items)]`, from its roots. `blaze_impls::lang_item` reads these alone.
    pub const LANG_ITEMS: CrateFlags = CrateFlags(1 << 2);
    /// A crate that may write an inherent impl of a type it does not define: its root turns
    /// on `#![feature(rustc_attrs)]` (for `#[rustc_allow_incoherent_impl]`) or writes
    /// `#![rustc_coherence_is_core]`, from its roots. An inherent question about a built-in
    /// type, or a type marked `#[rustc_has_incoherent_inherent_impls]`, asks these crates.
    pub const INCOHERENT: CrateFlags = CrateFlags(1 << 3);

    const ALL: u16 = (1 << 4) - 1;

    pub const fn bits(self) -> u16 {
        self.0
    }

    pub const fn from_bits(bits: u16) -> Option<CrateFlags> {
        match bits & !CrateFlags::ALL {
            0 => Some(CrateFlags(bits)),
            _ => None,
        }
    }

    #[must_use]
    pub const fn union(self, other: CrateFlags) -> CrateFlags {
        CrateFlags(self.0 | other.0)
    }

    pub const fn contains(self, other: CrateFlags) -> bool {
        self.0 & other.0 == other.0
    }
}

/// An edge's marks: `blaze_units::schema::EdgeMarks`' two (`prelude`, `sysroot`, the one rule
/// being `EdgeFacts::marks`) and base-db's `private` (a dependency without `public = true`
/// under `-Zpublic-dependency`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct EdgeFlags(u16);

impl EdgeFlags {
    pub const NONE: EdgeFlags = EdgeFlags(0);
    /// In the dependent's extern prelude: nameable with no `extern crate`.
    pub const PRELUDE: EdgeFlags = EdgeFlags(1);
    /// Injected by the toolchain: a sysroot crate under a crate that is not one.
    pub const SYSROOT: EdgeFlags = EdgeFlags(1 << 1);
    /// A private dependency: its types may not appear in the dependent's public interface.
    pub const PRIVATE: EdgeFlags = EdgeFlags(1 << 2);

    const ALL: u16 = (1 << 3) - 1;

    pub const fn bits(self) -> u16 {
        self.0
    }

    pub const fn from_bits(bits: u16) -> Option<EdgeFlags> {
        match bits & !EdgeFlags::ALL {
            0 => Some(EdgeFlags(bits)),
            _ => None,
        }
    }

    #[must_use]
    pub const fn union(self, other: EdgeFlags) -> EdgeFlags {
        EdgeFlags(self.0 | other.0)
    }

    pub const fn contains(self, other: EdgeFlags) -> bool {
        self.0 & other.0 == other.0
    }
}

/// One dependency of a crate given to [`build_crate_graph`]: the crate depended on by its
/// key, the name the dependent's code calls it, and the edge's marks.
#[derive(Clone, Copy, Debug)]
pub struct DepInput<'a> {
    pub on: CrateItemKey,
    pub name: &'a str,
    pub flags: EdgeFlags,
}

/// One crate given to [`build_crate_graph`].
#[derive(Clone, Copy, Debug)]
pub struct CrateInput<'a> {
    pub key: CrateItemKey,
    pub edition: Edition,
    pub flags: CrateFlags,
    pub deps: &'a [DepInput<'a>],
}

/// Why crates are not a graph.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GraphError {
    TooManyCrates { count: usize },
    TooManyEdges { count: usize },
    /// A crate key whose first 8 bytes are zero: they mark a self shape with no definition
    /// ([`crate::SelfShape`]), so no item key may begin with them.
    ReservedPrefix { krate: CrateItemKey },
    /// One crate given twice.
    DuplicateCrate { krate: CrateItemKey },
    /// Two crates whose keys share their first 8 bytes, which every item key of each begins
    /// with: an item could not be told to be of one or the other.
    PrefixClash { first: CrateItemKey, second: CrateItemKey },
    /// A dependency on a crate not given.
    UnknownDependency { krate: CrateItemKey, on: CrateItemKey },
    SelfDependency { krate: CrateItemKey },
    /// A cycle of dependencies through `krate`, the crate of least key left on one.
    Cycle { krate: CrateItemKey },
    /// An extern name that is empty or longer than [`MAX_EXTERN_NAME`].
    ExternName { krate: CrateItemKey, on: CrateItemKey, len: usize },
    /// Two dependencies of `krate` by one name.
    DuplicateExternName { krate: CrateItemKey, first: CrateItemKey, second: CrateItemKey },
}

impl fmt::Display for GraphError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GraphError::TooManyCrates { count } => {
                write!(f, "a crate graph of {count} crates, past its bound of {MAX_CRATES}")
            }
            GraphError::TooManyEdges { count } => {
                write!(f, "a crate graph of {count} edges, past its bound of {MAX_EDGES}")
            }
            GraphError::ReservedPrefix { krate } => write!(
                f,
                "the crate key {krate:x} begins with 8 zero bytes, which mark a self shape with no definition"
            ),
            GraphError::DuplicateCrate { krate } => {
                write!(f, "the crate {krate:x} is given to the crate graph twice")
            }
            GraphError::PrefixClash { first, second } => write!(
                f,
                "the crates {first:x} and {second:x} share their key's first 8 bytes, which their item keys begin with"
            ),
            GraphError::UnknownDependency { krate, on } => {
                write!(f, "the crate {krate:x} depends on {on:x}, which is not in the graph")
            }
            GraphError::SelfDependency { krate } => write!(f, "the crate {krate:x} depends on itself"),
            GraphError::Cycle { krate } => {
                write!(f, "the crate {krate:x} is on a cycle of dependencies")
            }
            GraphError::ExternName { krate, on, len } => write!(
                f,
                "the crate {krate:x} names its dependency {on:x} by {len} bytes: a name is 1 to {MAX_EXTERN_NAME}"
            ),
            GraphError::DuplicateExternName { krate, first, second } => write!(
                f,
                "the crate {krate:x} names its dependencies {first:x} and {second:x} by one name"
            ),
        }
    }
}

impl core::error::Error for GraphError {}

blaze_format::record! {
    #[derive(Clone, Copy, Debug)]
    struct CrateRow: 1 {
        key: ContentId,
        edges_at: u32,
        edges_len: u32,
        edition: u16,
        flags: u16,
    }
}

blaze_format::record! {
    #[derive(Clone, Copy, Debug)]
    struct EdgeRow: 1 {
        name_at: u32,
        name_len: u32,
        dep: u32,
        flags: u32,
    }
}

blaze_format::record! {
    #[derive(Clone, Copy, Debug)]
    struct PrefixRow: 1 {
        prefix: [u8; 8],
        index: u32,
    }
}

blaze_format::record! {
    #[derive(Clone, Copy)]
    struct GraphRoot: 1 {
        crates: Slice<CrateRow>,
        edges: Slice<EdgeRow>,
        names: Slice<u8>,
        prefixes: Slice<PrefixRow>,
        closure: Slice<u64>,
        dependents: Slice<u64>,
        words: u32,
    }
}

/// The schema a crate graph buffer's header names.
pub const GRAPH_SCHEMA: SchemaHash =
    schema_of(&[Part::Str("CrateGraph"), Part::U64(1), Part::Schema(GraphRoot::SCHEMA)]);

/// Words in a row of `bits` bits.
pub(crate) const fn words_for(bits: usize) -> usize {
    bits.div_ceil(64)
}

/// The slice a field `offset` bytes into a record at `record_at` holds of `len` values at
/// `target`.
pub(crate) fn slice_field<T>(record_at: u64, offset: usize, target: u64, len: usize) -> Slice<T> {
    Slice::between(record_at + offset as u64, target, len)
}

/// The graph of `crates`, in any order, as one buffer in `alloc`: equal bytes for every order
/// of the same crates and of each crate's dependencies.
pub fn build_crate_graph<'a>(
    crates: &[CrateInput<'_>],
    alloc: Alloc<'a>,
) -> Result<RecordBytes<'a>, GraphError> {
    let n = crates.len();
    if n > MAX_CRATES {
        return Err(GraphError::TooManyCrates { count: n });
    }
    let edge_count: usize = crates.iter().map(|it| it.deps.len()).sum();
    if edge_count > MAX_EDGES {
        return Err(GraphError::TooManyEdges { count: edge_count });
    }
    // The crates by key: input places sorted by key, which sorts them by prefix as well.
    let mut by_key: AVec<'a, u32> = Vec::with_capacity_in(n, alloc);
    by_key.extend((0..n).map(|p| p as u32));
    by_key.sort_unstable_by_key(|&p| crates[p as usize].key);
    for &p in &by_key {
        let krate = crates[p as usize].key;
        if krate.prefix() == [0; 8] {
            return Err(GraphError::ReservedPrefix { krate });
        }
    }
    for pair in by_key.windows(2) {
        let (first, second) = (crates[pair[0] as usize].key, crates[pair[1] as usize].key);
        if first == second {
            return Err(GraphError::DuplicateCrate { krate: first });
        }
        if first.prefix() == second.prefix() {
            return Err(GraphError::PrefixClash { first, second });
        }
    }
    let mut rank = Vec::from_elem_in(0u32, n, alloc);
    for (r, &p) in by_key.iter().enumerate() {
        rank[p as usize] = r as u32;
    }
    let place_of = |key: CrateItemKey| {
        by_key
            .binary_search_by_key(&key, |&p| crates[p as usize].key)
            .ok()
            .map(|r| by_key[r] as usize)
    };
    // Each edge's target, by input place, in each crate's run of `starts`.
    let mut starts = Vec::from_elem_in(0usize, n + 1, alloc);
    let mut targets: AVec<'a, u32> = Vec::with_capacity_in(edge_count, alloc);
    for (p, krate) in crates.iter().enumerate() {
        starts[p] = targets.len();
        for dep in krate.deps {
            let Some(q) = place_of(dep.on) else {
                return Err(GraphError::UnknownDependency { krate: krate.key, on: dep.on });
            };
            if q == p {
                return Err(GraphError::SelfDependency { krate: krate.key });
            }
            if dep.name.is_empty() || dep.name.len() > MAX_EXTERN_NAME {
                return Err(GraphError::ExternName { krate: krate.key, on: dep.on, len: dep.name.len() });
            }
            targets.push(q as u32);
        }
    }
    starts[n] = targets.len();
    // Who depends on each crate, by input place: the reverse edges, as runs.
    let mut rev_starts = Vec::from_elem_in(0usize, n + 1, alloc);
    for &q in &targets {
        rev_starts[q as usize + 1] += 1;
    }
    for q in 0..n {
        rev_starts[q + 1] += rev_starts[q];
    }
    let mut rev_fill = rev_starts.clone();
    let mut rev = Vec::from_elem_in(0u32, targets.len(), alloc);
    for p in 0..n {
        for &q in &targets[starts[p]..starts[p + 1]] {
            rev[rev_fill[q as usize]] = p as u32;
            rev_fill[q as usize] += 1;
        }
    }
    // Kahn's order, the ready crate of least key first: a bit per rank.
    let mut remaining = Vec::from_elem_in(0usize, n, alloc);
    let mut ready = Vec::from_elem_in(0u64, words_for(n), alloc);
    for p in 0..n {
        remaining[p] = starts[p + 1] - starts[p];
        if remaining[p] == 0 {
            let r = rank[p] as usize;
            ready[r / 64] |= 1 << (r % 64);
        }
    }
    const UNPLACED: u32 = u32::MAX;
    let mut index = Vec::from_elem_in(UNPLACED, n, alloc);
    let mut order: AVec<'a, u32> = Vec::with_capacity_in(n, alloc);
    for i in 0..n {
        let Some(word) = ready.iter().position(|&it| it != 0) else {
            let left = by_key.iter().find(|&&p| index[p as usize] == UNPLACED);
            let krate = crates[left.map_or(0, |&p| p as usize)].key;
            return Err(GraphError::Cycle { krate });
        };
        let r = word * 64 + ready[word].trailing_zeros() as usize;
        ready[word] &= ready[word] - 1;
        let p = by_key[r] as usize;
        index[p] = i as u32;
        order.push(p as u32);
        for &d in &rev[rev_starts[p]..rev_starts[p + 1]] {
            let d = d as usize;
            remaining[d] -= 1;
            if remaining[d] == 0 {
                let r = rank[d] as usize;
                ready[r / 64] |= 1 << (r % 64);
            }
        }
    }
    // The rows, the edges and the names, in index order.
    let mut crate_rows: AVec<'a, CrateRow> = Vec::with_capacity_in(n, alloc);
    let mut edge_rows: AVec<'a, EdgeRow> = Vec::with_capacity_in(edge_count, alloc);
    let mut names: AVec<'a, u8> = Vec::new_in(alloc);
    let mut slots: AVec<'a, usize> = Vec::new_in(alloc);
    for &p in &order {
        let krate = &crates[p as usize];
        slots.clear();
        slots.extend(0..krate.deps.len());
        let target_index = |slot: usize| index[targets[starts[p as usize] + slot] as usize];
        slots.sort_unstable_by(|&a, &b| {
            (krate.deps[a].name.as_bytes(), target_index(a))
                .cmp(&(krate.deps[b].name.as_bytes(), target_index(b)))
        });
        for pair in slots.windows(2) {
            if krate.deps[pair[0]].name == krate.deps[pair[1]].name {
                return Err(GraphError::DuplicateExternName {
                    krate: krate.key,
                    first: krate.deps[pair[0]].on,
                    second: krate.deps[pair[1]].on,
                });
            }
        }
        crate_rows.push(CrateRow {
            key: krate.key.0,
            edges_at: edge_rows.len() as u32,
            edges_len: slots.len() as u32,
            edition: year_of(krate.edition),
            flags: krate.flags.bits(),
        });
        for &slot in &slots {
            let dep = &krate.deps[slot];
            edge_rows.push(EdgeRow {
                name_at: names.len() as u32,
                name_len: dep.name.len() as u32,
                dep: target_index(slot),
                flags: u32::from(dep.flags.bits()),
            });
            names.extend_from_slice(dep.name.as_bytes());
        }
    }
    // What each crate loads, its dependencies' rows before its own; and the transpose.
    let words = words_for(n);
    let mut closure = Vec::from_elem_in(0u64, n * words, alloc);
    for i in 0..n {
        let (done, rest) = closure.split_at_mut(i * words);
        let row = &mut rest[..words];
        row[i / 64] |= 1 << (i % 64);
        let edges = &edge_rows[crate_rows[i].edges_at as usize..][..crate_rows[i].edges_len as usize];
        for edge in edges {
            let t = edge.dep as usize;
            if CrateFlags(crate_rows[t].flags).contains(CrateFlags::PROC_MACRO) {
                continue;
            }
            for (it, from) in row.iter_mut().zip(&done[t * words..(t + 1) * words]) {
                *it |= *from;
            }
        }
    }
    let mut dependents = Vec::from_elem_in(0u64, n * words, alloc);
    for i in 0..n {
        for j in (Members { words: &closure[i * words..(i + 1) * words], at: 0, bits: 0 }) {
            dependents[j.at() * words + i / 64] |= 1 << (i % 64);
        }
    }
    let mut prefix_rows: AVec<'a, PrefixRow> = Vec::with_capacity_in(n, alloc);
    prefix_rows.extend(by_key.iter().map(|&p| PrefixRow {
        prefix: crates[p as usize].key.prefix(),
        index: index[p as usize],
    }));

    let mut out = Writer::new(WriterOpts { schema: GRAPH_SCHEMA, alloc: alloc, capacity: None });
    let names_at = out.push_slice(&names);
    let edges_at = out.push_slice(&edge_rows);
    let crates_at = out.push_slice(&crate_rows);
    let prefixes_at = out.push_slice(&prefix_rows);
    let closure_at = out.push_slice(&closure);
    let dependents_at = out.push_slice(&dependents);
    let at = out.next::<GraphRoot>();
    let root = GraphRoot {
        crates: slice_field(at, offset_of!(GraphRoot, crates), crates_at, crate_rows.len()),
        edges: slice_field(at, offset_of!(GraphRoot, edges), edges_at, edge_rows.len()),
        names: slice_field(at, offset_of!(GraphRoot, names), names_at, names.len()),
        prefixes: slice_field(at, offset_of!(GraphRoot, prefixes), prefixes_at, prefix_rows.len()),
        closure: slice_field(at, offset_of!(GraphRoot, closure), closure_at, closure.len()),
        dependents: slice_field(at, offset_of!(GraphRoot, dependents), dependents_at, dependents.len()),
        words: words as u32,
    };
    let written = out.push(&root);
    debug_assert_eq!(written, at);
    Ok(out.finish(at))
}

/// One dependency as the graph holds it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DepEdge<'a> {
    pub on: CrateIndex,
    pub name: &'a str,
    pub flags: EdgeFlags,
}

/// The crates of a bit row, in index order.
#[derive(Clone, Debug)]
pub struct Members<'a> {
    words: &'a [u64],
    /// The next word to load.
    at: usize,
    /// What is left of the word before `at`.
    bits: u64,
}

impl Iterator for Members<'_> {
    type Item = CrateIndex;

    fn next(&mut self) -> Option<CrateIndex> {
        loop {
            if self.bits != 0 {
                let bit = self.bits.trailing_zeros();
                self.bits &= self.bits - 1;
                return Some(CrateIndex(((self.at - 1) * 64) as u32 + bit));
            }
            self.bits = *self.words.get(self.at)?;
            self.at += 1;
        }
    }
}

/// A crate graph read in place, its every index checked once when read: what a view's
/// methods read is in bounds.
#[derive(Clone, Copy, Debug)]
pub struct CrateGraphView<'a> {
    crates: &'a [CrateRow],
    edges: &'a [EdgeRow],
    names: &'a [u8],
    prefixes: &'a [PrefixRow],
    closure: &'a [u64],
    dependents: &'a [u64],
    words: usize,
}

impl<'a> CrateGraphView<'a> {
    /// The graph in `buffer`, which [`build_crate_graph`] wrote: its schema and layout
    /// checked, and every index and name in it.
    pub fn read(buffer: &'a [u8]) -> Result<CrateGraphView<'a>, FormatError> {
        let root = blaze_format::open_as::<GraphRoot>(buffer, GRAPH_SCHEMA)?;
        let at = blaze_format::header(buffer)?.root;
        let bad = |what: &'static str| FormatError::Invalid { at, what };
        let view = CrateGraphView {
            crates: root.crates.follow(buffer)?,
            edges: root.edges.follow(buffer)?,
            names: root.names.follow(buffer)?,
            prefixes: root.prefixes.follow(buffer)?,
            closure: root.closure.follow(buffer)?,
            dependents: root.dependents.follow(buffer)?,
            words: root.words as usize,
        };
        let n = view.crates.len();
        if n > MAX_CRATES || view.words != words_for(n) {
            return Err(bad("a crate graph whose row width is not its crates'"));
        }
        if view.closure.len() != n * view.words || view.dependents.len() != n * view.words {
            return Err(bad("a crate graph whose bit matrices are not n by n"));
        }
        if view.prefixes.len() != n {
            return Err(bad("a crate graph with a prefix row count that is not its crates'"));
        }
        for (i, krate) in view.crates.iter().enumerate() {
            if edition_of_year(krate.edition).is_none() {
                return Err(bad("a crate of an edition blaze does not know"));
            }
            if CrateFlags::from_bits(krate.flags).is_none() {
                return Err(bad("a crate with flags blaze does not know"));
            }
            let end = (krate.edges_at as usize).checked_add(krate.edges_len as usize);
            let Some(edges) = end.and_then(|end| view.edges.get(krate.edges_at as usize..end)) else {
                return Err(bad("a crate whose edges run past the edge table"));
            };
            for edge in edges {
                if edge.dep as usize >= i {
                    return Err(bad("an edge to a crate not before its dependent"));
                }
                if u16::try_from(edge.flags).ok().and_then(EdgeFlags::from_bits).is_none() {
                    return Err(bad("an edge with flags blaze does not know"));
                }
                let end = (edge.name_at as usize).checked_add(edge.name_len as usize);
                let Some(name) = end.and_then(|end| view.names.get(edge.name_at as usize..end)) else {
                    return Err(bad("an extern name past the name table"));
                };
                if core::str::from_utf8(name).is_err() {
                    return Err(bad("an extern name that is not UTF-8"));
                }
            }
        }
        for pair in view.prefixes.windows(2) {
            if pair[0].prefix >= pair[1].prefix {
                return Err(bad("prefix rows not sorted strictly"));
            }
        }
        for row in view.prefixes {
            match view.crates.get(row.index as usize) {
                Some(krate) if CrateItemKey(krate.key).prefix() == row.prefix => {}
                _ => return Err(bad("a prefix row naming a crate of another prefix")),
            }
        }
        Ok(view)
    }

    /// How many crates the graph has.
    pub fn len(&self) -> usize {
        self.crates.len()
    }

    pub fn is_empty(&self) -> bool {
        self.crates.is_empty()
    }

    /// Every crate, in index order: each after the crates it depends on.
    pub fn crates(&self) -> impl Iterator<Item = CrateIndex> + use<> {
        (0..self.crates.len() as u32).map(CrateIndex)
    }

    pub fn key(&self, of: CrateIndex) -> CrateItemKey {
        CrateItemKey(self.crates[of.at()].key)
    }

    pub fn edition(&self, of: CrateIndex) -> Edition {
        match edition_of_year(self.crates[of.at()].edition) {
            Some(edition) => edition,
            None => unreachable!("an edition checked when the graph was read"),
        }
    }

    pub fn flags(&self, of: CrateIndex) -> CrateFlags {
        CrateFlags(self.crates[of.at()].flags)
    }

    /// The crate's dependencies, sorted by extern name.
    pub fn deps(&self, of: CrateIndex) -> impl Iterator<Item = DepEdge<'a>> + use<'a> {
        let krate = self.crates[of.at()];
        let (names, edges): (&'a [u8], &'a [EdgeRow]) = (self.names, self.edges);
        edges[krate.edges_at as usize..][..krate.edges_len as usize].iter().map(move |edge| {
            let bytes = &names[edge.name_at as usize..][..edge.name_len as usize];
            let name = match core::str::from_utf8(bytes) {
                Ok(name) => name,
                Err(_) => unreachable!("a name checked when the graph was read"),
            };
            DepEdge { on: CrateIndex(edge.dep), name, flags: EdgeFlags(edge.flags as u16) }
        })
    }

    /// The crate whose key begins with `prefix`: what an item key's crate is found by.
    pub fn crate_of_prefix(&self, prefix: [u8; 8]) -> Option<CrateIndex> {
        let at = self.prefixes.binary_search_by(|row| row.prefix.cmp(&prefix)).ok()?;
        Some(CrateIndex(self.prefixes[at].index))
    }

    /// The crate of key `key`.
    pub fn index_of(&self, key: CrateItemKey) -> Option<CrateIndex> {
        self.crate_of_prefix(key.prefix()).filter(|&it| self.key(it) == key)
    }

    /// The bit row of what `of` loads, [`CrateGraphView::closure`] as words (bit `j` for
    /// [`CrateIndex::of_bit`]`(j)`): what a lookup ANDs with other rows.
    pub fn closure_row(&self, of: CrateIndex) -> &'a [u64] {
        let closure: &'a [u64] = self.closure;
        &closure[of.at() * self.words..][..self.words]
    }

    /// The bit row of the crates that load `of`, [`CrateGraphView::dependents`] as words.
    pub fn dependents_row(&self, of: CrateIndex) -> &'a [u64] {
        let dependents: &'a [u64] = self.dependents;
        &dependents[of.at() * self.words..][..self.words]
    }

    /// What `of` loads: itself and every crate reached through its dependencies, never into a
    /// proc-macro crate nor through one; in index order.
    pub fn closure(&self, of: CrateIndex) -> Members<'a> {
        Members { words: self.closure_row(of), at: 0, bits: 0 }
    }

    /// The crates that load `of`, itself among them, in index order.
    pub fn dependents(&self, of: CrateIndex) -> Members<'a> {
        Members { words: self.dependents_row(of), at: 0, bits: 0 }
    }

    /// Whether `a` loads `b`: true for `a == b`.
    pub fn depends_on(&self, a: CrateIndex, b: CrateIndex) -> bool {
        self.closure_row(a)[b.at() / 64] & (1 << (b.at() % 64)) != 0
    }
}
