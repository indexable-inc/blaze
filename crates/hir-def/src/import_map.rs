//! A map of all publicly exported items in a crate.

use std::{cmp::Ordering, fmt};

use base_db::{Crate, Db};
use blaze_hash::{FixedState, HashSet};
use blaze_ids::bits::BitSet;
use blaze_mem::Frame;
use hir_expand::name::Name;
use indexmap::IndexSet;
use span::Edition;
use stdx::format_to;

use crate::{
    AdtId, AssocItemId, AttrDefId, Complete, EnumId, ModuleDefId, ModuleId, TraitId,
    attrs::AttrFlags,
    item_scope::{ImportOrExternCrate, ItemInNs},
    nameres::{assoc::TraitItems, crate_def_map},
    visibility::Visibility,
};

type FxIndexSet<T> = IndexSet<T, FixedState>;

/// Item import details stored in the `ImportMap`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ImportInfo {
    /// A name that can be used to import the item, relative to the container.
    pub name: Name,
    /// The module containing this item.
    pub container: ModuleId,
    /// Whether this item is annotated with `#[doc(hidden)]`.
    pub is_doc_hidden: bool,
    /// Whether this item is annotated with `#[unstable(..)]`.
    pub is_unstable: bool,
    /// The value of `#[rust_analyzer::completions(...)]`, if exists.
    pub complete: Complete,
}

/// 12 bytes: the name's symbol (4), the module's id (4) and three bytes of flags.
const _: () = assert!(size_of::<ImportInfo>() == 12);

/// A map from publicly exported items to its name.
///
/// Reexports of items are taken into account.
///
/// Columns over a row per item, in the order the collection first meets each item: `items` (the
/// key with its hash, found in one probe), the row's infos as a CSR range of `infos`, and a bit
/// for "associated item of a trait". A row costs its set entry and index slot (about 34 bytes)
/// and 4 for its CSR offset; each info 24, and 4 for its place in `importables`; plus each run's
/// lowercased name and 8 bytes for its two offsets. Nothing is allocated per row or per info.
#[derive(Default)]
pub struct ImportMap {
    /// Every importable item; an item's index is its row.
    items: FxIndexSet<ItemInNs>,
    /// The infos of row `i` are `infos[info_start[i]..info_start[i + 1]]`, in the order the
    /// imports were found. Every row has at least one, so the offsets strictly increase.
    info_start: Box<[u32]>,
    /// Information of the imports that bring each item into scope, grouped by row.
    infos: Box<[ImportInfo]>,
    /// Bit `i` set: row `i` is an associated item of a trait.
    trait_assoc_items: BitSet,
    /// Indices into [`Self::infos`], sorted by their names lowercased (ASCII), ties in row
    /// order, in runs of one name ([`Self::run_start`]).
    importables: Box<[u32]>,
    /// The name of each run of [`Self::importables`] equal up to ASCII case, lowercased, end to
    /// end in byte order: run `r`'s is `keys[key_start[r]..key_start[r + 1]]`. Strictly
    /// increasing, as the names are sorted.
    keys: Box<[u8]>,
    key_start: Box<[u32]>,
    /// Run `r` is `importables[run_start[r]..run_start[r + 1]]`.
    run_start: Box<[u32]>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Ord, PartialOrd)]
enum IsTraitAssocItem {
    Yes,
    No,
}

#[blaze_query::tracked]
impl ImportMap {
    #[blaze_query::tracked(returns(ref), mem_tag = Nameres)]
    pub fn of(db: &Db, krate: Crate) -> Self {
        Self::import_map_query_impl(db, krate)
    }
}

impl ImportMap {
    pub fn dump(&self, db: &Db) -> String {
        // alloc: a debug dump (the edge)
        let mut out = String::new();
        for (row, item) in self.items.iter().enumerate() {
            format_to!(out, "{:?} ({:?}) -> ", item, self.is_trait_assoc_item(row));
            for v in self.row_infos(row) {
                format_to!(out, "{}:{:?}, ", v.name.display(db, Edition::CURRENT), v.container);
            }
            format_to!(out, "\n");
        }
        out
    }

    fn import_map_query_impl(db: &Db, krate: Crate) -> Self {
        let _p = tracing::info_span!("import_map_query").entered();
        blaze_mem::frame::current(|frame| Self::build(db, krate, frame))
    }

    fn build(db: &Db, krate: Crate, frame: &Frame) -> Self {
        let found = Collector::collect(db, krate, frame);
        let rows = found.rows.len();

        // Group the infos by row with a counting sort, which keeps the order they were found in
        // within a row. Both columns come out at their length and become the memo's.
        // clone: an `ImportInfo` is plain data (`Name` is `Copy`), copied into the memo's column
        let pairs = found.infos.iter().map(|(row, info)| (*row, info.clone()));
        // alloc: the memo's offsets and infos, exact; the sort's cursors are the frame's
        let (info_start, infos) = blaze_ids::radix::Csr::build_in(rows, pairs, frame).into_parts();
        let (info_start, infos) = (info_start.into_boxed_slice(), infos.into_boxed_slice());

        // alloc: the memo's bits, exact
        let mut trait_assoc_items = BitSet::new(rows);
        for (row, &is_trait_assoc_item) in found.trait_assoc_items.iter().enumerate() {
            if is_trait_assoc_item {
                trait_assoc_items.insert(row);
            }
        }

        // Every info, in row order, sorted by its lowercased name, ties by index: the stable
        // order, with no merge buffer. The order of UTF-8 bytes is the order of chars, and
        // lowercasing ASCII bytes leaves the others alone, so this is the keys' byte order.
        // alloc: the memo's order, exact
        let mut importables: Box<[u32]> = (0..infos.len() as u32).collect();
        importables.sort_unstable_by(|&l, &r| {
            cmp_ascii_lowercase(infos[l as usize].name.as_str(), infos[r as usize].name.as_str())
                .then(l.cmp(&r))
        });

        // One key per run of names equal up to ASCII case, lowercased, end to end. A run is at
        // least one info, and its key at most the bytes of one of their names.
        let name_bytes: usize = infos.iter().map(|info| info.name.as_str().len()).sum();
        let mut keys = Vec::with_capacity_in(name_bytes, frame);
        let mut key_start = Vec::with_capacity_in(importables.len() + 1, frame);
        let mut run_start = Vec::with_capacity_in(importables.len() + 1, frame);
        let mut start = 0;
        while start < importables.len() {
            let name = infos[importables[start] as usize].name.as_str();
            let mut end = start + 1;
            while end < importables.len()
                && infos[importables[end] as usize].name.as_str().eq_ignore_ascii_case(name)
            {
                end += 1;
            }
            key_start.push(keys.len() as u32);
            run_start.push(start as u32);
            keys.extend(name.bytes().map(|b| b.to_ascii_lowercase()));
            start = end;
        }
        key_start.push(keys.len() as u32);
        run_start.push(importables.len() as u32);

        // alloc: the memo's set, exact
        let mut items = FxIndexSet::with_capacity_and_hasher(rows, FixedState);
        items.extend(found.rows.iter().copied());
        ImportMap {
            items,
            info_start,
            infos,
            trait_assoc_items,
            importables,
            // alloc: the memo's keys and their offsets, exact
            keys: Box::from(&keys[..]),
            key_start: Box::from(&key_start[..]),
            run_start: Box::from(&run_start[..]),
        }
    }

    pub fn import_info_for(&self, item: ItemInNs) -> Option<&[ImportInfo]> {
        self.items.get_index_of(&item).map(|row| self.row_infos(row))
    }

    fn row_infos(&self, row: usize) -> &[ImportInfo] {
        &self.infos[self.info_start[row] as usize..self.info_start[row + 1] as usize]
    }

    /// The row an index into [`Self::infos`] belongs to: a binary search of the offsets.
    fn row_of_info(&self, info: u32) -> usize {
        self.info_start.partition_point(|&start| start <= info) - 1
    }

    /// How many runs of one name there are.
    fn runs(&self) -> usize {
        self.run_start.len().saturating_sub(1)
    }

    fn key(&self, run: usize) -> &[u8] {
        &self.keys[self.key_start[run] as usize..self.key_start[run + 1] as usize]
    }

    fn run(&self, run: usize) -> &[u32] {
        &self.importables[self.run_start[run] as usize..self.run_start[run + 1] as usize]
    }

    /// The first run whose key is not below `query`.
    fn lower_bound(&self, query: &[u8]) -> usize {
        let (mut lo, mut hi) = (0, self.runs());
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.key(mid) < query {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    /// Each run whose key `mode` takes for `query`, lowercased: the key itself for
    /// [`SearchMode::Exact`], a key it starts for [`SearchMode::Prefix`], a key it is a
    /// subsequence of (by bytes) for [`SearchMode::Fuzzy`]; the runs fst's `Str`,
    /// `Str::starts_with` and `Subsequence` automata took over the same keys, in key order.
    fn search_runs(&self, mode: SearchMode, query: &[u8], mut each: impl FnMut(usize)) {
        match mode {
            SearchMode::Exact => {
                let at = self.lower_bound(query);
                if at < self.runs() && self.key(at) == query {
                    each(at);
                }
            }
            SearchMode::Prefix => {
                for run in self.lower_bound(query)..self.runs() {
                    if !self.key(run).starts_with(query) {
                        break;
                    }
                    each(run);
                }
            }
            SearchMode::Fuzzy => {
                for run in 0..self.runs() {
                    if is_subsequence(query, self.key(run)) {
                        each(run);
                    }
                }
            }
        }
    }

    fn is_trait_assoc_item(&self, row: usize) -> IsTraitAssocItem {
        if self.trait_assoc_items.contains(row) {
            IsTraitAssocItem::Yes
        } else {
            IsTraitAssocItem::No
        }
    }
}

fn cmp_ascii_lowercase(l: &str, r: &str) -> Ordering {
    let l = l.bytes().map(|b| b.to_ascii_lowercase());
    let r = r.bytes().map(|b| b.to_ascii_lowercase());
    l.cmp(r)
}

/// Whether the bytes of `needle` appear in `hay` in order: fst's `Subsequence` automaton.
fn is_subsequence(needle: &[u8], hay: &[u8]) -> bool {
    let mut rest = hay;
    needle.iter().all(|&byte| match rest.iter().position(|&it| it == byte) {
        Some(at) => {
            rest = &rest[at + 1..];
            true
        }
        None => false,
    })
}

/// What the walk over the public modules finds, before it is laid out as columns: the run's.
struct Collector<'f> {
    /// The row of each item found, in the order first met.
    rows_of: hashbrown::HashMap<ItemInNs, u32, FixedState, &'f Frame>,
    rows: Vec<ItemInNs, &'f Frame>,
    /// Per row, whether the item is an associated item of a trait (as the first info found
    /// for it says).
    trait_assoc_items: Vec<bool, &'f Frame>,
    /// `(row, info)` in the order found.
    infos: Vec<(u32, ImportInfo), &'f Frame>,
}

impl<'f> Collector<'f> {
    fn add(&mut self, item: ItemInNs, is_trait_assoc_item: bool, info: ImportInfo) {
        let next = self.rows.len() as u32;
        let row = *self.rows_of.entry(item).or_insert(next);
        if row == next {
            self.rows.push(item);
            self.trait_assoc_items.push(is_trait_assoc_item);
        }
        self.infos.push((row, info));
    }

    fn collect(db: &Db, krate: Crate, frame: &'f Frame) -> Collector<'f> {
        let _p = tracing::info_span!("collect_import_map").entered();

        let def_map = crate_def_map(db, krate);
        let mut found = Collector {
            rows_of: hashbrown::HashMap::with_hasher_in(FixedState, frame),
            rows: Vec::new_in(frame),
            trait_assoc_items: Vec::new_in(frame),
            infos: Vec::new_in(frame),
        };

        // We look only into modules that are public(ly reexported), starting with the crate root.
        let root = def_map.root_module_id();
        let mut worklist = Vec::new_in(frame);
        worklist.push(root);
        let mut visited = hashbrown::HashSet::with_hasher_in(FixedState, frame);

        while let Some(module) = worklist.pop() {
            if !visited.insert(module) {
                continue;
            }
            let mod_data = if module.krate(db) == krate {
                &def_map.module(db, module)
            } else {
                // The crate might reexport a module defined in another crate.
                &module.def_map(db).module(db, module)
            };

            // The public items of each name, types then values then macros.
            for (&name, per_ns) in mod_data.scope(db).entries() {
                if let Some(it) = per_ns.types
                    && it.vis == Visibility::Public
                {
                    found.visit(db, &mut worklist, name, module, ItemInNs::Types(it.def), it.import);
                }
                if let Some(it) = per_ns.values
                    && it.vis == Visibility::Public
                {
                    let import = it.import.map(ImportOrExternCrate::from);
                    found.visit(db, &mut worklist, name, module, ItemInNs::Values(it.def), import);
                }
                if let Some(it) = per_ns.macros
                    && it.vis == Visibility::Public
                {
                    found.visit(db, &mut worklist, name, module, ItemInNs::Macros(it.def), it.import);
                }
            }
        }
        found
    }

    fn visit(
        &mut self,
        db: &Db,
        worklist: &mut Vec<ModuleId, &'f Frame>,
        name: Name,
        module: ModuleId,
        item: ItemInNs,
        import: Option<ImportOrExternCrate>,
    ) {
        let attr_id = if let Some(import) = import {
            match import {
                ImportOrExternCrate::ExternCrate(id) => Some(id.into()),
                ImportOrExternCrate::Import(id) => Some(id.use_.into()),
                ImportOrExternCrate::Glob(id) => Some(id.use_.into()),
            }
        } else {
            match item {
                ItemInNs::Types(id) | ItemInNs::Values(id) => match id {
                    ModuleDefId::ModuleId(it) => Some(AttrDefId::ModuleId(it)),
                    ModuleDefId::FunctionId(it) => Some(it.into()),
                    ModuleDefId::AdtId(it) => Some(it.into()),
                    ModuleDefId::EnumVariantId(it) => Some(it.into()),
                    ModuleDefId::ConstId(it) => Some(it.into()),
                    ModuleDefId::StaticId(it) => Some(it.into()),
                    ModuleDefId::TraitId(it) => Some(it.into()),
                    ModuleDefId::TypeAliasId(it) => Some(it.into()),
                    ModuleDefId::MacroId(it) => Some(it.into()),
                    ModuleDefId::BuiltinType(_) => None,
                },
                ItemInNs::Macros(id) => Some(id.into()),
            }
        };
        let (is_doc_hidden, is_unstable, do_not_complete) = match attr_id {
            None => (false, false, Complete::Yes),
            Some(attr_id) => {
                let attrs = AttrFlags::query(db, attr_id);
                let do_not_complete =
                    Complete::extract(matches!(attr_id, AttrDefId::TraitId(_)), attrs);
                (
                    attrs.contains(AttrFlags::IS_DOC_HIDDEN),
                    attrs.contains(AttrFlags::IS_UNSTABLE),
                    do_not_complete,
                )
            }
        };

        let import_info = ImportInfo {
            name,
            container: module,
            is_doc_hidden,
            is_unstable,
            complete: do_not_complete,
        };

        let module_def = item.as_module_def_id();
        if let Some(ModuleDefId::TraitId(tr)) = module_def {
            self.collect_trait_assoc_items(
                db,
                tr,
                matches!(item, ItemInNs::Types(_)),
                &import_info,
            );
        } else if let Some(ModuleDefId::AdtId(AdtId::EnumId(enum_))) = module_def {
            self.collect_enum_variants(db, enum_, &import_info);
        }

        self.add(item, false, import_info);

        // If we've just added a module, descend into it.
        if let Some(ModuleDefId::ModuleId(mod_id)) = module_def {
            worklist.push(mod_id);
        }
    }

    fn collect_enum_variants(
        &mut self,
        db: &Db,
        enum_: EnumId,
        enum_import_info: &ImportInfo,
    ) {
        for (&variant_name, &(variant, _)) in &enum_.enum_variants(db).variants {
            let attr_id = variant.into();
            let attrs = AttrFlags::query(db, attr_id);
            let do_not_complete = Complete::extract(false, attrs);
            let variant_info = ImportInfo {
                container: enum_import_info.container,
                name: variant_name,
                is_doc_hidden: attrs.contains(AttrFlags::IS_DOC_HIDDEN),
                is_unstable: attrs.contains(AttrFlags::IS_UNSTABLE),
                complete: do_not_complete,
            };
            self.add(ItemInNs::Types(variant.into()), false, variant_info);
        }
    }

    fn collect_trait_assoc_items(
        &mut self,
        db: &Db,
        tr: TraitId,
        is_type_in_ns: bool,
        trait_import_info: &ImportInfo,
    ) {
        for &(assoc_item_name, item) in &TraitItems::query(db, tr).items {
            let module_def_id = match item {
                AssocItemId::FunctionId(f) => ModuleDefId::from(f),
                AssocItemId::ConstId(c) => ModuleDefId::from(c),
                // cannot use associated type aliases directly: need a `<Struct as Trait>::TypeAlias`
                // qualifier, ergo no need to store it for imports in import_map
                AssocItemId::TypeAliasId(_) => {
                    cov_mark::hit!(type_aliases_ignored);
                    continue;
                }
            };
            let assoc_item = if is_type_in_ns {
                ItemInNs::Types(module_def_id)
            } else {
                ItemInNs::Values(module_def_id)
            };

            let attr_id = item.into();
            let attrs = AttrFlags::query(db, attr_id);
            let item_do_not_complete = Complete::extract(false, attrs);
            let do_not_complete =
                Complete::for_trait_item(trait_import_info.complete, item_do_not_complete);
            let assoc_item_info = ImportInfo {
                container: trait_import_info.container,
                name: assoc_item_name,
                is_doc_hidden: attrs.contains(AttrFlags::IS_DOC_HIDDEN),
                is_unstable: attrs.contains(AttrFlags::IS_UNSTABLE),
                complete: do_not_complete,
            };
            self.add(assoc_item, true, assoc_item_info);
        }
    }
}

impl Eq for ImportMap {}
/// The same items with the same infos and flags, in any row order. `importables` and the keys
/// are built from those, so they are not compared.
impl PartialEq for ImportMap {
    fn eq(&self, other: &Self) -> bool {
        self.items.len() == other.items.len()
            && self.items.iter().enumerate().all(|(row, item)| {
                other.items.get_index_of(item).is_some_and(|other_row| {
                    self.is_trait_assoc_item(row) == other.is_trait_assoc_item(other_row)
                        && self.row_infos(row) == other.row_infos(other_row)
                })
            })
    }
}

impl fmt::Debug for ImportMap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut importable_names: Vec<_> = self
            .items
            .iter()
            .enumerate()
            .map(|(row, item)| {
                let l = self.row_infos(row).len();
                match item {
                    // alloc: a debug dump (the edge)
                    ItemInNs::Types(it) => format!("- {it:?} (t) [{l}]",),
                    // alloc: a debug dump (the edge)
                    ItemInNs::Values(it) => format!("- {it:?} (v) [{l}]",),
                    // alloc: a debug dump (the edge)
                    ItemInNs::Macros(it) => format!("- {it:?} (m) [{l}]",),
                }
            })
            // alloc: a debug dump (the edge)
            .collect();

        importable_names.sort();
        f.write_str(&importable_names.join("\n"))
    }
}

/// A way to match import map contents against the search query.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum SearchMode {
    /// Import map entry should strictly match the query string.
    Exact,
    /// Import map entry should contain all letters from the query string,
    /// in the same order, but not necessary adjacent.
    Fuzzy,
    /// Import map entry should match the query string by prefix.
    Prefix,
}

impl SearchMode {
    /// Compares bytes in place: nothing is allocated or case-folded into a copy.
    pub fn check(self, query: &str, case_sensitive: bool, candidate: &str) -> bool {
        match self {
            SearchMode::Exact if case_sensitive => candidate == query,
            SearchMode::Exact => candidate.eq_ignore_ascii_case(query),
            SearchMode::Prefix => {
                // On bytes, so a query ending inside one of the candidate's chars is no match
                // (equal bytes end on a char boundary of both).
                let (query, candidate) = (query.as_bytes(), candidate.as_bytes());
                query.len() <= candidate.len() && {
                    let prefix = &candidate[..query.len()];
                    if case_sensitive {
                        prefix == query
                    } else {
                        prefix.eq_ignore_ascii_case(query)
                    }
                }
            }
            SearchMode::Fuzzy => {
                let mut name = candidate;
                query.chars().all(|query_char| {
                    // The end of the first match of the query char (or, ignoring case, of it or
                    // its ASCII uppercase) in what is left of the name.
                    let end = if query_char.is_ascii() {
                        // An ASCII byte in UTF-8 is always a whole char.
                        let exact = query_char as u8;
                        let upper = exact.to_ascii_uppercase();
                        let bytes = name.as_bytes();
                        let at = if case_sensitive {
                            bytes.iter().position(|&b| b == exact)
                        } else {
                            bytes.iter().position(|&b| b == exact || b == upper)
                        };
                        at.map(|at| at + 1)
                    } else {
                        // `to_ascii_uppercase` leaves a non-ASCII char as it is.
                        name.match_indices(query_char).next().map(|(at, m)| at + m.len())
                    };
                    match end {
                        Some(end) => {
                            name = &name[end..];
                            true
                        }
                        None => false,
                    }
                })
            }
        }
    }
}

/// Three possible ways to search for the name in associated and/or other items.
#[derive(Debug, Clone, Copy)]
pub enum AssocSearchMode {
    /// Search for the name in both associated and other items.
    Include,
    /// Search for the name in other items only.
    Exclude,
    /// Search for the name in the associated items only.
    AssocItemsOnly,
}

#[derive(Debug)]
pub struct Query {
    query: String,
    lowercased: String,
    search_mode: SearchMode,
    assoc_mode: AssocSearchMode,
    case_sensitive: bool,
}

impl Query {
    pub fn new(query: String) -> Self {
        let lowercased = query.to_lowercase();
        Self {
            query,
            lowercased,
            search_mode: SearchMode::Exact,
            assoc_mode: AssocSearchMode::Include,
            case_sensitive: false,
        }
    }

    /// Fuzzy finds items instead of exact matching.
    pub fn fuzzy(self) -> Self {
        Self { search_mode: SearchMode::Fuzzy, ..self }
    }

    pub fn prefix(self) -> Self {
        Self { search_mode: SearchMode::Prefix, ..self }
    }

    pub fn exact(self) -> Self {
        Self { search_mode: SearchMode::Exact, ..self }
    }

    /// Specifies whether we want to include associated items in the result.
    pub fn assoc_search_mode(self, assoc_mode: AssocSearchMode) -> Self {
        Self { assoc_mode, ..self }
    }

    /// Respect casing of the query string when matching.
    pub fn case_sensitive(self) -> Self {
        Self { case_sensitive: true, ..self }
    }

    fn matches_assoc_mode(&self, is_trait_assoc_item: IsTraitAssocItem) -> bool {
        !matches!(
            (is_trait_assoc_item, self.assoc_mode),
            (IsTraitAssocItem::Yes, AssocSearchMode::Exclude)
                | (IsTraitAssocItem::No, AssocSearchMode::AssocItemsOnly)
        )
    }
}

/// Searches dependencies of `krate` for an importable name matching `query`.
///
/// This returns a list of items that could be imported from dependencies of `krate`.
pub fn search_dependencies(
    db: &Db,
    krate: Crate,
    query: &Query,
) -> HashSet<(ItemInNs, Complete)> {
    // alloc: the caller's result, at the edge
    let mut found = HashSet::default();
    search_dependencies_into(db, krate, query, &mut found);
    found
}

/// [`search_dependencies`] into `found`, which it adds to: with a set the caller keeps, a
/// search allocates nothing once the set has grown to its results.
pub fn search_dependencies_into(
    db: &Db,
    krate: Crate,
    query: &Query,
    found: &mut HashSet<(ItemInNs, Complete)>,
) {
    search_dependencies_each(db, krate, query, |item, complete| {
        found.insert((item, complete));
    });
}

/// Calls `f` on each importable name of `krate`'s dependencies that matches `query`, in the
/// order the search finds them (dependencies in order, then each map's runs in key order, each
/// run's infos in order); an item can come more than once.
pub fn search_dependencies_each(
    db: &Db,
    krate: Crate,
    query: &Query,
    mut f: impl FnMut(ItemInNs, Complete),
) {
    let _p = tracing::info_span!("search_dependencies", ?query).entered();
    let lowercased = query.lowercased.as_bytes();
    for dep in krate.data(db).dependencies.iter() {
        let map = ImportMap::of(db, dep.crate_id);
        map.search_runs(query.search_mode, lowercased, |run| {
            for &info_idx in map.run(run) {
                let info = &map.infos[info_idx as usize];
                if !query.search_mode.check(&query.query, query.case_sensitive, info.name.as_str())
                {
                    continue;
                }
                let row = map.row_of_info(info_idx);
                if query.matches_assoc_mode(map.is_trait_assoc_item(row)) {
                    f(map.items[row], info.complete);
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use base_db::all_crates;
    use expect_test::{Expect, expect};
    use hir_expand::fixture;
    use itertools::Itertools;

    use crate::{ItemContainerId, Lookup, nameres::assoc::TraitItems, test_db::TestDBExt};

    use super::*;

    impl ImportMap {
        fn fmt_for_test(&self, db: &Db) -> String {
            let mut importable_paths: Vec<_> = self
                .items
                .iter()
                .enumerate()
                .flat_map(|(row, item)| self.row_infos(row).iter().map(move |info| (item, info)))
                .map(|(item, info)| {
                    let path = render_path(db, info);
                    let ns = match item {
                        ItemInNs::Types(_) => "t",
                        ItemInNs::Values(_) => "v",
                        ItemInNs::Macros(_) => "m",
                    };
                    format!("- {path} ({ns})")
                })
                .collect();

            importable_paths.sort();
            importable_paths.join("\n")
        }
    }

    fn check_search(
        #[rust_analyzer::rust_fixture] ra_fixture: &str,
        crate_name: &str,
        query: Query,
        expect: Expect,
    ) {
        let db = fixture::db(ra_fixture).into_db();
        let all_crates = all_crates(&db);
        let krate = all_crates
            .iter()
            .copied()
            .find(|&krate| {
                krate
                    .extra_data(&db)
                    .display_name
                    .as_ref()
                    .is_some_and(|it| it.crate_name().as_str() == crate_name)
            })
            .expect("could not find crate");

        let actual = search_dependencies(&db, krate, &query)
            .into_iter()
            .filter_map(|(dependency, _)| {
                let dependency_krate = dependency.krate(&db)?;
                let dependency_imports = ImportMap::of(&db, dependency_krate);

                let (path, mark) = match assoc_item_path(&db, dependency_imports, dependency) {
                    Some(assoc_item_path) => (assoc_item_path, "a"),
                    None => (
                        render_path(&db, &dependency_imports.import_info_for(dependency)?[0]),
                        match dependency {
                            ItemInNs::Types(ModuleDefId::FunctionId(_))
                            | ItemInNs::Values(ModuleDefId::FunctionId(_)) => "f",
                            ItemInNs::Types(_) => "t",
                            ItemInNs::Values(_) => "v",
                            ItemInNs::Macros(_) => "m",
                        },
                    ),
                };

                Some(format!(
                    "{}::{} ({})\n",
                    dependency_krate.extra_data(&db).display_name.as_ref()?,
                    path,
                    mark
                ))
            })
            // HashSet iteration order isn't defined - it's different on
            // x86_64 and i686 at the very least
            .sorted()
            .collect::<String>();
        expect.assert_eq(&actual)
    }

    fn assoc_item_path(
        db: &Db,
        dependency_imports: &ImportMap,
        dependency: ItemInNs,
    ) -> Option<String> {
        let (dependency_assoc_item_id, container) = match dependency.as_module_def_id()? {
            ModuleDefId::FunctionId(id) => (AssocItemId::from(id), id.lookup(db).container),
            ModuleDefId::ConstId(id) => (AssocItemId::from(id), id.lookup(db).container),
            ModuleDefId::TypeAliasId(id) => (AssocItemId::from(id), id.lookup(db).container),
            _ => return None,
        };

        let ItemContainerId::TraitId(trait_id) = container else {
            return None;
        };

        let trait_info = dependency_imports.import_info_for(ItemInNs::Types(trait_id.into()))?;

        let trait_items = TraitItems::query(db, trait_id);
        let (assoc_item_name, _) = trait_items
            .items
            .iter()
            .find(|(_, assoc_item_id)| &dependency_assoc_item_id == assoc_item_id)?;
        // FIXME: This should check all import infos, not just the first
        Some(format!(
            "{}::{}",
            render_path(db, &trait_info[0]),
            assoc_item_name.display(db, Edition::CURRENT)
        ))
    }

    fn check(#[rust_analyzer::rust_fixture] ra_fixture: &str, expect: Expect) {
        let db = fixture::db(ra_fixture).into_db();
        let all_crates = all_crates(&db);

        let actual = all_crates
            .iter()
            .copied()
            .filter_map(|krate| {
                let cdata = &krate.extra_data(&db);
                let name = cdata.display_name.as_ref()?;

                let map = ImportMap::of(&db, krate);

                Some(format!("{name}:\n{}\n", map.fmt_for_test(&db)))
            })
            .sorted()
            .collect::<String>();

        expect.assert_eq(&actual)
    }

    fn render_path(db: &Db, info: &ImportInfo) -> String {
        let mut module = info.container;
        let mut segments = vec![&info.name];

        let def_map = module.def_map(db);
        assert!(def_map.block_id().is_none(), "block local items should not be in `ImportMap`");

        while let Some(parent) = module.containing_module(db) {
            let parent_data = &def_map.module(db, parent);
            let (name, _) = parent_data.children(db).iter().find(|(_, id)| **id == module).unwrap();
            segments.push(name);
            module = parent;
        }

        segments.iter().rev().map(|it| it.display(db, Edition::CURRENT)).join("::")
    }

    #[test]
    fn smoke() {
        check(
            r"
            //- /main.rs crate:main deps:lib

            mod private {
                pub use lib::Pub;
                pub struct InPrivateModule;
            }

            pub mod publ1 {
                use lib::Pub;
            }

            pub mod real_pub {
                pub use lib::Pub;
            }
            pub mod real_pu2 { // same path length as above
                pub use lib::Pub;
            }

            //- /lib.rs crate:lib
            pub struct Pub {}
            pub struct Pub2; // t + v
            struct Priv;
        ",
            expect![[r#"
                lib:
                - Pub (t)
                - Pub2 (t)
                - Pub2 (v)
                main:
                - publ1 (t)
                - real_pu2 (t)
                - real_pu2::Pub (t)
                - real_pub (t)
                - real_pub::Pub (t)
            "#]],
        );
    }

    #[test]
    fn prefers_shortest_path() {
        check(
            r"
            //- /main.rs crate:main

            pub mod sub {
                pub mod subsub {
                    pub struct Def {}
                }

                pub use super::sub::subsub::Def;
            }
        ",
            expect![[r#"
                main:
                - sub (t)
                - sub::Def (t)
                - sub::subsub (t)
                - sub::subsub::Def (t)
            "#]],
        );
    }

    #[test]
    fn type_reexport_cross_crate() {
        // Reexports need to be visible from a crate, even if the original crate exports the item
        // at a shorter path.
        check(
            r"
            //- /main.rs crate:main deps:lib
            pub mod m {
                pub use lib::S;
            }
            //- /lib.rs crate:lib
            pub struct S;
        ",
            expect![[r#"
                lib:
                - S (t)
                - S (v)
                main:
                - m (t)
                - m::S (t)
                - m::S (v)
            "#]],
        );
    }

    #[test]
    fn macro_reexport() {
        check(
            r"
            //- /main.rs crate:main deps:lib
            pub mod m {
                pub use lib::pub_macro;
            }
            //- /lib.rs crate:lib
            #[macro_export]
            macro_rules! pub_macro {
                () => {};
            }
        ",
            expect![[r#"
                lib:
                - pub_macro (m)
                main:
                - m (t)
                - m::pub_macro (m)
            "#]],
        );
    }

    #[test]
    fn module_reexport() {
        // Reexporting modules from a dependency adds all contents to the import map.
        // XXX: The rendered paths are relative to the defining crate.
        check(
            r"
            //- /main.rs crate:main deps:lib
            pub use lib::module as reexported_module;
            //- /lib.rs crate:lib
            pub mod module {
                pub struct S;
            }
        ",
            expect![[r#"
                lib:
                - module (t)
                - module::S (t)
                - module::S (v)
                main:
                - module::S (t)
                - module::S (v)
                - reexported_module (t)
            "#]],
        );
    }

    #[test]
    fn cyclic_module_reexport() {
        // A cyclic reexport does not hang.
        check(
            r"
            //- /lib.rs crate:lib
            pub mod module {
                pub struct S;
                pub use super::sub::*;
            }

            pub mod sub {
                pub use super::module;
            }
        ",
            expect![[r#"
                lib:
                - module (t)
                - module::S (t)
                - module::S (v)
                - module::module (t)
                - sub (t)
                - sub::module (t)
            "#]],
        );
    }

    #[test]
    fn private_macro() {
        check(
            r"
            //- /lib.rs crate:lib
            macro_rules! private_macro {
                () => {};
            }
        ",
            expect![[r#"
                lib:

            "#]],
        );
    }

    #[test]
    fn namespacing() {
        check(
            r"
            //- /lib.rs crate:lib
            pub struct Thing;     // t + v
            #[macro_export]
            macro_rules! Thing {  // m
                () => {};
            }
        ",
            expect![[r#"
                lib:
                - Thing (m)
                - Thing (t)
                - Thing (v)
            "#]],
        );

        check(
            r"
            //- /lib.rs crate:lib
            pub mod Thing {}      // t
            #[macro_export]
            macro_rules! Thing {  // m
                () => {};
            }
        ",
            expect![[r#"
                lib:
                - Thing (m)
                - Thing (t)
            "#]],
        );
    }

    #[test]
    fn fuzzy_import_trait_and_assoc_items() {
        cov_mark::check!(type_aliases_ignored);
        let ra_fixture = r#"
        //- /main.rs crate:main deps:dep
        //- /dep.rs crate:dep
        pub mod fmt {
            pub trait Display {
                type FmtTypeAlias;
                const FMT_CONST: bool;

                fn format_function();
                fn format_method(&self);
            }
        }
    "#;

        check_search(
            ra_fixture,
            "main",
            Query::new("fmt".to_owned()).fuzzy(),
            expect![[r#"
                dep::fmt (t)
                dep::fmt::Display::FMT_CONST (a)
                dep::fmt::Display::format_function (a)
                dep::fmt::Display::format_method (a)
            "#]],
        );
    }

    #[test]
    fn assoc_items_filtering() {
        let ra_fixture = r#"
        //- /main.rs crate:main deps:dep
        //- /dep.rs crate:dep
        pub mod fmt {
            pub trait Display {
                type FmtTypeAlias;
                const FMT_CONST: bool;

                fn format_function();
                fn format_method(&self);
            }
        }
    "#;

        check_search(
            ra_fixture,
            "main",
            Query::new("fmt".to_owned()).fuzzy().assoc_search_mode(AssocSearchMode::AssocItemsOnly),
            expect![[r#"
                dep::fmt::Display::FMT_CONST (a)
                dep::fmt::Display::format_function (a)
                dep::fmt::Display::format_method (a)
            "#]],
        );

        check_search(
            ra_fixture,
            "main",
            Query::new("fmt".to_owned()).fuzzy().assoc_search_mode(AssocSearchMode::Exclude),
            expect![[r#"
                dep::fmt (t)
            "#]],
        );
    }

    #[test]
    fn search_mode() {
        let ra_fixture = r#"
//- /main.rs crate:main deps:dep
//- /dep.rs crate:dep deps:tdep
use tdep::fmt as fmt_dep;
pub mod fmt {
    pub trait Display {
        fn fmt();
    }
}
#[macro_export]
macro_rules! Fmt {
    () => {};
}
pub struct Fmt;

pub fn format() {}
pub fn no() {}

//- /tdep.rs crate:tdep
pub mod fmt {
    pub struct NotImportableFromMain;
}
"#;

        check_search(
            ra_fixture,
            "main",
            Query::new("fmt".to_owned()).fuzzy(),
            expect![[r#"
                dep::Fmt (m)
                dep::Fmt (t)
                dep::Fmt (v)
                dep::fmt (t)
                dep::fmt::Display::fmt (a)
                dep::format (f)
            "#]],
        );

        check_search(
            ra_fixture,
            "main",
            Query::new("fmt".to_owned()),
            expect![[r#"
                dep::Fmt (m)
                dep::Fmt (t)
                dep::Fmt (v)
                dep::fmt (t)
                dep::fmt::Display::fmt (a)
            "#]],
        );
    }

    #[test]
    fn name_only() {
        let ra_fixture = r#"
            //- /main.rs crate:main deps:dep
            //- /dep.rs crate:dep deps:tdep
            use tdep::fmt as fmt_dep;
            pub mod fmt {
                pub trait Display {
                    fn fmt();
                }
            }
            #[macro_export]
            macro_rules! Fmt {
                () => {};
            }
            pub struct Fmt;

            pub fn format() {}
            pub fn no() {}

            //- /tdep.rs crate:tdep
            pub mod fmt {
                pub struct NotImportableFromMain;
            }
        "#;

        check_search(
            ra_fixture,
            "main",
            Query::new("fmt".to_owned()),
            expect![[r#"
                dep::Fmt (m)
                dep::Fmt (t)
                dep::Fmt (v)
                dep::fmt (t)
                dep::fmt::Display::fmt (a)
            "#]],
        );
    }

    #[test]
    fn search_casing() {
        let ra_fixture = r#"
            //- /main.rs crate:main deps:dep
            //- /dep.rs crate:dep

            pub struct fmt;
            pub struct FMT;
        "#;

        check_search(
            ra_fixture,
            "main",
            Query::new("FMT".to_owned()),
            expect![[r#"
                dep::FMT (t)
                dep::FMT (v)
                dep::fmt (t)
                dep::fmt (v)
            "#]],
        );

        check_search(
            ra_fixture,
            "main",
            Query::new("FMT".to_owned()).case_sensitive(),
            expect![[r#"
                dep::FMT (t)
                dep::FMT (v)
            "#]],
        );
    }

    #[test]
    fn unicode_fn_name() {
        let ra_fixture = r#"
            //- /main.rs crate:main deps:dep
            //- /dep.rs crate:dep
            pub fn あい() {}
        "#;

        check_search(
            ra_fixture,
            "main",
            Query::new("あ".to_owned()).fuzzy(),
            expect![[r#"
            dep::あい (f)
        "#]],
        );
    }

    #[test]
    fn prefix_query_ending_inside_a_char() {
        // The query's byte length falls inside the candidate's first char.
        assert!(!SearchMode::Prefix.check("a", false, "あい"));
        assert!(!SearchMode::Prefix.check("ab", true, "あい"));
        assert!(SearchMode::Prefix.check("あ", true, "あい"));
        assert!(SearchMode::Prefix.check("fO", false, "Foo"));
    }

    /// The import map as it was before its columns: an `FxIndexMap` from item to a `SmallVec` of
    /// infos, importables as `(item, index in the SmallVec)`, the FST keys lowercased into a
    /// `String` each. The model the differential test compares with.
    mod reference {
        use base_db::{Crate, Db};
        use blaze_hash::HashSet;
        use fst::{Automaton, IntoStreamer, Streamer, raw::IndexedValue};
        use itertools::Itertools;
        use smallvec::SmallVec;

        use super::super::{ImportInfo, IsTraitAssocItem, Query, SearchMode};
        use crate::{
            AdtId, AssocItemId, AttrDefId, Complete, EnumId, FxIndexMap, ModuleDefId, TraitId,
            attrs::AttrFlags,
            item_scope::{ImportOrExternCrate, ItemInNs},
            nameres::{assoc::TraitItems, crate_def_map},
            visibility::Visibility,
        };

        pub(super) type Index =
            FxIndexMap<ItemInNs, (SmallVec<[ImportInfo; 1]>, IsTraitAssocItem)>;

        pub(super) struct OldImportMap {
            pub(super) map: Index,
            pub(super) importables: Vec<(ItemInNs, u32)>,
            pub(super) fst: fst::Map<Vec<u8>>,
        }

        pub(super) fn build(db: &Db, krate: Crate) -> OldImportMap {
            let map = collect(db, krate);
            let mut importables: Vec<_> = map
                .iter()
                .flat_map(|(&item, (info, _))| {
                    info.iter()
                        .enumerate()
                        .map(move |(idx, info)| (item, info.name.as_str(), idx as u32))
                })
                .collect();
            importables.sort_by(|(_, l_info, _), (_, r_info, _)| {
                let lhs_chars = l_info.chars().map(|c| c.to_ascii_lowercase());
                let rhs_chars = r_info.chars().map(|c| c.to_ascii_lowercase());
                lhs_chars.cmp(rhs_chars)
            });
            importables.dedup();

            let mut builder = fst::MapBuilder::memory();
            let mut iter = importables
                .iter()
                .enumerate()
                .dedup_by(|&(_, (_, lhs, _)), &(_, (_, rhs, _))| lhs.eq_ignore_ascii_case(rhs));
            let mut insert = |name: &str, start, end| {
                builder
                    .insert(name.to_ascii_lowercase(), ((start as u64) << 32) | end as u64)
                    .unwrap()
            };
            if let Some((mut last, (_, name, _))) = iter.next() {
                let mut last_name = name;
                for (next, (_, next_name, _)) in iter {
                    insert(last_name, last, next);
                    last = next;
                    last_name = next_name;
                }
                insert(last_name, last, importables.len());
            }
            let importables = importables.into_iter().map(|(item, _, idx)| (item, idx)).collect();
            OldImportMap { map, fst: builder.into_map(), importables }
        }

        fn collect(db: &Db, krate: Crate) -> Index {
            let def_map = crate_def_map(db, krate);
            let mut map = FxIndexMap::default();
            let root = def_map.root_module_id();
            let mut worklist = vec![root];
            let mut visited = HashSet::default();
            while let Some(module) = worklist.pop() {
                if !visited.insert(module) {
                    continue;
                }
                let mod_data = if module.krate(db) == krate {
                    &def_map.module(db, module)
                } else {
                    &module.def_map(db).module(db, module)
                };
                let visible_items = mod_data.scope(db).entries().filter_map(|(name, per_ns)| {
                    let per_ns = per_ns.filter_visibility(|vis| vis == Visibility::Public);
                    if per_ns.is_none() { None } else { Some((name, per_ns)) }
                });
                for (name, per_ns) in visible_items {
                    for (item, import) in per_ns.iter_items() {
                        let attr_id = if let Some(import) = import {
                            match import {
                                ImportOrExternCrate::ExternCrate(id) => Some(id.into()),
                                ImportOrExternCrate::Import(id) => Some(id.use_.into()),
                                ImportOrExternCrate::Glob(id) => Some(id.use_.into()),
                            }
                        } else {
                            match item {
                                ItemInNs::Types(id) | ItemInNs::Values(id) => match id {
                                    ModuleDefId::ModuleId(it) => Some(AttrDefId::ModuleId(it)),
                                    ModuleDefId::FunctionId(it) => Some(it.into()),
                                    ModuleDefId::AdtId(it) => Some(it.into()),
                                    ModuleDefId::EnumVariantId(it) => Some(it.into()),
                                    ModuleDefId::ConstId(it) => Some(it.into()),
                                    ModuleDefId::StaticId(it) => Some(it.into()),
                                    ModuleDefId::TraitId(it) => Some(it.into()),
                                    ModuleDefId::TypeAliasId(it) => Some(it.into()),
                                    ModuleDefId::MacroId(it) => Some(it.into()),
                                    ModuleDefId::BuiltinType(_) => None,
                                },
                                ItemInNs::Macros(id) => Some(id.into()),
                            }
                        };
                        let (is_doc_hidden, is_unstable, do_not_complete) = match attr_id {
                            None => (false, false, Complete::Yes),
                            Some(attr_id) => {
                                let attrs = AttrFlags::query(db, attr_id);
                                let do_not_complete = Complete::extract(
                                    matches!(attr_id, AttrDefId::TraitId(_)),
                                    attrs,
                                );
                                (
                                    attrs.contains(AttrFlags::IS_DOC_HIDDEN),
                                    attrs.contains(AttrFlags::IS_UNSTABLE),
                                    do_not_complete,
                                )
                            }
                        };
                        let import_info = ImportInfo {
                            name: name.clone(),
                            container: module,
                            is_doc_hidden,
                            is_unstable,
                            complete: do_not_complete,
                        };
                        let module_def = item.as_module_def_id();
                        if let Some(ModuleDefId::TraitId(tr)) = module_def {
                            collect_trait_assoc_items(
                                db,
                                &mut map,
                                tr,
                                matches!(item, ItemInNs::Types(_)),
                                &import_info,
                            );
                        } else if let Some(ModuleDefId::AdtId(AdtId::EnumId(enum_))) = module_def
                        {
                            collect_enum_variants(db, &mut map, enum_, &import_info);
                        }
                        let (infos, _) = map
                            .entry(item)
                            .or_insert_with(|| (SmallVec::new(), IsTraitAssocItem::No));
                        infos.push(import_info);
                        if let Some(ModuleDefId::ModuleId(mod_id)) = module_def {
                            worklist.push(mod_id);
                        }
                    }
                }
            }
            map
        }

        fn collect_enum_variants(
            db: &Db,
            map: &mut Index,
            enum_: EnumId,
            enum_import_info: &ImportInfo,
        ) {
            for (variant_name, &(variant, _)) in &enum_.enum_variants(db).variants {
                let attrs = AttrFlags::query(db, variant.into());
                let variant_info = ImportInfo {
                    container: enum_import_info.container,
                    name: variant_name.clone(),
                    is_doc_hidden: attrs.contains(AttrFlags::IS_DOC_HIDDEN),
                    is_unstable: attrs.contains(AttrFlags::IS_UNSTABLE),
                    complete: Complete::extract(false, attrs),
                };
                let (infos, _) = map
                    .entry(ItemInNs::Types(variant.into()))
                    .or_insert_with(|| (SmallVec::new(), IsTraitAssocItem::No));
                infos.push(variant_info);
            }
        }

        fn collect_trait_assoc_items(
            db: &Db,
            map: &mut Index,
            tr: TraitId,
            is_type_in_ns: bool,
            trait_import_info: &ImportInfo,
        ) {
            for &(ref assoc_item_name, item) in &TraitItems::query(db, tr).items {
                let module_def_id = match item {
                    AssocItemId::FunctionId(f) => ModuleDefId::from(f),
                    AssocItemId::ConstId(c) => ModuleDefId::from(c),
                    AssocItemId::TypeAliasId(_) => continue,
                };
                let assoc_item = if is_type_in_ns {
                    ItemInNs::Types(module_def_id)
                } else {
                    ItemInNs::Values(module_def_id)
                };
                let attrs = AttrFlags::query(db, item.into());
                let do_not_complete = Complete::for_trait_item(
                    trait_import_info.complete,
                    Complete::extract(false, attrs),
                );
                let assoc_item_info = ImportInfo {
                    container: trait_import_info.container,
                    name: assoc_item_name.clone(),
                    is_doc_hidden: attrs.contains(AttrFlags::IS_DOC_HIDDEN),
                    is_unstable: attrs.contains(AttrFlags::IS_UNSTABLE),
                    complete: do_not_complete,
                };
                let (infos, _) = map
                    .entry(assoc_item)
                    .or_insert_with(|| (SmallVec::new(), IsTraitAssocItem::Yes));
                infos.push(assoc_item_info);
            }
        }

        /// `SearchMode::check` as it was, slicing `str`s.
        pub(super) fn check(
            mode: SearchMode,
            query: &str,
            case_sensitive: bool,
            candidate: &str,
        ) -> bool {
            match mode {
                SearchMode::Exact if case_sensitive => candidate == query,
                SearchMode::Exact => candidate.eq_ignore_ascii_case(query),
                SearchMode::Prefix => {
                    query.len() <= candidate.len() && {
                        let prefix = &candidate[..query.len()];
                        if case_sensitive {
                            prefix == query
                        } else {
                            prefix.eq_ignore_ascii_case(query)
                        }
                    }
                }
                SearchMode::Fuzzy => {
                    let mut name = candidate;
                    query.chars().all(|query_char| {
                        let m = if case_sensitive {
                            name.match_indices(query_char).next()
                        } else {
                            name.match_indices([query_char, query_char.to_ascii_uppercase()])
                                .next()
                        };
                        match m {
                            Some((index, _)) => {
                                name = name[index..]
                                    .strip_prefix(|_: char| true)
                                    .unwrap_or_default();
                                true
                            }
                            None => false,
                        }
                    })
                }
            }
        }

        pub(super) fn search(
            db: &Db,
            krate: Crate,
            query: &Query,
        ) -> HashSet<(ItemInNs, Complete)> {
            let maps: Vec<_> =
                krate.data(db).dependencies.iter().map(|dep| build(db, dep.crate_id)).collect();
            let mut op = fst::map::OpBuilder::new();
            match query.search_mode {
                SearchMode::Exact => {
                    let automaton = fst::automaton::Str::new(&query.lowercased);
                    for map in &maps {
                        op = op.add(map.fst.search(&automaton));
                    }
                    search_maps(&maps, op.union(), query)
                }
                SearchMode::Fuzzy => {
                    let automaton = fst::automaton::Subsequence::new(&query.lowercased);
                    for map in &maps {
                        op = op.add(map.fst.search(&automaton));
                    }
                    search_maps(&maps, op.union(), query)
                }
                SearchMode::Prefix => {
                    let automaton = fst::automaton::Str::new(&query.lowercased).starts_with();
                    for map in &maps {
                        op = op.add(map.fst.search(&automaton));
                    }
                    search_maps(&maps, op.union(), query)
                }
            }
        }

        /// The matches in the order fst finds them: dependencies in order, each one's keys in
        /// order (its own FST, no union), each key's importables in order, every occurrence.
        pub(super) fn search_each(db: &Db, krate: Crate, query: &Query) -> Vec<(ItemInNs, Complete)> {
            let mut res = Vec::new();
            for dep in krate.data(db).dependencies.iter() {
                let map = build(db, dep.crate_id);
                let lowercased = query.lowercased.as_str();
                match query.search_mode {
                    SearchMode::Exact => {
                        visit_map(&map, fst::automaton::Str::new(lowercased), query, &mut res)
                    }
                    SearchMode::Fuzzy => visit_map(
                        &map,
                        fst::automaton::Subsequence::new(lowercased),
                        query,
                        &mut res,
                    ),
                    SearchMode::Prefix => visit_map(
                        &map,
                        fst::automaton::Str::new(lowercased).starts_with(),
                        query,
                        &mut res,
                    ),
                }
            }
            res
        }

        fn visit_map<A: Automaton>(
            map: &OldImportMap,
            automaton: A,
            query: &Query,
            res: &mut Vec<(ItemInNs, Complete)>,
        ) {
            let OldImportMap { map: index, importables, fst: keys } = map;
            let mut stream = keys.search(automaton).into_stream();
            while let Some((_, value)) = stream.next() {
                let end = (value & 0xFFFF_FFFF) as usize;
                let start = (value >> 32) as usize;
                for &(item, info_idx) in &importables[start..end] {
                    let (import_infos, assoc_mode) = &index[&item];
                    let info = &import_infos[info_idx as usize];
                    if query.matches_assoc_mode(*assoc_mode)
                        && check(
                            query.search_mode,
                            &query.query,
                            query.case_sensitive,
                            info.name.as_str(),
                        )
                    {
                        res.push((item, info.complete));
                    }
                }
            }
        }

        fn search_maps(
            maps: &[OldImportMap],
            mut stream: fst::map::Union<'_>,
            query: &Query,
        ) -> HashSet<(ItemInNs, Complete)> {
            let mut res = HashSet::default();
            while let Some((_, indexed_values)) = stream.next() {
                for &IndexedValue { index, value } in indexed_values {
                    let end = (value & 0xFFFF_FFFF) as usize;
                    let start = (value >> 32) as usize;
                    let OldImportMap { map, importables, .. } = &maps[index];
                    for &(item, info_idx) in &importables[start..end] {
                        let (import_infos, assoc_mode) = &map[&item];
                        let info = &import_infos[info_idx as usize];
                        if query.matches_assoc_mode(*assoc_mode)
                            && check(
                                query.search_mode,
                                &query.query,
                                query.case_sensitive,
                                info.name.as_str(),
                            )
                        {
                            res.insert((item, info.complete));
                        }
                    }
                }
            }
            res
        }
    }

    /// Over seeded random crates (modules, re-exports, renames, globs, enums, traits, macros, a
    /// prelude), the columns give what the old map gave: the same rows in the same order, the
    /// same infos and trait flags per row, the same importables, and the FST's keys and runs in
    /// its order; and `search_dependencies` over the sorted keys finds the same items as the
    /// FST's union for exact, prefix and fuzzy queries, either case, in every associated-item
    /// mode.
    #[test]
    fn differential_against_reference() {
        use crate::find_path::tests::{Shape, gen_fixture};

        for seed in 0..16u64 {
            let shape = Shape { private_modules: seed % 2 == 0, private_imports: seed % 3 != 0 };
            let fixture = gen_fixture(seed, shape);
            let db = fixture::db(&fixture).into_db();
            let mut names = Vec::new();
            for &krate in all_crates(&db).iter() {
                let new = ImportMap::of(&db, krate);
                let old = reference::build(&db, krate);
                assert_eq!(
                    new.items.iter().copied().collect::<Vec<_>>(),
                    old.map.keys().copied().collect::<Vec<_>>(),
                    "seed {seed}\n{fixture}"
                );
                for (row, (&item, (infos, assoc))) in old.map.iter().enumerate() {
                    assert_eq!(new.import_info_for(item), Some(&infos[..]), "seed {seed}, {item:?}");
                    assert_eq!(new.is_trait_assoc_item(row), *assoc, "seed {seed}, {item:?}");
                    names.extend(infos.iter().map(|info| info.name.as_str().to_owned()));
                }
                let importables: Vec<(ItemInNs, u32)> = new
                    .importables
                    .iter()
                    .map(|&info| {
                        let row = new.row_of_info(info);
                        (new.items[row], info - new.info_start[row])
                    })
                    .collect();
                assert_eq!(importables, old.importables, "seed {seed}\n{fixture}");
                {
                    use fst::Streamer;
                    let mut stream = old.fst.stream();
                    let mut run = 0;
                    while let Some((key, value)) = stream.next() {
                        assert_eq!(new.key(run), key, "seed {seed}, run {run}\n{fixture}");
                        let bounds = (u64::from(new.run_start[run]) << 32)
                            | u64::from(new.run_start[run + 1]);
                        assert_eq!(bounds, value, "seed {seed}, run {run}\n{fixture}");
                        run += 1;
                    }
                    assert_eq!(run, new.runs(), "seed {seed}\n{fixture}");
                }
                let rebuilt = blaze_mem::frame::task(blaze_mem::Tag::Nameres, |frame| {
                    ImportMap::build(&db, krate, frame)
                });
                assert!(rebuilt == *new, "seed {seed}");
            }
            names.sort();
            names.dedup();

            let main = *all_crates(&db)
                .iter()
                .find(|krate| {
                    krate
                        .extra_data(&db)
                        .display_name
                        .as_ref()
                        .is_some_and(|it| it.crate_name().as_str() == "main")
                })
                .unwrap();
            let step = names.len().div_ceil(12).max(1);
            for name in names.iter().step_by(step) {
                let head = name.get(..2).unwrap_or(name.as_str()).to_owned();
                let every_other: String = name.chars().step_by(2).collect();
                let queries = || {
                    [
                        Query::new(name.clone()),
                        Query::new(name.to_uppercase()),
                        Query::new(name.clone()).case_sensitive(),
                        Query::new(head.clone()).prefix(),
                        Query::new(head.to_lowercase()).prefix().case_sensitive(),
                        Query::new(every_other.clone()).fuzzy(),
                        Query::new(every_other.to_uppercase()).fuzzy(),
                        Query::new(every_other.clone()).fuzzy().case_sensitive(),
                    ]
                };
                for assoc_mode in [
                    AssocSearchMode::Include,
                    AssocSearchMode::Exclude,
                    AssocSearchMode::AssocItemsOnly,
                ] {
                    for query in queries() {
                        let query = query.assoc_search_mode(assoc_mode);
                        assert_eq!(
                            search_dependencies(&db, main, &query),
                            reference::search(&db, main, &query),
                            "seed {seed}, {query:?}\n{fixture}"
                        );
                        let mut each = Vec::new();
                        search_dependencies_each(&db, main, &query, |item, complete| {
                            each.push((item, complete))
                        });
                        assert_eq!(
                            each,
                            reference::search_each(&db, main, &query),
                            "seed {seed}, {query:?}\n{fixture}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn search_mode_against_reference_check() {
        let mut rng = blaze_ids::rng::SplitMix64::new(7);
        let alphabet = ['a', 'B', 'c', 'D', '_', '1', 'あ', 'é', 'É'];
        let word = |rng: &mut blaze_ids::rng::SplitMix64, len: usize| -> String {
            (0..len).map(|_| alphabet[rng.index(alphabet.len())]).collect()
        };
        for _ in 0..4000 {
            let query_len = rng.index(4);
            let query = word(&mut rng, query_len);
            let candidate_len = rng.index(7);
            let candidate = word(&mut rng, candidate_len);
            for mode in [SearchMode::Exact, SearchMode::Fuzzy] {
                for case_sensitive in [false, true] {
                    assert_eq!(
                        mode.check(&query, case_sensitive, &candidate),
                        reference::check(mode, &query, case_sensitive, &candidate),
                        "{mode:?} {query:?} {candidate:?} {case_sensitive}"
                    );
                }
            }
            // The old prefix check panicked when the query ended inside a char of the
            // candidate; where it did not, the answers agree.
            for case_sensitive in [false, true] {
                if candidate.is_char_boundary(query.len().min(candidate.len())) {
                    assert_eq!(
                        SearchMode::Prefix.check(&query, case_sensitive, &candidate),
                        reference::check(SearchMode::Prefix, &query, case_sensitive, &candidate),
                        "prefix {query:?} {candidate:?} {case_sensitive}"
                    );
                }
            }
        }
    }

    /// `is_subsequence` takes the keys fst's `Subsequence` automaton takes, over random keys
    /// of a small alphabet (with a multibyte char) and every short query of it.
    #[test]
    fn is_subsequence_against_fst() {
        use fst::{IntoStreamer, Streamer};

        let mut rng = blaze_ids::rng::SplitMix64::new(11);
        let alphabet = ["a", "b", "_", "あ"];
        let mut keys: Vec<String> = (0..300)
            .map(|_| (0..rng.index(6)).map(|_| alphabet[rng.index(alphabet.len())]).collect())
            .collect();
        keys.sort();
        keys.dedup();
        let set = fst::Set::from_iter(&keys).unwrap();
        for query_len in 0..3 {
            for at in 0..alphabet.len().pow(query_len) {
                let query: String = (0..query_len)
                    .map(|digit| alphabet[at / alphabet.len().pow(digit) % alphabet.len()])
                    .collect();
                let mut stream =
                    set.search(fst::automaton::Subsequence::new(&query)).into_stream();
                let mut taken = Vec::new();
                while let Some(key) = stream.next() {
                    taken.push(key.to_vec());
                }
                let ours: Vec<Vec<u8>> = keys
                    .iter()
                    .filter(|key| is_subsequence(query.as_bytes(), key.as_bytes()))
                    .map(|key| key.as_bytes().to_vec())
                    .collect();
                assert_eq!(ours, taken, "{query:?}");
            }
        }
    }

    /// With the caller's set grown once, an exact, prefix or fuzzy search of the dependencies
    /// allocates nothing.
    #[test]
    fn warm_search_allocates_nothing() {
        let db = fixture::db(
            r#"
//- /main.rs crate:main deps:dep
//- /dep.rs crate:dep
pub struct Fmt;
pub mod fmt {
    pub trait Display { fn fmt(&self); }
    pub fn format() {}
}
"#,
        )
        .into_db();
        let main = *all_crates(&db)
            .iter()
            .find(|krate| {
                krate
                    .extra_data(&db)
                    .display_name
                    .as_ref()
                    .is_some_and(|it| it.crate_name().as_str() == "main")
            })
            .unwrap();
        let queries = [
            Query::new("fmt".to_owned()),
            Query::new("fo".to_owned()).prefix(),
            Query::new("fmt".to_owned()).fuzzy(),
            Query::new("FMT".to_owned()).fuzzy().case_sensitive(),
        ];
        let mut found = HashSet::default();
        for query in &queries {
            search_dependencies_into(&db, main, query, &mut found);
        }
        for query in &queries {
            found.clear();
            blaze_alloc_count::assert_at_most(0, "a warm import-map search", || {
                search_dependencies_into(&db, main, query, &mut found)
            });
            assert_eq!(found, search_dependencies(&db, main, query), "{query:?}");
        }
    }

    /// `search_dependencies_each` allocates nothing of its own, visits what
    /// `search_dependencies` returns, and visits it in the same order every time, whatever
    /// was searched before.
    #[test]
    fn each_is_free_and_in_a_fixed_order() {
        let db = fixture::db(
            r#"
//- /main.rs crate:main deps:dep,other
//- /dep.rs crate:dep
pub struct Fmt;
pub mod fmt { pub fn format() {} pub use super::Fmt as Formatter; }
//- /other.rs crate:other
pub fn fmt() {}
"#,
        )
        .into_db();
        let main = *all_crates(&db)
            .iter()
            .find(|krate| {
                krate
                    .extra_data(&db)
                    .display_name
                    .as_ref()
                    .is_some_and(|it| it.crate_name().as_str() == "main")
            })
            .unwrap();
        let query = Query::new("f".to_owned()).prefix();
        // alloc: test, room for every visit, so the measure counts only the search
        let mut first = Vec::with_capacity(64);
        search_dependencies_each(&db, main, &query, |item, complete| first.push((item, complete)));
        assert!(first.len() < 64);
        let _ = search_dependencies(&db, main, &Query::new("fmt".to_owned()).fuzzy());
        // alloc: test, as `first`
        let mut again = Vec::with_capacity(64);
        blaze_alloc_count::assert_at_most(0, "a visiting import-map search", || {
            search_dependencies_each(&db, main, &query, |item, complete| {
                again.push((item, complete))
            })
        });
        assert_eq!(first, again);
        let visited: HashSet<_> = first.iter().copied().collect();
        assert_eq!(visited, search_dependencies(&db, main, &query));
    }

    /// A build of a crate's import map, its inputs warm, allocates its nine columns and nothing
    /// else: the collection, the counting sort's cursors, the sort and the keys are the frame's.
    #[test]
    fn build_is_its_columns() {
        let db = fixture::db(
            r#"
//- /main.rs crate:main
pub struct S;
pub enum E { A }
pub trait T { fn m(); }
pub mod m { pub fn f() {} pub use super::S as R; }
"#,
        )
        .into_db();
        let krate = db.fetch_test_crate();
        let memo = ImportMap::of(&db, krate);
        let built = blaze_mem::frame::task(blaze_mem::Tag::Nameres, |frame| {
            blaze_alloc_count::assert_at_most(9, "an import-map build", || {
                ImportMap::build(&db, krate, frame)
            })
        });
        assert!(built == *memo);
    }
}
