//! An algorithm to find a path to refer to a certain item.
//!
//! Candidates are [`Choice`]s: `Copy` handles to a chain of segments in the call's
//! [`FindPathScratch`]. Only the chosen one becomes a [`ModPath`] (one interning per call); a
//! candidate costs one 16-byte segment per name pushed and no allocation once the scratch has
//! grown.

use std::{
    cell::{Cell, RefCell},
    cmp::Ordering,
    iter,
};

use base_db::{Crate, CrateOrigin, Db, LangCrateOrigin};
use blaze_hash::{FixedState, HashSet};
use blaze_query::db::{AsId, FromId};
use hir_expand::{
    Lookup,
    mod_path::{ModPath, PathKind},
    name::{AsName, Name},
};
use intern::sym;
use smallvec::SmallVec;

use crate::{
    ModuleDefId, ModuleIdLt,
    attrs::crate_supports_no_std,
    import_map::ImportMap,
    item_scope::ItemInNs,
    nameres::DefMap,
    visibility::{Visibility, VisibilityExplicitness},
};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Copy)]
pub struct FindPathConfig {
    /// If true, prefer to unconditionally use imports of the `core` and `alloc` crate
    /// over the std.
    pub prefer_no_std: bool,
    /// If true, prefer import paths containing a prelude module.
    pub prefer_prelude: bool,
    /// If true, prefer abs path (starting with `::`) where it is available.
    pub prefer_absolute: bool,
    /// If true, paths containing `#[unstable]` segments may be returned, but only if if there is no
    /// stable path. This does not check, whether the item itself that is being imported is `#[unstable]`.
    pub allow_unstable: bool,
}

/// Find a path that can be used to refer to a certain item. This can depend on
/// *from where* you're referring to the item, hence the `from` parameter.
pub fn find_path(
    db: &Db,
    item: ItemInNs,
    from: ModuleIdLt<'_>,
    prefix_kind: PrefixKind,
    ignore_local_imports: bool,
    cfg: FindPathConfig,
) -> Option<ModPath> {
    // Taken rather than borrowed: a query `find_path` reads may print a path itself, and gets
    // an empty scratch instead of a borrow error.
    let mut scratch = SCRATCH.take();
    let found =
        find_path_with(&mut scratch, db, item, from, prefix_kind, ignore_local_imports, cfg);
    SCRATCH.set(scratch);
    found
}

/// [`find_path`] with the caller's scratch, so that a run of calls (a diagnostic pass printing
/// many paths) allocates only while the scratch grows.
pub fn find_path_with<'db>(
    scratch: &mut FindPathScratch,
    db: &'db Db,
    item: ItemInNs,
    from: ModuleIdLt<'db>,
    mut prefix_kind: PrefixKind,
    ignore_local_imports: bool,
    mut cfg: FindPathConfig,
) -> Option<ModPath> {
    let _p = tracing::info_span!("find_path").entered();

    // - if the item is a builtin, it's in scope
    if let ItemInNs::Types(ModuleDefId::BuiltinType(builtin)) = item {
        return Some(ModPath::from_segments(PathKind::Plain, iter::once(builtin.as_name())));
    }

    // within block modules, forcing a `self` or `crate` prefix will not allow using inner items, so
    // default to plain paths.
    let item_module = item.module(db)?;
    if item_module.block(db).is_some() {
        prefix_kind = PrefixKind::Plain;
    }

    let from_def_map = from.def_map(db);

    let from_crate = from.krate(db);
    cfg.prefer_no_std =
        cfg.prefer_no_std || from_def_map.is_no_std() || crate_supports_no_std(db, from_crate);

    scratch.reset();
    let ctx = FindPathCtx {
        db,
        prefix: prefix_kind,
        cfg,
        ignore_local_imports,
        is_std_item: item_module.krate(db).data(db).origin.is_lang(),
        from,
        from_crate,
        crate_root: from_def_map.crate_root(db),
        from_def_map,
        fuel: Cell::new(FIND_PATH_FUEL),
    };
    let choice = find_path_inner(&ctx, scratch, item, MAX_PATH_LEN)?;
    Some(choice.into_path(scratch))
}

/// What one [`find_path`] call works in: the visited set, the segment arena every candidate
/// points into, and a worklist per depth of the local search. Each thread keeps one, so a call
/// allocates only while it grows.
pub struct FindPathScratch {
    /// `(item, module)` pairs the local searches on the current stack have looked at, the module
    /// as its `blaze_query::db::Id`. Empty between top-level searches: each local search removes what it
    /// added.
    visited: HashSet<(ItemInNs, blaze_query::db::Id)>,
    /// The segments of every candidate of the call, each linked to the one before it, so a
    /// candidate extended by a name shares its prefix instead of copying it.
    segments: Vec<Segment>,
    /// Worklists of [`find_local_import_locations`], one per depth of its recursion.
    worklists: Vec<Vec<(blaze_query::db::Id, bool)>>,
}

impl Default for FindPathScratch {
    fn default() -> Self {
        FindPathScratch::new()
    }
}

thread_local! {
    static SCRATCH: RefCell<FindPathScratch> = const { RefCell::new(FindPathScratch::new()) };
}

impl FindPathScratch {
    pub const fn new() -> FindPathScratch {
        FindPathScratch {
            visited: HashSet::with_hasher(FixedState),
            // alloc: none until the thread's first call grows it, then kept
            segments: Vec::new(),
            // alloc: none until the thread's first call grows it, then kept
            worklists: Vec::new(),
        }
    }

    fn reset(&mut self) {
        self.segments.clear();
        self.clear_visited();
    }

    fn clear_visited(&mut self) {
        if !self.visited.is_empty() {
            self.visited.clear();
        }
    }

    fn push_segment(&mut self, parent: u32, name: Name) -> u32 {
        let at = u32::try_from(self.segments.len()).expect("more than u32::MAX path segments");
        self.segments.push(Segment { name, parent });
        at
    }
}

/// One segment of a candidate path: its name and the index of the segment before it in
/// [`FindPathScratch::segments`] ([`NO_SEGMENT`] for the first). 8 bytes; a path of `n`
/// segments is read back in `n` loads.
#[derive(Clone, Copy)]
struct Segment {
    name: Name,
    parent: u32,
}

const _: () = assert!(size_of::<Segment>() == 8);

const NO_SEGMENT: u32 = u32::MAX;

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Stability {
    Unstable,
    Stable,
}
use Stability::*;

const MAX_PATH_LEN: usize = 15;
const FIND_PATH_FUEL: usize = 10000;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PrefixKind {
    /// Causes paths to always start with either `self`, `super`, `crate` or a crate-name.
    /// This is the same as plain, just that paths will start with `self` prepended if the path
    /// starts with an identifier that is not a crate.
    BySelf,
    /// Causes paths to not use a self, super or crate prefix.
    Plain,
    /// Causes paths to start with `crate` where applicable, effectively forcing paths to be absolute.
    ByCrate,
}

impl PrefixKind {
    #[inline]
    fn path_kind(self) -> PathKind {
        match self {
            PrefixKind::BySelf => PathKind::SELF,
            PrefixKind::Plain => PathKind::Plain,
            PrefixKind::ByCrate => PathKind::Crate,
        }
    }
}

struct FindPathCtx<'db> {
    db: &'db Db,
    prefix: PrefixKind,
    cfg: FindPathConfig,
    ignore_local_imports: bool,
    is_std_item: bool,
    from: ModuleIdLt<'db>,
    from_crate: Crate,
    crate_root: ModuleIdLt<'db>,
    from_def_map: &'db DefMap,
    fuel: Cell<usize>,
}

/// Attempts to find a path to refer to the given `item` visible from the `from` ModuleIdLt<'_>
fn find_path_inner<'db>(
    ctx: &FindPathCtx<'db>,
    st: &mut FindPathScratch,
    item: ItemInNs,
    max_len: usize,
) -> Option<Choice> {
    // - if the item is a module, jump straight to module search
    if !ctx.is_std_item
        && let ItemInNs::Types(ModuleDefId::ModuleId(module_id)) = item
    {
        st.clear_visited();
        return find_path_for_module(ctx, st, module_id, true, max_len);
    }

    let may_be_in_scope = match ctx.prefix {
        PrefixKind::Plain | PrefixKind::BySelf => true,
        PrefixKind::ByCrate => ctx.crate_root == ctx.from,
    };
    if may_be_in_scope {
        // - if the item is already in scope, return the name under which it is
        let scope_name =
            find_in_scope(ctx.db, ctx.from_def_map, ctx.from, item, ctx.ignore_local_imports);
        if let Some(scope_name) = scope_name {
            return Some(Choice::new(st, ctx.prefix.path_kind(), scope_name, Stable));
        }
    }

    // - if the item is in the prelude, return the name from there
    if let Some(value) = find_in_prelude(ctx.db, st, ctx.from_def_map, item, ctx.from) {
        return Some(value);
    }

    if let Some(ModuleDefId::EnumVariantId(variant)) = item.as_module_def_id() {
        // - if the item is an enum variant, refer to it via the enum
        let loc = variant.lookup(ctx.db);
        if let Some(path) = find_path_inner(ctx, st, ItemInNs::Types(loc.parent.into()), max_len)
        {
            return Some(path.push(st, loc.name));
        }
        // If this doesn't work, it seems we have no way of referring to the
        // enum; that's very weird, but there might still be a reexport of the
        // variant somewhere
    }

    let mut best_choice = None;
    st.clear_visited();
    calculate_best_path(ctx, st, item, max_len, &mut best_choice);
    best_choice
}

fn find_path_for_module<'db>(
    ctx: &FindPathCtx<'db>,
    st: &mut FindPathScratch,
    module_id: ModuleIdLt<'db>,
    maybe_extern: bool,
    max_len: usize,
) -> Option<Choice> {
    if max_len == 0 {
        // recursive base case, we can't find a path of length 0
        return None;
    }
    let module_crate_root = module_id.def_map(ctx.db).crate_root(ctx.db);
    if module_crate_root == module_id {
        if !maybe_extern || module_crate_root == ctx.crate_root {
            // - if the item is the crate root, return `crate`
            return Some(Choice::keyword(PathKind::Crate));
        }
        // - otherwise if the item is the crate root of a dependency crate, return the name from the extern prelude

        let root_local_def_map = ctx.crate_root.local_def_map(ctx.db).1;
        // rev here so we prefer looking at renamed extern decls first
        for (name, (def_id, _extern_crate)) in root_local_def_map.extern_prelude().rev() {
            if module_crate_root != def_id {
                continue;
            }
            let name_already_occupied_in_type_ns = ctx
                .from_def_map
                .with_ancestor_maps(ctx.db, ctx.from, &mut |def_map, local_id| {
                    def_map
                        .module(ctx.db, local_id)
                        .scope(ctx.db)
                        .type_(name)
                        .filter(|&(id, _)| id != ModuleDefId::ModuleId(def_id))
                })
                .is_some();
            let kind = if name_already_occupied_in_type_ns {
                cov_mark::hit!(ambiguous_crate_start);
                PathKind::Abs
            } else if ctx.cfg.prefer_absolute {
                PathKind::Abs
            } else {
                PathKind::Plain
            };
            return Some(Choice::new(st, kind, *name, Stable));
        }
    }

    let may_be_in_scope = match ctx.prefix {
        PrefixKind::Plain | PrefixKind::BySelf => true,
        PrefixKind::ByCrate => ctx.crate_root == ctx.from,
    };
    if may_be_in_scope {
        let scope_name = find_in_scope(
            ctx.db,
            ctx.from_def_map,
            ctx.from,
            ItemInNs::Types(module_id.to_static().into()),
            ctx.ignore_local_imports,
        );
        if let Some(scope_name) = scope_name {
            // - if the item is already in scope, return the name under which it is
            return Some(Choice::new(st, ctx.prefix.path_kind(), scope_name, Stable));
        }
    }

    // - if the module can be referenced as self, super or crate, do that
    if let Some(kind) = is_kw_kind_relative_to_from(ctx.db, ctx.from_def_map, module_id, ctx.from)
        && (ctx.prefix != PrefixKind::ByCrate || kind == PathKind::Crate)
    {
        return Some(Choice::keyword(kind));
    }

    // - if the module is in the prelude, return it by that path
    let item = ItemInNs::Types(module_id.to_static().into());
    if let Some(choice) = find_in_prelude(ctx.db, st, ctx.from_def_map, item, ctx.from) {
        return Some(choice);
    }
    let mut best_choice = None;
    if maybe_extern {
        calculate_best_path(ctx, st, item, max_len, &mut best_choice);
    } else {
        calculate_best_path_local(ctx, st, item, max_len, &mut best_choice);
    }
    best_choice
}

fn find_in_scope<'db>(
    db: &'db Db,
    def_map: &DefMap,
    from: ModuleIdLt<'db>,
    item: ItemInNs,
    ignore_local_imports: bool,
) -> Option<Name> {
    // FIXME: We could have multiple applicable names here, but we currently only return the first
    def_map.with_ancestor_maps(db, from, &mut |def_map, local_id| {
        def_map.module(db, local_id).scope(db).names_of(item, |name, _, declared| {
            (declared || !ignore_local_imports).then(|| *name)
        })
    })
}

/// Returns single-segment path (i.e. without any prefix) if `item` is found in prelude and its
/// name doesn't clash in current scope.
fn find_in_prelude(
    db: &Db,
    st: &mut FindPathScratch,
    local_def_map: &DefMap,
    item: ItemInNs,
    from: ModuleIdLt<'_>,
) -> Option<Choice> {
    let (prelude_module, _) = local_def_map.prelude()?;
    let prelude_def_map = prelude_module.def_map(db);
    let prelude_scope = &prelude_def_map.module(db, prelude_module).scope(db);
    let (name, vis, _declared) = prelude_scope.name_of(item)?;
    if !vis.is_visible_from(db, from) {
        return None;
    }

    // Check if the name is in current scope and it points to the same def.
    let found_and_same_def =
        local_def_map.with_ancestor_maps(db, from, &mut |def_map, local_id| {
            let per_ns = def_map.module(db, local_id).scope(db).get(name);
            let same_def = match item {
                ItemInNs::Types(it) => per_ns.take_types()? == it,
                ItemInNs::Values(it) => per_ns.take_values()? == it,
                ItemInNs::Macros(it) => per_ns.take_macros()? == it,
            };
            Some(same_def)
        });

    if found_and_same_def.unwrap_or(true) {
        Some(Choice::new(st, PathKind::Plain, *name, Stable))
    } else {
        None
    }
}

fn is_kw_kind_relative_to_from(
    db: &Db,
    def_map: &DefMap,
    item: ModuleIdLt<'_>,
    from: ModuleIdLt<'_>,
) -> Option<PathKind> {
    if item.krate(db) != from.krate(db) || item.block(db).is_some() || from.block(db).is_some() {
        return None;
    }
    if item == from {
        // - if the item is the module we're in, use `self`
        Some(PathKind::SELF)
    } else if let Some(parent_id) = def_map.module(db, from).parent {
        if item == parent_id {
            // - if the item is the parent module, use `super` (this is not used recursively, since `super::super` is ugly)
            Some(if parent_id == def_map.root { PathKind::Crate } else { PathKind::Super(1) })
        } else {
            None
        }
    } else {
        None
    }
}

fn calculate_best_path<'db>(
    ctx: &FindPathCtx<'db>,
    st: &mut FindPathScratch,
    item: ItemInNs,
    max_len: usize,
    best_choice: &mut Option<Choice>,
) {
    let fuel = ctx.fuel.get();
    if fuel == 0 {
        // we ran out of fuel, so we stop searching here
        tracing::warn!(
            "ran out of fuel while searching for a path for item {item:?} of krate {:?} from krate {:?}",
            item.krate(ctx.db),
            ctx.from_crate
        );
        return;
    }
    ctx.fuel.set(fuel - 1);

    if item.krate(ctx.db) == Some(ctx.from_crate) {
        // Item was defined in the same crate that wants to import it. It cannot be found in any
        // dependency in this case.
        calculate_best_path_local(ctx, st, item, max_len, best_choice)
    } else if ctx.is_std_item {
        // The item we are searching for comes from the sysroot libraries, so skip prefer looking in
        // the sysroot libraries directly.
        // We do need to fallback as the item in question could be re-exported by another crate
        // while not being a transitive dependency of the current crate.
        find_in_sysroot(ctx, st, item, max_len, best_choice)
    } else {
        // Item was defined in some upstream crate. This means that it must be exported from one,
        // too (unless we can't name it at all). It could *also* be (re)exported by the same crate
        // that wants to import it here, but we always prefer to use the external path here.

        for dep in ctx.from_crate.data(ctx.db).dependencies.iter() {
            find_in_dep(ctx, st, item, max_len, best_choice, dep.crate_id);
        }
    }
}

fn find_in_sysroot<'db>(
    ctx: &FindPathCtx<'db>,
    st: &mut FindPathScratch,
    item: ItemInNs,
    max_len: usize,
    best_choice: &mut Option<Choice>,
) {
    let dependencies = &ctx.from_crate.data(ctx.db).dependencies;
    let mut search = |lang, best_choice: &mut _| {
        if let Some(dep) = dependencies.iter().filter(|it| it.is_sysroot()).find(|dep| {
            match dep.crate_id.data(ctx.db).origin {
                CrateOrigin::Lang(l) => l == lang,
                _ => false,
            }
        }) {
            find_in_dep(ctx, st, item, max_len, best_choice, dep.crate_id);
        }
    };
    if ctx.cfg.prefer_no_std {
        search(LangCrateOrigin::Core, best_choice);
        if matches!(best_choice, Some(Choice { stability: Stable, .. })) {
            return;
        }
        search(LangCrateOrigin::Alloc, best_choice);
        if matches!(best_choice, Some(Choice { stability: Stable, .. })) {
            return;
        }
        search(LangCrateOrigin::Std, best_choice);
        if matches!(best_choice, Some(Choice { stability: Stable, .. })) {
            return;
        }
    } else {
        search(LangCrateOrigin::Std, best_choice);
        if matches!(best_choice, Some(Choice { stability: Stable, .. })) {
            return;
        }
        search(LangCrateOrigin::Core, best_choice);
        if matches!(best_choice, Some(Choice { stability: Stable, .. })) {
            return;
        }
        search(LangCrateOrigin::Alloc, best_choice);
        if matches!(best_choice, Some(Choice { stability: Stable, .. })) {
            return;
        }
    }
    dependencies
        .iter()
        .filter(|it| it.is_sysroot())
        .chain(dependencies.iter().filter(|it| !it.is_sysroot()))
        .for_each(|dep| {
            find_in_dep(ctx, st, item, max_len, best_choice, dep.crate_id);
        });
}

fn find_in_dep<'db>(
    ctx: &FindPathCtx<'db>,
    st: &mut FindPathScratch,
    item: ItemInNs,
    max_len: usize,
    best_choice: &mut Option<Choice>,
    dep: Crate,
) {
    let import_map = ImportMap::of(ctx.db, dep);
    let Some(import_info_for) = import_map.import_info_for(item) else {
        return;
    };
    for info in import_info_for {
        if info.is_doc_hidden {
            // the item or import is `#[doc(hidden)]`, so skip it as it is in an external crate
            continue;
        }

        // Determine best path for containing module and append last segment from `info`.
        // FIXME: we should guide this to look up the path locally, or from the same crate again?
        let choice = find_path_for_module(ctx, st, info.container, true, max_len - 1);
        let Some(mut choice) = choice else {
            continue;
        };
        cov_mark::hit!(partially_imported);
        if info.is_unstable {
            if !ctx.cfg.allow_unstable {
                // the item is unstable and we are not allowed to use unstable items
                continue;
            }
            choice.stability = Unstable;
        }

        Choice::try_select(st, best_choice, choice, ctx.cfg.prefer_prelude, info.name);
    }
}

fn calculate_best_path_local<'db>(
    ctx: &FindPathCtx<'db>,
    st: &mut FindPathScratch,
    item: ItemInNs,
    max_len: usize,
    best_choice: &mut Option<Choice>,
) {
    // FIXME: cache the `find_local_import_locations` output?
    find_local_import_locations(ctx, st, item, |st, name, module_id| {
        // The container path may be at most `max_len - 1` segments since we push
        // `name` on top of it.
        if let Some(choice) = find_path_for_module(ctx, st, module_id, false, max_len - 1) {
            Choice::try_select(st, best_choice, choice, ctx.cfg.prefer_prelude, name);
        }
    });
}

/// A candidate path: its kind, its segments as a chain in [`FindPathScratch::segments`], and
/// what the tie-breaks compare. 24 bytes (the `PathKind` is 8: `DollarCrate` holds an id),
/// `Copy`; comparing two is a few integer compares.
#[derive(Clone, Copy, Debug)]
struct Choice {
    kind: PathKind,
    /// The last segment, [`NO_SEGMENT`] when there is none.
    last: u32,
    /// The number of segments.
    segments: u32,
    /// The length in characters of the path
    path_text_len: u32,
    /// The stability of the path
    stability: Stability,
    /// Whether any segment of this path is named `prelude`
    has_prelude_segment: bool,
}

const _: () = assert!(size_of::<Choice>() <= 28);

impl Choice {
    /// A path of `kind` alone: `crate`, `self` or `super`.
    fn keyword(kind: PathKind) -> Self {
        Self {
            kind,
            last: NO_SEGMENT,
            segments: 0,
            path_text_len: path_kind_len(kind),
            stability: Stable,
            has_prelude_segment: false,
        }
    }

    fn new(st: &mut FindPathScratch, kind: PathKind, name: Name, stability: Stability) -> Self {
        Self {
            kind,
            last: st.push_segment(NO_SEGMENT, name),
            segments: 1,
            path_text_len: path_kind_len(kind) + name_len(name),
            stability,
            has_prelude_segment: name == sym::prelude,
        }
    }

    /// [`ModPath::len`] of the path.
    fn len(&self) -> usize {
        self.segments as usize + self.kind.segment_count()
    }

    fn push_segment(&mut self, st: &mut FindPathScratch, name: Name) {
        self.last = st.push_segment(self.last, name);
        self.segments += 1;
    }

    fn push(mut self, st: &mut FindPathScratch, name: Name) -> Self {
        self.path_text_len += name_len(name);
        self.has_prelude_segment |= name == sym::prelude;
        self.push_segment(st, name);
        self
    }

    fn try_select(
        st: &mut FindPathScratch,
        current: &mut Option<Choice>,
        mut other: Choice,
        prefer_prelude: bool,
        name: Name,
    ) {
        let Some(current) = current else {
            *current = Some(other.push(st, name));
            return;
        };
        match other
            .stability
            .cmp(&current.stability)
            .then_with(|| {
                if prefer_prelude {
                    other.has_prelude_segment.cmp(&current.has_prelude_segment)
                } else {
                    current.has_prelude_segment.cmp(&other.has_prelude_segment)
                }
            })
            .then_with(|| current.len().cmp(&(other.len() + 1)))
        {
            Ordering::Less => return,
            Ordering::Equal => {
                other.path_text_len += name_len(name);
                if let Ordering::Less | Ordering::Equal =
                    current.path_text_len.cmp(&other.path_text_len)
                {
                    return;
                }
            }
            Ordering::Greater => {
                other.path_text_len += name_len(name);
            }
        }
        // `has_prelude_segment` stays the container's here, unlike in `push`: which path wins
        // later depends on it.
        other.push_segment(st, name);
        *current = other;
    }

    /// The one `ModPath` of the call: the segments read back from the arena and interned.
    fn into_path(self, st: &FindPathScratch) -> ModPath {
        let mut names = SmallVec::<[Name; 8]>::with_capacity(self.segments as usize);
        let mut at = self.last;
        while at != NO_SEGMENT {
            let segment = st.segments[at as usize];
            names.push(segment.name);
            at = segment.parent;
        }
        names.reverse();
        ModPath::from_segments(self.kind, names)
    }
}

#[inline]
fn name_len(name: Name) -> u32 {
    name.as_str().len() as u32
}

fn path_kind_len(kind: PathKind) -> u32 {
    match kind {
        PathKind::Plain => 0,
        PathKind::Super(0) => 4,
        PathKind::Super(s) => s as u32 * 5,
        PathKind::Crate => 5,
        PathKind::Abs => 2,
        PathKind::DollarCrate(_) => 0,
    }
}

/// Finds locations in `from.krate` from which `item` can be imported by `from`.
fn find_local_import_locations<'db>(
    ctx: &FindPathCtx<'db>,
    st: &mut FindPathScratch,
    item: ItemInNs,
    mut cb: impl FnMut(&mut FindPathScratch, Name, ModuleIdLt<'db>),
) {
    let db = ctx.db;

    // `from` can import anything below `from` with visibility of at least `from`, and anything
    // above `from` with any visibility. That means we do not need to descend into private siblings
    // of `from` (and similar).

    // Compute the initial worklist. We start with all direct child modules of `from` as well as all
    // of its (recursive) parent modules.
    let mut worklist = st.worklists.pop().unwrap_or_default();
    worklist.extend(
        ctx.from_def_map
            .module(db, ctx.from)
            .children(db)
            .values()
            .copied()
            .chain(iter::successors(ctx.from.containing_module(db), |m| m.containing_module(db)))
            .map(|it| (it.as_id(), false)),
    );

    let def_map =
        if ctx.crate_root == ctx.from { ctx.from_def_map } else { ctx.crate_root.def_map(db) };
    let mut block_def_map;
    let mut cursor = 0;

    while let Some(&mut (id, ref mut processed)) = worklist.get_mut(cursor) {
        cursor += 1;
        if !st.visited.insert((item, id)) {
            // already processed this module
            continue;
        }
        *processed = true;
        // Every id in the scratch was made from `db` during this call: the scratch is empty
        // between calls (`reset`, and each local search removes what it added).
        let module = ModuleIdLt::<'db>::from_id(id);
        let data = if module.block(db).is_some() {
            // Re-query the block's DefMap
            block_def_map = module.def_map(db);
            &block_def_map.module(db, module)
        } else {
            // Reuse the root DefMap
            &def_map.module(db, module)
        };

        if let Some((name, vis, declared)) = data.scope(db).name_of(item)
            && vis.is_visible_from(db, ctx.from)
        {
            let is_pub_or_explicit = match vis {
                Visibility::Module(_, VisibilityExplicitness::Explicit) => {
                    cov_mark::hit!(explicit_private_imports);
                    true
                }
                Visibility::Module(_, VisibilityExplicitness::Implicit) => {
                    cov_mark::hit!(discount_private_imports);
                    false
                }
                Visibility::PubCrate(_) => true,
                Visibility::Public => true,
            };

            // Ignore private imports unless they are explicit. these could be used if we are
            // in a submodule of this module, but that's usually not
            // what the user wants; and if this module can import
            // the item and we're a submodule of it, so can we.
            // Also this keeps the cached data smaller.
            if declared || is_pub_or_explicit {
                cb(st, *name, module);
            }
        }

        // Descend into all modules visible from `from`.
        for (module, vis) in data.scope(db).modules_in_scope() {
            if module.krate(db) != ctx.from_crate {
                // We don't need to look at modules from other crates as our item has to be in the
                // current crate
                continue;
            }
            if st.visited.contains(&(item, module.as_id())) {
                continue;
            }

            if vis.is_visible_from(db, ctx.from) {
                worklist.push((module.as_id(), false));
            }
        }
    }
    for &(id, processed) in &worklist {
        if processed {
            st.visited.remove(&(item, id));
        }
    }
    worklist.clear();
    st.worklists.push(worklist);
}

#[cfg(test)]
pub(crate) mod tests {
    use std::cell::LazyCell;

    use blaze_alloc_count::assert_at_most;
    use blaze_ids::rng::SplitMix64;
    use expect_test::{Expect, expect};
    use hir_expand::fixture;
    use itertools::Itertools;
    use span::Edition;
    use stdx::format_to;
    use syntax::view::AstNode;

    use crate::test_db::{TestDB, TestDBExt};

    use super::*;

    /// `code` needs to contain a cursor marker; checks that `find_path` for the
    /// item the `path` refers to returns that same path when called from the
    /// module the cursor is in.
    #[track_caller]
    fn check_found_path_(
        #[rust_analyzer::rust_fixture] ra_fixture: &str,
        path: &str,
        prefer_prelude: bool,
        prefer_absolute: bool,
        prefer_no_std: bool,
        allow_unstable: bool,
        expect: Expect,
    ) {
        let fx = fixture::db(ra_fixture);
        let pos = fx.position();
        let db = fx.into_db();
        let module = db.module_at_position(pos);
        let parsed_path_file =
            syntax::SourceFile::parse(&format!("use {path};"), span::Edition::CURRENT);
        let ast_path =
            parsed_path_file.root().descendants().find_map(syntax::view::Path::cast).unwrap();
        let span_map = LazyCell::new(|| hir_expand::HirFileId::from(pos.file_id).span_map(&db));
        let mod_path =
            ModPath::from_src_view(&db, ast_path, &mut |range| span_map.span_for_range(range).ctx)
                .unwrap();

        let (def_map, local_def_map) = module.local_def_map(&db);
        let resolved = def_map
            .resolve_path(
                local_def_map,
                &db,
                module,
                &mod_path,
                crate::item_scope::BuiltinShadowMode::Module,
                None,
            )
            .0;
        let resolved = resolved
            .take_types()
            .map(ItemInNs::Types)
            .or_else(|| resolved.take_values().map(ItemInNs::Values))
            .expect("path does not resolve to a type or value");

        let mut res = String::new();
        for (prefix, ignore_local_imports) in
            [PrefixKind::Plain, PrefixKind::ByCrate, PrefixKind::BySelf]
                .into_iter()
                .cartesian_product([false, true])
        {
            let found_path = find_path(
                &db,
                resolved,
                module,
                prefix,
                ignore_local_imports,
                FindPathConfig { prefer_no_std, prefer_prelude, prefer_absolute, allow_unstable },
            );
            format_to!(
                res,
                "{:7}(imports {}): {}\n",
                format!("{:?}", prefix),
                if ignore_local_imports { '✖' } else { '✔' },
                found_path.map_or_else(
                    || "<unresolvable>".to_owned(),
                    |it| it.display(&db, Edition::CURRENT).to_string()
                ),
            );
        }
        expect.assert_eq(&res);
    }

    fn check_found_path(
        #[rust_analyzer::rust_fixture] ra_fixture: &str,
        path: &str,
        expect: Expect,
    ) {
        check_found_path_(ra_fixture, path, false, false, false, false, expect);
    }

    fn check_found_path_prelude(
        #[rust_analyzer::rust_fixture] ra_fixture: &str,
        path: &str,
        expect: Expect,
    ) {
        check_found_path_(ra_fixture, path, true, false, false, false, expect);
    }

    fn check_found_path_absolute(
        #[rust_analyzer::rust_fixture] ra_fixture: &str,
        path: &str,
        expect: Expect,
    ) {
        check_found_path_(ra_fixture, path, false, true, false, false, expect);
    }

    fn check_found_path_prefer_no_std(
        #[rust_analyzer::rust_fixture] ra_fixture: &str,
        path: &str,
        expect: Expect,
    ) {
        check_found_path_(ra_fixture, path, false, false, true, false, expect);
    }

    fn check_found_path_prefer_no_std_allow_unstable(
        #[rust_analyzer::rust_fixture] ra_fixture: &str,
        path: &str,
        expect: Expect,
    ) {
        check_found_path_(ra_fixture, path, false, false, true, true, expect);
    }

    #[test]
    fn same_module() {
        check_found_path(
            r#"
struct S;
$0
        "#,
            "S",
            expect![[r#"
                Plain  (imports ✔): S
                Plain  (imports ✖): S
                ByCrate(imports ✔): crate::S
                ByCrate(imports ✖): crate::S
                BySelf (imports ✔): self::S
                BySelf (imports ✖): self::S
            "#]],
        );
    }

    #[test]
    fn enum_variant() {
        check_found_path(
            r#"
enum E { A }
$0
        "#,
            "E::A",
            expect![[r#"
                Plain  (imports ✔): E::A
                Plain  (imports ✖): E::A
                ByCrate(imports ✔): crate::E::A
                ByCrate(imports ✖): crate::E::A
                BySelf (imports ✔): self::E::A
                BySelf (imports ✖): self::E::A
            "#]],
        );
    }

    #[test]
    fn sub_module() {
        check_found_path(
            r#"
mod foo {
    pub struct S;
}
$0
        "#,
            "foo::S",
            expect![[r#"
                Plain  (imports ✔): foo::S
                Plain  (imports ✖): foo::S
                ByCrate(imports ✔): crate::foo::S
                ByCrate(imports ✖): crate::foo::S
                BySelf (imports ✔): self::foo::S
                BySelf (imports ✖): self::foo::S
            "#]],
        );
    }

    #[test]
    fn super_module() {
        check_found_path(
            r#"
//- /main.rs
mod foo;
//- /foo.rs
mod bar;
struct S;
//- /foo/bar.rs
$0
        "#,
            "super::S",
            expect![[r#"
                Plain  (imports ✔): super::S
                Plain  (imports ✖): super::S
                ByCrate(imports ✔): crate::foo::S
                ByCrate(imports ✖): crate::foo::S
                BySelf (imports ✔): super::S
                BySelf (imports ✖): super::S
            "#]],
        );
    }

    #[test]
    fn self_module() {
        check_found_path(
            r#"
//- /main.rs
mod foo;
//- /foo.rs
$0
        "#,
            "self",
            expect![[r#"
                Plain  (imports ✔): self
                Plain  (imports ✖): self
                ByCrate(imports ✔): crate::foo
                ByCrate(imports ✖): crate::foo
                BySelf (imports ✔): self
                BySelf (imports ✖): self
            "#]],
        );
    }

    #[test]
    fn crate_root() {
        check_found_path(
            r#"
//- /main.rs
mod foo;
//- /foo.rs
$0
        "#,
            "crate",
            expect![[r#"
                Plain  (imports ✔): crate
                Plain  (imports ✖): crate
                ByCrate(imports ✔): crate
                ByCrate(imports ✖): crate
                BySelf (imports ✔): crate
                BySelf (imports ✖): crate
            "#]],
        );
    }

    #[test]
    fn same_crate() {
        check_found_path(
            r#"
//- /main.rs
mod foo;
struct S;
//- /foo.rs
$0
        "#,
            "crate::S",
            expect![[r#"
                Plain  (imports ✔): crate::S
                Plain  (imports ✖): crate::S
                ByCrate(imports ✔): crate::S
                ByCrate(imports ✖): crate::S
                BySelf (imports ✔): crate::S
                BySelf (imports ✖): crate::S
            "#]],
        );
    }

    #[test]
    fn different_crate() {
        check_found_path(
            r#"
//- /main.rs crate:main deps:std
$0
//- /std.rs crate:std
pub struct S;
        "#,
            "std::S",
            expect![[r#"
                Plain  (imports ✔): std::S
                Plain  (imports ✖): std::S
                ByCrate(imports ✔): std::S
                ByCrate(imports ✖): std::S
                BySelf (imports ✔): std::S
                BySelf (imports ✖): std::S
            "#]],
        );
    }

    #[test]
    fn different_crate_renamed() {
        check_found_path(
            r#"
//- /main.rs crate:main deps:std
extern crate std as std_renamed;
$0
//- /std.rs crate:std
pub struct S;
        "#,
            "std_renamed::S",
            expect![[r#"
                Plain  (imports ✔): std_renamed::S
                Plain  (imports ✖): std_renamed::S
                ByCrate(imports ✔): std_renamed::S
                ByCrate(imports ✖): std_renamed::S
                BySelf (imports ✔): std_renamed::S
                BySelf (imports ✖): std_renamed::S
            "#]],
        );
    }

    #[test]
    fn partially_imported() {
        cov_mark::check!(partially_imported);
        // Tests that short paths are used even for external items, when parts of the path are
        // already in scope.
        check_found_path(
            r#"
//- /main.rs crate:main deps:syntax

use syntax::ast;
$0

//- /lib.rs crate:syntax
pub mod ast {
    pub enum ModuleItem {
        A, B, C,
    }
}
        "#,
            "syntax::ast::ModuleItem",
            expect![[r#"
                Plain  (imports ✔): ast::ModuleItem
                Plain  (imports ✖): syntax::ast::ModuleItem
                ByCrate(imports ✔): crate::ast::ModuleItem
                ByCrate(imports ✖): syntax::ast::ModuleItem
                BySelf (imports ✔): self::ast::ModuleItem
                BySelf (imports ✖): syntax::ast::ModuleItem
            "#]],
        );

        check_found_path(
            r#"
//- /main.rs crate:main deps:syntax
$0

//- /lib.rs crate:syntax
pub mod ast {
    pub enum ModuleItem {
        A, B, C,
    }
}
        "#,
            "syntax::ast::ModuleItem",
            expect![[r#"
                Plain  (imports ✔): syntax::ast::ModuleItem
                Plain  (imports ✖): syntax::ast::ModuleItem
                ByCrate(imports ✔): syntax::ast::ModuleItem
                ByCrate(imports ✖): syntax::ast::ModuleItem
                BySelf (imports ✔): syntax::ast::ModuleItem
                BySelf (imports ✖): syntax::ast::ModuleItem
            "#]],
        );
    }

    #[test]
    fn partially_imported_with_prefer_absolute() {
        cov_mark::check!(partially_imported);
        // Similar to partially_imported test case above, but with prefer_absolute enabled.
        // Even if the actual imported item is in external crate, if the path to that item
        // is starting from the imported name, then the path should not start from "::".
        // i.e. The first line in the expected output should not start from "::".
        check_found_path_absolute(
            r#"
//- /main.rs crate:main deps:syntax

use syntax::ast;
$0

//- /lib.rs crate:syntax
pub mod ast {
    pub enum ModuleItem {
        A, B, C,
    }
}
        "#,
            "syntax::ast::ModuleItem",
            expect![[r#"
                Plain  (imports ✔): ast::ModuleItem
                Plain  (imports ✖): ::syntax::ast::ModuleItem
                ByCrate(imports ✔): crate::ast::ModuleItem
                ByCrate(imports ✖): ::syntax::ast::ModuleItem
                BySelf (imports ✔): self::ast::ModuleItem
                BySelf (imports ✖): ::syntax::ast::ModuleItem
            "#]],
        );
    }

    #[test]
    fn same_crate_reexport() {
        check_found_path(
            r#"
mod bar {
    mod foo { pub(crate) struct S; }
    pub(crate) use foo::*;
}
$0
        "#,
            "bar::S",
            expect![[r#"
                Plain  (imports ✔): bar::S
                Plain  (imports ✖): bar::S
                ByCrate(imports ✔): crate::bar::S
                ByCrate(imports ✖): crate::bar::S
                BySelf (imports ✔): self::bar::S
                BySelf (imports ✖): self::bar::S
            "#]],
        );
    }

    #[test]
    fn same_crate_reexport_rename() {
        check_found_path(
            r#"
mod bar {
    mod foo { pub(crate) struct S; }
    pub(crate) use foo::S as U;
}
$0
        "#,
            "bar::U",
            expect![[r#"
                Plain  (imports ✔): bar::U
                Plain  (imports ✖): bar::U
                ByCrate(imports ✔): crate::bar::U
                ByCrate(imports ✖): crate::bar::U
                BySelf (imports ✔): self::bar::U
                BySelf (imports ✖): self::bar::U
            "#]],
        );
    }

    #[test]
    fn different_crate_reexport() {
        check_found_path(
            r#"
//- /main.rs crate:main deps:std
$0
//- /std.rs crate:std deps:core
pub use core::S;
//- /core.rs crate:core
pub struct S;
        "#,
            "std::S",
            expect![[r#"
                Plain  (imports ✔): std::S
                Plain  (imports ✖): std::S
                ByCrate(imports ✔): std::S
                ByCrate(imports ✖): std::S
                BySelf (imports ✔): std::S
                BySelf (imports ✖): std::S
            "#]],
        );
    }

    #[test]
    fn prelude() {
        check_found_path(
            r#"
//- /main.rs edition:2018 crate:main deps:std
$0
//- /std.rs crate:std
pub mod prelude {
    pub mod rust_2018 {
        pub struct S;
    }
}
        "#,
            "S",
            expect![[r#"
                Plain  (imports ✔): S
                Plain  (imports ✖): S
                ByCrate(imports ✔): S
                ByCrate(imports ✖): S
                BySelf (imports ✔): S
                BySelf (imports ✖): S
            "#]],
        );
    }

    #[test]
    fn shadowed_prelude() {
        check_found_path(
            r#"
//- /main.rs crate:main deps:std
struct S;
$0
//- /std.rs crate:std
pub mod prelude {
    pub mod rust_2018 {
        pub struct S;
    }
}
"#,
            "std::prelude::rust_2018::S",
            expect![[r#"
                Plain  (imports ✔): std::prelude::rust_2018::S
                Plain  (imports ✖): std::prelude::rust_2018::S
                ByCrate(imports ✔): std::prelude::rust_2018::S
                ByCrate(imports ✖): std::prelude::rust_2018::S
                BySelf (imports ✔): std::prelude::rust_2018::S
                BySelf (imports ✖): std::prelude::rust_2018::S
            "#]],
        );
    }

    #[test]
    fn imported_prelude() {
        check_found_path(
            r#"
//- /main.rs edition:2018 crate:main deps:std
use S;
$0
//- /std.rs crate:std
pub mod prelude {
    pub mod rust_2018 {
        pub struct S;
    }
}
"#,
            "S",
            expect![[r#"
                Plain  (imports ✔): S
                Plain  (imports ✖): S
                ByCrate(imports ✔): crate::S
                ByCrate(imports ✖): S
                BySelf (imports ✔): self::S
                BySelf (imports ✖): S
            "#]],
        );
    }

    #[test]
    fn enum_variant_from_prelude() {
        let code = r#"
//- /main.rs edition:2018 crate:main deps:std
$0
//- /std.rs crate:std
pub mod prelude {
    pub mod rust_2018 {
        pub enum Option<T> { Some(T), None }
        pub use Option::*;
    }
}
        "#;
        check_found_path(
            code,
            "None",
            expect![[r#"
                Plain  (imports ✔): None
                Plain  (imports ✖): None
                ByCrate(imports ✔): None
                ByCrate(imports ✖): None
                BySelf (imports ✔): None
                BySelf (imports ✖): None
            "#]],
        );
        check_found_path(
            code,
            "Some",
            expect![[r#"
                Plain  (imports ✔): Some
                Plain  (imports ✖): Some
                ByCrate(imports ✔): Some
                ByCrate(imports ✖): Some
                BySelf (imports ✔): Some
                BySelf (imports ✖): Some
            "#]],
        );
    }

    #[test]
    fn shortest_path() {
        check_found_path(
            r#"
//- /main.rs
pub mod foo;
pub mod baz;
struct S;
$0
//- /foo.rs
pub mod bar { pub struct S; }
//- /baz.rs
pub use crate::foo::bar::S;
        "#,
            "baz::S",
            expect![[r#"
                Plain  (imports ✔): baz::S
                Plain  (imports ✖): baz::S
                ByCrate(imports ✔): crate::baz::S
                ByCrate(imports ✖): crate::baz::S
                BySelf (imports ✔): self::baz::S
                BySelf (imports ✖): self::baz::S
            "#]],
        );
    }

    #[test]
    fn discount_private_imports() {
        cov_mark::check!(discount_private_imports);
        check_found_path(
            r#"
//- /main.rs
mod foo;
pub mod bar { pub struct S; }
use bar::S;
//- /foo.rs
$0
        "#,
            // crate::S would be shorter, but using private imports seems wrong
            "crate::bar::S",
            expect![[r#"
                Plain  (imports ✔): crate::bar::S
                Plain  (imports ✖): crate::bar::S
                ByCrate(imports ✔): crate::bar::S
                ByCrate(imports ✖): crate::bar::S
                BySelf (imports ✔): crate::bar::S
                BySelf (imports ✖): crate::bar::S
            "#]],
        );
    }

    #[test]
    fn explicit_private_imports_crate() {
        check_found_path(
            r#"
//- /main.rs
mod foo;
pub mod bar { pub struct S; }
pub(crate) use bar::S;
//- /foo.rs
$0
        "#,
            "crate::S",
            expect![[r#"
                Plain  (imports ✔): crate::S
                Plain  (imports ✖): crate::S
                ByCrate(imports ✔): crate::S
                ByCrate(imports ✖): crate::S
                BySelf (imports ✔): crate::S
                BySelf (imports ✖): crate::S
            "#]],
        );
    }

    #[test]
    fn explicit_private_imports() {
        cov_mark::check!(explicit_private_imports);
        check_found_path(
            r#"
//- /main.rs
pub mod bar {
    mod foo;
    pub mod baz { pub struct S; }
    pub(self) use baz::S;
}

//- /bar/foo.rs
$0
        "#,
            "super::S",
            expect![[r#"
                Plain  (imports ✔): super::S
                Plain  (imports ✖): super::S
                ByCrate(imports ✔): crate::bar::S
                ByCrate(imports ✖): crate::bar::S
                BySelf (imports ✔): super::S
                BySelf (imports ✖): super::S
            "#]],
        );
    }

    #[test]
    fn import_cycle() {
        check_found_path(
            r#"
//- /main.rs
pub mod foo;
pub mod bar;
pub mod baz;
//- /bar.rs
$0
//- /foo.rs
pub use super::baz;
pub struct S;
//- /baz.rs
pub use super::foo;
        "#,
            "crate::foo::S",
            expect![[r#"
                Plain  (imports ✔): crate::foo::S
                Plain  (imports ✖): crate::foo::S
                ByCrate(imports ✔): crate::foo::S
                ByCrate(imports ✖): crate::foo::S
                BySelf (imports ✔): crate::foo::S
                BySelf (imports ✖): crate::foo::S
            "#]],
        );
    }

    #[test]
    fn prefer_std_paths_over_alloc() {
        check_found_path(
            r#"
//- /main.rs crate:main deps:alloc,std
$0

//- /std.rs crate:std deps:alloc
pub mod sync {
    pub use alloc::sync::Arc;
}

//- /zzz.rs crate:alloc
pub mod sync {
    pub struct Arc;
}
        "#,
            "std::sync::Arc",
            expect![[r#"
                Plain  (imports ✔): std::sync::Arc
                Plain  (imports ✖): std::sync::Arc
                ByCrate(imports ✔): std::sync::Arc
                ByCrate(imports ✖): std::sync::Arc
                BySelf (imports ✔): std::sync::Arc
                BySelf (imports ✖): std::sync::Arc
            "#]],
        );
    }
    #[test]
    fn prefer_core_paths_over_std_for_mod_reexport() {
        check_found_path_prefer_no_std(
            r#"
//- /main.rs crate:main deps:core,std

$0

//- /stdlib.rs crate:std deps:core

pub use core::pin;

//- /corelib.rs crate:core

pub mod pin {
    pub struct Pin;
}
            "#,
            "std::pin::Pin",
            expect![[r#"
                Plain  (imports ✔): core::pin::Pin
                Plain  (imports ✖): core::pin::Pin
                ByCrate(imports ✔): core::pin::Pin
                ByCrate(imports ✖): core::pin::Pin
                BySelf (imports ✔): core::pin::Pin
                BySelf (imports ✖): core::pin::Pin
            "#]],
        );
    }

    #[test]
    fn prefer_core_paths_over_std() {
        check_found_path_prefer_no_std(
            r#"
//- /main.rs crate:main deps:core,std

$0

//- /std.rs crate:std deps:core

pub mod fmt {
    pub use core::fmt::Error;
}

//- /zzz.rs crate:core

pub mod fmt {
    pub struct Error;
}
        "#,
            "core::fmt::Error",
            expect![[r#"
                Plain  (imports ✔): core::fmt::Error
                Plain  (imports ✖): core::fmt::Error
                ByCrate(imports ✔): core::fmt::Error
                ByCrate(imports ✖): core::fmt::Error
                BySelf (imports ✔): core::fmt::Error
                BySelf (imports ✖): core::fmt::Error
            "#]],
        );
        check_found_path(
            r#"
//- /main.rs crate:main deps:core,std
#![no_std]

$0

//- /std.rs crate:std deps:core

pub mod fmt {
    pub use core::fmt::Error;
}

//- /zzz.rs crate:core

pub mod fmt {
    pub struct Error;
}
        "#,
            "core::fmt::Error",
            expect![[r#"
                Plain  (imports ✔): core::fmt::Error
                Plain  (imports ✖): core::fmt::Error
                ByCrate(imports ✔): core::fmt::Error
                ByCrate(imports ✖): core::fmt::Error
                BySelf (imports ✔): core::fmt::Error
                BySelf (imports ✖): core::fmt::Error
            "#]],
        );

        // Should also work (on a best-effort basis) if `no_std` is conditionally enabled.
        check_found_path(
            r#"
//- /main.rs crate:main deps:core,std
#![cfg_attr(not(test), no_std)]

$0

//- /std.rs crate:std deps:core

pub mod fmt {
    pub use core::fmt::Error;
}

//- /zzz.rs crate:core

pub mod fmt {
    pub struct Error;
}
        "#,
            "core::fmt::Error",
            expect![[r#"
                Plain  (imports ✔): core::fmt::Error
                Plain  (imports ✖): core::fmt::Error
                ByCrate(imports ✔): core::fmt::Error
                ByCrate(imports ✖): core::fmt::Error
                BySelf (imports ✔): core::fmt::Error
                BySelf (imports ✖): core::fmt::Error
            "#]],
        );

        // Should also work (on a best-effort basis) if `no_std` is conditionally disabled.
        check_found_path(
            r#"
//- /main.rs crate:main deps:core,std cfg:test
#![cfg_attr(not(test), no_std)]

$0

//- /std.rs crate:std deps:core

pub mod fmt {
    pub use core::fmt::Error;
}

//- /zzz.rs crate:core

pub mod fmt {
    pub struct Error;
}
        "#,
            "core::fmt::Error",
            expect![[r#"
                Plain  (imports ✔): core::fmt::Error
                Plain  (imports ✖): core::fmt::Error
                ByCrate(imports ✔): core::fmt::Error
                ByCrate(imports ✖): core::fmt::Error
                BySelf (imports ✔): core::fmt::Error
                BySelf (imports ✖): core::fmt::Error
            "#]],
        );
    }

    #[test]
    fn prefer_alloc_paths_over_std() {
        check_found_path(
            r#"
//- /main.rs crate:main deps:alloc,std
#![no_std]

extern crate alloc;

$0

//- /std.rs crate:std deps:alloc

pub mod sync {
    pub use alloc::sync::Arc;
}

//- /zzz.rs crate:alloc

pub mod sync {
    pub struct Arc;
}
            "#,
            "alloc::sync::Arc",
            expect![[r#"
                Plain  (imports ✔): alloc::sync::Arc
                Plain  (imports ✖): alloc::sync::Arc
                ByCrate(imports ✔): alloc::sync::Arc
                ByCrate(imports ✖): alloc::sync::Arc
                BySelf (imports ✔): alloc::sync::Arc
                BySelf (imports ✖): alloc::sync::Arc
            "#]],
        );
    }

    #[test]
    fn prefer_alloc_paths_over_std_with_extern_crate_std() {
        check_found_path(
            r#"
//- /main.rs crate:main deps:alloc,std
#![no_std]

extern crate alloc;

extern crate std;

$0

//- /std.rs crate:std deps:alloc

pub mod sync {
    pub use alloc::sync::Arc;
}

//- /zzz.rs crate:alloc

pub mod sync {
    pub struct Arc;
}
            "#,
            "alloc::sync::Arc",
            expect![[r#"
                Plain  (imports ✔): alloc::sync::Arc
                Plain  (imports ✖): alloc::sync::Arc
                ByCrate(imports ✔): alloc::sync::Arc
                ByCrate(imports ✖): alloc::sync::Arc
                BySelf (imports ✔): alloc::sync::Arc
                BySelf (imports ✖): alloc::sync::Arc
            "#]],
        );
    }

    #[test]
    fn prefer_shorter_paths_if_not_alloc() {
        check_found_path(
            r#"
//- /main.rs crate:main deps:megaalloc,std
$0

//- /std.rs crate:std deps:megaalloc
pub mod sync {
    pub use megaalloc::sync::Arc;
}

//- /zzz.rs crate:megaalloc
pub struct Arc;
            "#,
            "megaalloc::Arc",
            expect![[r#"
                Plain  (imports ✔): megaalloc::Arc
                Plain  (imports ✖): megaalloc::Arc
                ByCrate(imports ✔): megaalloc::Arc
                ByCrate(imports ✖): megaalloc::Arc
                BySelf (imports ✔): megaalloc::Arc
                BySelf (imports ✖): megaalloc::Arc
            "#]],
        );
    }

    #[test]
    fn builtins_are_in_scope() {
        let code = r#"
$0

pub mod primitive {
    pub use u8;
}
        "#;
        check_found_path(
            code,
            "u8",
            expect![[r#"
                Plain  (imports ✔): u8
                Plain  (imports ✖): u8
                ByCrate(imports ✔): u8
                ByCrate(imports ✖): u8
                BySelf (imports ✔): u8
                BySelf (imports ✖): u8
            "#]],
        );
        check_found_path(
            code,
            "u16",
            expect![[r#"
                Plain  (imports ✔): u16
                Plain  (imports ✖): u16
                ByCrate(imports ✔): u16
                ByCrate(imports ✖): u16
                BySelf (imports ✔): u16
                BySelf (imports ✖): u16
            "#]],
        );
    }

    #[test]
    fn inner_items() {
        check_found_path(
            r#"
fn main() {
    struct Inner {}
    $0
}
        "#,
            "Inner",
            expect![[r#"
                Plain  (imports ✔): Inner
                Plain  (imports ✖): Inner
                ByCrate(imports ✔): Inner
                ByCrate(imports ✖): Inner
                BySelf (imports ✔): Inner
                BySelf (imports ✖): Inner
            "#]],
        );
    }

    #[test]
    fn inner_items_from_outer_scope() {
        check_found_path(
            r#"
fn main() {
    struct Struct {}
    {
        $0
    }
}
        "#,
            "Struct",
            expect![[r#"
                Plain  (imports ✔): Struct
                Plain  (imports ✖): Struct
                ByCrate(imports ✔): Struct
                ByCrate(imports ✖): Struct
                BySelf (imports ✔): Struct
                BySelf (imports ✖): Struct
            "#]],
        );
    }

    #[test]
    fn inner_items_from_inner_module() {
        check_found_path(
            r#"
fn main() {
    mod module {
        pub struct Struct {}
    }
    {
        $0
    }
}
        "#,
            "module::Struct",
            expect![[r#"
                Plain  (imports ✔): module::Struct
                Plain  (imports ✖): module::Struct
                ByCrate(imports ✔): module::Struct
                ByCrate(imports ✖): module::Struct
                BySelf (imports ✔): module::Struct
                BySelf (imports ✖): module::Struct
            "#]],
        );
    }

    #[test]
    fn outer_items_with_inner_items_present() {
        check_found_path(
            r#"
mod module {
    pub struct CompleteMe;
}

fn main() {
    fn inner() {}
    $0
}
            "#,
            "module::CompleteMe",
            expect![[r#"
                Plain  (imports ✔): module::CompleteMe
                Plain  (imports ✖): module::CompleteMe
                ByCrate(imports ✔): crate::module::CompleteMe
                ByCrate(imports ✖): crate::module::CompleteMe
                BySelf (imports ✔): self::module::CompleteMe
                BySelf (imports ✖): self::module::CompleteMe
            "#]],
        )
    }

    #[test]
    fn from_inside_module() {
        check_found_path(
            r#"
mod baz {
    pub struct Foo {}
}

mod bar {
    fn bar() {
        $0
    }
}
            "#,
            "crate::baz::Foo",
            expect![[r#"
                Plain  (imports ✔): crate::baz::Foo
                Plain  (imports ✖): crate::baz::Foo
                ByCrate(imports ✔): crate::baz::Foo
                ByCrate(imports ✖): crate::baz::Foo
                BySelf (imports ✔): crate::baz::Foo
                BySelf (imports ✖): crate::baz::Foo
            "#]],
        )
    }

    #[test]
    fn from_inside_module2() {
        check_found_path(
            r#"
mod qux {
    pub mod baz {
        pub struct Foo {}
    }

    mod bar {
        fn bar() {
            $0;
        }
    }
}

            "#,
            "crate::qux::baz::Foo",
            expect![[r#"
                Plain  (imports ✔): super::baz::Foo
                Plain  (imports ✖): super::baz::Foo
                ByCrate(imports ✔): crate::qux::baz::Foo
                ByCrate(imports ✖): crate::qux::baz::Foo
                BySelf (imports ✔): super::baz::Foo
                BySelf (imports ✖): super::baz::Foo
            "#]],
        )
    }

    #[test]
    fn from_inside_module_with_inner_items() {
        check_found_path(
            r#"
mod baz {
    pub struct Foo {}
}

mod bar {
    fn bar() {
        fn inner() {}
        $0
    }
}
            "#,
            "crate::baz::Foo",
            expect![[r#"
                Plain  (imports ✔): crate::baz::Foo
                Plain  (imports ✖): crate::baz::Foo
                ByCrate(imports ✔): crate::baz::Foo
                ByCrate(imports ✖): crate::baz::Foo
                BySelf (imports ✔): crate::baz::Foo
                BySelf (imports ✖): crate::baz::Foo
            "#]],
        )
    }

    #[test]
    fn recursive_pub_mod_reexport() {
        check_found_path(
            r#"
fn main() {
    let _ = 22_i32.as_name$0();
}

pub mod name {
    pub trait AsName {
        fn as_name(&self) -> String;
    }
    impl AsName for i32 {
        fn as_name(&self) -> String {
            format!("Name: {}", self)
        }
    }
    pub use crate::name;
}
"#,
            "name::AsName",
            expect![[r#"
                Plain  (imports ✔): name::AsName
                Plain  (imports ✖): name::AsName
                ByCrate(imports ✔): crate::name::AsName
                ByCrate(imports ✖): crate::name::AsName
                BySelf (imports ✔): self::name::AsName
                BySelf (imports ✖): self::name::AsName
            "#]],
        );
    }

    #[test]
    fn extern_crate() {
        check_found_path(
            r#"
//- /main.rs crate:main deps:dep
$0
//- /dep.rs crate:dep
"#,
            "dep",
            expect![[r#"
                Plain  (imports ✔): dep
                Plain  (imports ✖): dep
                ByCrate(imports ✔): dep
                ByCrate(imports ✖): dep
                BySelf (imports ✔): dep
                BySelf (imports ✖): dep
            "#]],
        );

        check_found_path(
            r#"
//- /main.rs crate:main deps:dep
fn f() {
    fn inner() {}
    $0
}
//- /dep.rs crate:dep
"#,
            "dep",
            expect![[r#"
                Plain  (imports ✔): dep
                Plain  (imports ✖): dep
                ByCrate(imports ✔): dep
                ByCrate(imports ✖): dep
                BySelf (imports ✔): dep
                BySelf (imports ✖): dep
            "#]],
        );
    }

    #[test]
    fn prelude_with_inner_items() {
        check_found_path(
            r#"
//- /main.rs edition:2018 crate:main deps:std
fn f() {
    fn inner() {}
    $0
}
//- /std.rs crate:std
pub mod prelude {
    pub mod rust_2018 {
        pub enum Option { None }
        pub use Option::*;
    }
}
        "#,
            "None",
            expect![[r#"
                Plain  (imports ✔): None
                Plain  (imports ✖): None
                ByCrate(imports ✔): None
                ByCrate(imports ✖): None
                BySelf (imports ✔): None
                BySelf (imports ✖): None
            "#]],
        );
    }

    #[test]
    fn different_crate_renamed_through_dep() {
        check_found_path(
            r#"
//- /main.rs crate:main deps:intermediate
$0
//- /intermediate.rs crate:intermediate deps:std
pub extern crate std as std_renamed;
//- /std.rs crate:std
pub struct S;
    "#,
            "intermediate::std_renamed::S",
            expect![[r#"
                Plain  (imports ✔): intermediate::std_renamed::S
                Plain  (imports ✖): intermediate::std_renamed::S
                ByCrate(imports ✔): intermediate::std_renamed::S
                ByCrate(imports ✖): intermediate::std_renamed::S
                BySelf (imports ✔): intermediate::std_renamed::S
                BySelf (imports ✖): intermediate::std_renamed::S
            "#]],
        );
    }

    #[test]
    fn different_crate_doc_hidden() {
        check_found_path(
            r#"
//- /main.rs crate:main deps:intermediate
$0
//- /intermediate.rs crate:intermediate deps:std
#[doc(hidden)]
pub extern crate std;
pub extern crate std as longer;
//- /std.rs crate:std
pub struct S;
    "#,
            "intermediate::longer::S",
            expect![[r#"
                Plain  (imports ✔): intermediate::longer::S
                Plain  (imports ✖): intermediate::longer::S
                ByCrate(imports ✔): intermediate::longer::S
                ByCrate(imports ✖): intermediate::longer::S
                BySelf (imports ✔): intermediate::longer::S
                BySelf (imports ✖): intermediate::longer::S
            "#]],
        );
    }

    #[test]
    fn respect_doc_hidden() {
        check_found_path(
            r#"
//- /main.rs crate:main deps:std,lazy_static
$0
//- /lazy_static.rs crate:lazy_static deps:core
#[doc(hidden)]
pub use core::ops::Deref as __Deref;
//- /std.rs crate:std deps:core
pub use core::ops;
//- /core.rs crate:core
pub mod ops {
    pub trait Deref {}
}
    "#,
            "std::ops::Deref",
            expect![[r#"
                Plain  (imports ✔): std::ops::Deref
                Plain  (imports ✖): std::ops::Deref
                ByCrate(imports ✔): std::ops::Deref
                ByCrate(imports ✖): std::ops::Deref
                BySelf (imports ✔): std::ops::Deref
                BySelf (imports ✖): std::ops::Deref
            "#]],
        );
    }

    #[test]
    fn respect_unstable_modules() {
        check_found_path_prefer_no_std_allow_unstable(
            r#"
//- /main.rs crate:main deps:std,core
extern crate std;
$0
//- /longer.rs crate:std deps:core
pub mod error {
    pub use core::error::Error;
}
//- /core.rs crate:core
pub mod error {
    #![unstable(feature = "error_in_core", issue = "103765")]
    pub trait Error {}
}
"#,
            "std::error::Error",
            expect![[r#"
                Plain  (imports ✔): std::error::Error
                Plain  (imports ✖): std::error::Error
                ByCrate(imports ✔): std::error::Error
                ByCrate(imports ✖): std::error::Error
                BySelf (imports ✔): std::error::Error
                BySelf (imports ✖): std::error::Error
            "#]],
        );
    }

    #[test]
    fn respects_prelude_setting() {
        let ra_fixture = r#"
//- /main.rs crate:main deps:krate
$0
//- /krate.rs crate:krate
pub mod prelude {
    pub use crate::foo::*;
}

pub mod foo {
    pub struct Foo;
}
"#;
        check_found_path(
            ra_fixture,
            "krate::foo::Foo",
            expect![[r#"
                Plain  (imports ✔): krate::foo::Foo
                Plain  (imports ✖): krate::foo::Foo
                ByCrate(imports ✔): krate::foo::Foo
                ByCrate(imports ✖): krate::foo::Foo
                BySelf (imports ✔): krate::foo::Foo
                BySelf (imports ✖): krate::foo::Foo
            "#]],
        );
        check_found_path_prelude(
            ra_fixture,
            "krate::prelude::Foo",
            expect![[r#"
                Plain  (imports ✔): krate::prelude::Foo
                Plain  (imports ✖): krate::prelude::Foo
                ByCrate(imports ✔): krate::prelude::Foo
                ByCrate(imports ✖): krate::prelude::Foo
                BySelf (imports ✔): krate::prelude::Foo
                BySelf (imports ✖): krate::prelude::Foo
            "#]],
        );
    }

    #[test]
    fn avoids_prelude_when_prefer_prelude_false() {
        let ra_fixture = r#"
//- /main.rs crate:main deps:krate
$0
//- /krate.rs crate:krate
pub mod prelude {
    pub use crate::module::sub::*;
}
pub mod module {
    pub mod sub {
        pub struct Foo;
    }
}
"#;
        // krate::prelude::Foo (3 segs) is shorter than krate::module::sub::Foo (4 segs),
        // but prefer_prelude=false should pick the longer canonical path.
        check_found_path(
            ra_fixture,
            "krate::module::sub::Foo",
            expect![[r#"
                Plain  (imports ✔): krate::module::sub::Foo
                Plain  (imports ✖): krate::module::sub::Foo
                ByCrate(imports ✔): krate::module::sub::Foo
                ByCrate(imports ✖): krate::module::sub::Foo
                BySelf (imports ✔): krate::module::sub::Foo
                BySelf (imports ✖): krate::module::sub::Foo
            "#]],
        );
        // prefer_prelude=true should still pick the shorter prelude path.
        check_found_path_prelude(
            ra_fixture,
            "krate::prelude::Foo",
            expect![[r#"
                Plain  (imports ✔): krate::prelude::Foo
                Plain  (imports ✖): krate::prelude::Foo
                ByCrate(imports ✔): krate::prelude::Foo
                ByCrate(imports ✖): krate::prelude::Foo
                BySelf (imports ✔): krate::prelude::Foo
                BySelf (imports ✖): krate::prelude::Foo
            "#]],
        );
    }

    #[test]
    fn respects_absolute_setting() {
        let ra_fixture = r#"
//- /main.rs crate:main deps:krate
$0
//- /krate.rs crate:krate
pub mod foo {
    pub struct Foo;
}
"#;
        check_found_path(
            ra_fixture,
            "krate::foo::Foo",
            expect![[r#"
            Plain  (imports ✔): krate::foo::Foo
            Plain  (imports ✖): krate::foo::Foo
            ByCrate(imports ✔): krate::foo::Foo
            ByCrate(imports ✖): krate::foo::Foo
            BySelf (imports ✔): krate::foo::Foo
            BySelf (imports ✖): krate::foo::Foo
        "#]],
        );

        check_found_path_absolute(
            ra_fixture,
            "krate::foo::Foo",
            expect![[r#"
            Plain  (imports ✔): ::krate::foo::Foo
            Plain  (imports ✖): ::krate::foo::Foo
            ByCrate(imports ✔): ::krate::foo::Foo
            ByCrate(imports ✖): ::krate::foo::Foo
            BySelf (imports ✔): ::krate::foo::Foo
            BySelf (imports ✖): ::krate::foo::Foo
        "#]],
        );
    }

    #[test]
    fn respect_segment_length() {
        check_found_path(
            r#"
//- /main.rs crate:main deps:petgraph
$0
//- /petgraph.rs crate:petgraph
pub mod graph {
    pub use crate::graph_impl::{
        NodeIndex
    };
}

mod graph_impl {
    pub struct NodeIndex<Ix>(Ix);
}

pub mod stable_graph {
    #[doc(no_inline)]
    pub use crate::graph::{NodeIndex};
}

pub mod prelude {
    #[doc(no_inline)]
    pub use crate::graph::{NodeIndex};
}
"#,
            "petgraph::graph::NodeIndex",
            expect![[r#"
                Plain  (imports ✔): petgraph::graph::NodeIndex
                Plain  (imports ✖): petgraph::graph::NodeIndex
                ByCrate(imports ✔): petgraph::graph::NodeIndex
                ByCrate(imports ✖): petgraph::graph::NodeIndex
                BySelf (imports ✔): petgraph::graph::NodeIndex
                BySelf (imports ✖): petgraph::graph::NodeIndex
            "#]],
        );
    }

    #[test]
    fn regression_17271() {
        check_found_path(
            r#"
//- /lib.rs crate:main
mod foo;

//- /foo.rs
mod bar;

pub fn b() {$0}
//- /foo/bar.rs
pub fn c() {}
"#,
            "bar::c",
            expect![[r#"
                Plain  (imports ✔): bar::c
                Plain  (imports ✖): bar::c
                ByCrate(imports ✔): crate::foo::bar::c
                ByCrate(imports ✖): crate::foo::bar::c
                BySelf (imports ✔): self::bar::c
                BySelf (imports ✖): self::bar::c
            "#]],
        );
    }

    #[test]
    fn prefer_long_std_over_short_extern() {
        check_found_path(
            r#"
//- /lib.rs crate:main deps:futures_lite,std,core
$0
//- /futures_lite.rs crate:futures_lite deps:std,core
pub use crate::future::Future;
pub mod future {
    pub use core::future::Future;
}
//- /std.rs crate:std deps:core
pub use core::future;
//- /core.rs crate:core
pub mod future {
    pub trait Future {}
}
"#,
            "core::future::Future",
            expect![[r#"
                Plain  (imports ✔): std::future::Future
                Plain  (imports ✖): std::future::Future
                ByCrate(imports ✔): std::future::Future
                ByCrate(imports ✖): std::future::Future
                BySelf (imports ✔): std::future::Future
                BySelf (imports ✖): std::future::Future
            "#]],
        );
    }

    /// `find_path` as it was before candidates moved into the scratch arena: every candidate a
    /// `ModPath`, a fresh visited set per search. The model the differential tests compare with.
    mod reference {
        use std::{cell::Cell, cmp::Ordering, iter};

        use base_db::{Crate, CrateOrigin, Db, LangCrateOrigin};
        use blaze_hash::HashSet;
        use hir_expand::{
            Lookup,
            mod_path::{ModPath, PathKind},
            name::{AsName, Name},
        };
        use intern::sym;

        use super::super::{
            FIND_PATH_FUEL, FindPathConfig, MAX_PATH_LEN, PrefixKind, Stability, Stability::*,
            find_in_scope, is_kw_kind_relative_to_from,
        };
        use crate::{
            ModuleDefId, ModuleIdLt,
            attrs::crate_supports_no_std,
            import_map::ImportMap,
            item_scope::ItemInNs,
            nameres::DefMap,
            visibility::{Visibility, VisibilityExplicitness},
        };

        pub(super) fn find_path(
            db: &Db,
            item: ItemInNs,
            from: ModuleIdLt<'_>,
            mut prefix_kind: PrefixKind,
            ignore_local_imports: bool,
            mut cfg: FindPathConfig,
        ) -> Option<ModPath> {
            if let ItemInNs::Types(ModuleDefId::BuiltinType(builtin)) = item {
                return Some(ModPath::from_segments(
                    PathKind::Plain,
                    iter::once(builtin.as_name()),
                ));
            }
            let item_module = item.module(db)?;
            if item_module.block(db).is_some() {
                prefix_kind = PrefixKind::Plain;
            }
            let from_def_map = from.def_map(db);
            let from_crate = from.krate(db);
            cfg.prefer_no_std = cfg.prefer_no_std
                || from_def_map.is_no_std()
                || crate_supports_no_std(db, from_crate);
            find_path_inner(
                &FindPathCtx {
                    db,
                    prefix: prefix_kind,
                    cfg,
                    ignore_local_imports,
                    is_std_item: item_module.krate(db).data(db).origin.is_lang(),
                    from,
                    from_crate,
                    crate_root: from_def_map.crate_root(db),
                    from_def_map,
                    fuel: Cell::new(FIND_PATH_FUEL),
                },
                item,
                MAX_PATH_LEN,
            )
        }

        struct FindPathCtx<'db> {
            db: &'db Db,
            prefix: PrefixKind,
            cfg: FindPathConfig,
            ignore_local_imports: bool,
            is_std_item: bool,
            from: ModuleIdLt<'db>,
            from_crate: Crate,
            crate_root: ModuleIdLt<'db>,
            from_def_map: &'db DefMap,
            fuel: Cell<usize>,
        }

        type Visited<'db> = HashSet<(ItemInNs, ModuleIdLt<'db>)>;

        fn find_path_inner(
            ctx: &FindPathCtx<'_>,
            item: ItemInNs,
            max_len: usize,
        ) -> Option<ModPath> {
            if !ctx.is_std_item
                && let ItemInNs::Types(ModuleDefId::ModuleId(module_id)) = item
            {
                return find_path_for_module(
                    ctx,
                    &mut HashSet::default(),
                    module_id,
                    true,
                    max_len,
                )
                .map(|choice| choice.path);
            }
            let may_be_in_scope = match ctx.prefix {
                PrefixKind::Plain | PrefixKind::BySelf => true,
                PrefixKind::ByCrate => ctx.crate_root == ctx.from,
            };
            if may_be_in_scope {
                let scope_name = find_in_scope(
                    ctx.db,
                    ctx.from_def_map,
                    ctx.from,
                    item,
                    ctx.ignore_local_imports,
                );
                if let Some(scope_name) = scope_name {
                    return Some(ModPath::from_segments(
                        ctx.prefix.path_kind(),
                        iter::once(scope_name),
                    ));
                }
            }
            if let Some(value) = find_in_prelude(ctx.db, ctx.from_def_map, item, ctx.from) {
                return Some(value.path);
            }
            if let Some(ModuleDefId::EnumVariantId(variant)) = item.as_module_def_id() {
                let loc = variant.lookup(ctx.db);
                if let Some(mut path) =
                    find_path_inner(ctx, ItemInNs::Types(loc.parent.into()), max_len)
                {
                    path.push_segment(loc.name.clone());
                    return Some(path);
                }
            }
            let mut best_choice = None;
            calculate_best_path(ctx, &mut HashSet::default(), item, max_len, &mut best_choice);
            best_choice.map(|choice| choice.path)
        }

        fn find_path_for_module<'db>(
            ctx: &'db FindPathCtx<'db>,
            visited_modules: &mut Visited<'db>,
            module_id: ModuleIdLt<'db>,
            maybe_extern: bool,
            max_len: usize,
        ) -> Option<Choice> {
            if max_len == 0 {
                return None;
            }
            let module_crate_root = module_id.def_map(ctx.db).crate_root(ctx.db);
            if module_crate_root == module_id {
                if !maybe_extern || module_crate_root == ctx.crate_root {
                    return Some(Choice {
                        path: ModPath::from_segments(PathKind::Crate, None),
                        path_text_len: 5,
                        stability: Stable,
                        has_prelude_segment: false,
                    });
                }
                let root_local_def_map = ctx.crate_root.local_def_map(ctx.db).1;
                for (name, (def_id, _extern_crate)) in root_local_def_map.extern_prelude().rev() {
                    if module_crate_root != def_id {
                        continue;
                    }
                    let name_already_occupied_in_type_ns = ctx
                        .from_def_map
                        .with_ancestor_maps(ctx.db, ctx.from, &mut |def_map, local_id| {
                            def_map
                                .module(ctx.db, local_id)
                                .scope(ctx.db)
                                .type_(name)
                                .filter(|&(id, _)| id != ModuleDefId::ModuleId(def_id))
                        })
                        .is_some();
                    let kind = if name_already_occupied_in_type_ns || ctx.cfg.prefer_absolute {
                        PathKind::Abs
                    } else {
                        PathKind::Plain
                    };
                    return Some(Choice::new(kind, name.clone(), Stable));
                }
            }
            let may_be_in_scope = match ctx.prefix {
                PrefixKind::Plain | PrefixKind::BySelf => true,
                PrefixKind::ByCrate => ctx.crate_root == ctx.from,
            };
            if may_be_in_scope {
                let scope_name = find_in_scope(
                    ctx.db,
                    ctx.from_def_map,
                    ctx.from,
                    ItemInNs::Types(module_id.to_static().into()),
                    ctx.ignore_local_imports,
                );
                if let Some(scope_name) = scope_name {
                    return Some(Choice::new(ctx.prefix.path_kind(), scope_name, Stable));
                }
            }
            if let Some(kind) =
                is_kw_kind_relative_to_from(ctx.db, ctx.from_def_map, module_id, ctx.from)
                && (ctx.prefix != PrefixKind::ByCrate || kind == PathKind::Crate)
            {
                return Some(Choice {
                    path: ModPath::from_segments(kind, None),
                    path_text_len: path_kind_len(kind),
                    stability: Stable,
                    has_prelude_segment: false,
                });
            }
            let item = ItemInNs::Types(module_id.to_static().into());
            if let Some(choice) = find_in_prelude(ctx.db, ctx.from_def_map, item, ctx.from) {
                return Some(choice);
            }
            let mut best_choice = None;
            if maybe_extern {
                calculate_best_path(ctx, visited_modules, item, max_len, &mut best_choice);
            } else {
                calculate_best_path_local(ctx, visited_modules, item, max_len, &mut best_choice);
            }
            best_choice
        }

        fn find_in_prelude(
            db: &Db,
            local_def_map: &DefMap,
            item: ItemInNs,
            from: ModuleIdLt<'_>,
        ) -> Option<Choice> {
            let (prelude_module, _) = local_def_map.prelude()?;
            let prelude_def_map = prelude_module.def_map(db);
            let prelude_scope = &prelude_def_map.module(db, prelude_module).scope(db);
            let (name, vis, _declared) = prelude_scope.name_of(item)?;
            if !vis.is_visible_from(db, from) {
                return None;
            }
            let found_and_same_def =
                local_def_map.with_ancestor_maps(db, from, &mut |def_map, local_id| {
                    let per_ns = def_map.module(db, local_id).scope(db).get(name);
                    let same_def = match item {
                        ItemInNs::Types(it) => per_ns.take_types()? == it,
                        ItemInNs::Values(it) => per_ns.take_values()? == it,
                        ItemInNs::Macros(it) => per_ns.take_macros()? == it,
                    };
                    Some(same_def)
                });
            if found_and_same_def.unwrap_or(true) {
                Some(Choice::new(PathKind::Plain, name.clone(), Stable))
            } else {
                None
            }
        }

        fn calculate_best_path<'db>(
            ctx: &'db FindPathCtx<'db>,
            visited_modules: &mut Visited<'db>,
            item: ItemInNs,
            max_len: usize,
            best_choice: &mut Option<Choice>,
        ) {
            let fuel = ctx.fuel.get();
            if fuel == 0 {
                return;
            }
            ctx.fuel.set(fuel - 1);
            if item.krate(ctx.db) == Some(ctx.from_crate) {
                calculate_best_path_local(ctx, visited_modules, item, max_len, best_choice)
            } else if ctx.is_std_item {
                find_in_sysroot(ctx, visited_modules, item, max_len, best_choice)
            } else {
                ctx.from_crate.data(ctx.db).dependencies.iter().for_each(|dep| {
                    find_in_dep(ctx, visited_modules, item, max_len, best_choice, dep.crate_id)
                });
            }
        }

        fn find_in_sysroot<'db>(
            ctx: &'db FindPathCtx<'db>,
            visited_modules: &mut Visited<'db>,
            item: ItemInNs,
            max_len: usize,
            best_choice: &mut Option<Choice>,
        ) {
            let dependencies = &ctx.from_crate.data(ctx.db).dependencies;
            let mut search = |lang, best_choice: &mut _| {
                if let Some(dep) = dependencies.iter().filter(|it| it.is_sysroot()).find(|dep| {
                    match dep.crate_id.data(ctx.db).origin {
                        CrateOrigin::Lang(l) => l == lang,
                        _ => false,
                    }
                }) {
                    find_in_dep(ctx, visited_modules, item, max_len, best_choice, dep.crate_id);
                }
            };
            let order = if ctx.cfg.prefer_no_std {
                [LangCrateOrigin::Core, LangCrateOrigin::Alloc, LangCrateOrigin::Std]
            } else {
                [LangCrateOrigin::Std, LangCrateOrigin::Core, LangCrateOrigin::Alloc]
            };
            for lang in order {
                search(lang, best_choice);
                if matches!(best_choice, Some(Choice { stability: Stable, .. })) {
                    return;
                }
            }
            dependencies
                .iter()
                .filter(|it| it.is_sysroot())
                .chain(dependencies.iter().filter(|it| !it.is_sysroot()))
                .for_each(|dep| {
                    find_in_dep(ctx, visited_modules, item, max_len, best_choice, dep.crate_id);
                });
        }

        fn find_in_dep<'db>(
            ctx: &'db FindPathCtx<'db>,
            visited_modules: &mut Visited<'db>,
            item: ItemInNs,
            max_len: usize,
            best_choice: &mut Option<Choice>,
            dep: Crate,
        ) {
            let import_map = ImportMap::of(ctx.db, dep);
            let Some(import_info_for) = import_map.import_info_for(item) else {
                return;
            };
            for info in import_info_for {
                if info.is_doc_hidden {
                    continue;
                }
                let choice =
                    find_path_for_module(ctx, visited_modules, info.container, true, max_len - 1);
                let Some(mut choice) = choice else {
                    continue;
                };
                if info.is_unstable {
                    if !ctx.cfg.allow_unstable {
                        continue;
                    }
                    choice.stability = Unstable;
                }
                Choice::try_select(best_choice, choice, ctx.cfg.prefer_prelude, info.name.clone());
            }
        }

        fn calculate_best_path_local<'db>(
            ctx: &'db FindPathCtx<'db>,
            visited_modules: &mut Visited<'db>,
            item: ItemInNs,
            max_len: usize,
            best_choice: &mut Option<Choice>,
        ) {
            find_local_import_locations(
                ctx,
                item,
                visited_modules,
                |visited_modules, name, module_id| {
                    if let Some(choice) =
                        find_path_for_module(ctx, visited_modules, module_id, false, max_len - 1)
                    {
                        Choice::try_select(
                            best_choice,
                            choice,
                            ctx.cfg.prefer_prelude,
                            name.clone(),
                        );
                    }
                },
            );
        }

        #[derive(Debug)]
        struct Choice {
            path: ModPath,
            path_text_len: usize,
            stability: Stability,
            has_prelude_segment: bool,
        }

        impl Choice {
            fn new(kind: PathKind, name: Name, stability: Stability) -> Self {
                Self {
                    path_text_len: path_kind_len(kind) + name.as_str().len(),
                    stability,
                    has_prelude_segment: name == sym::prelude,
                    path: ModPath::from_segments(kind, iter::once(name)),
                }
            }

            fn push(mut self, name: Name) -> Self {
                self.path_text_len += name.as_str().len();
                self.has_prelude_segment |= name == sym::prelude;
                self.path.push_segment(name);
                self
            }

            fn try_select(
                current: &mut Option<Choice>,
                mut other: Choice,
                prefer_prelude: bool,
                name: Name,
            ) {
                let Some(current) = current else {
                    *current = Some(other.push(name));
                    return;
                };
                match other
                    .stability
                    .cmp(&current.stability)
                    .then_with(|| {
                        if prefer_prelude {
                            other.has_prelude_segment.cmp(&current.has_prelude_segment)
                        } else {
                            current.has_prelude_segment.cmp(&other.has_prelude_segment)
                        }
                    })
                    .then_with(|| (current.path.len()).cmp(&(other.path.len() + 1)))
                {
                    Ordering::Less => return,
                    Ordering::Equal => {
                        other.path_text_len += name.as_str().len();
                        if let Ordering::Less | Ordering::Equal =
                            current.path_text_len.cmp(&other.path_text_len)
                        {
                            return;
                        }
                    }
                    Ordering::Greater => {
                        other.path_text_len += name.as_str().len();
                    }
                }
                other.path.push_segment(name);
                *current = other;
            }
        }

        fn path_kind_len(kind: PathKind) -> usize {
            match kind {
                PathKind::Plain => 0,
                PathKind::Super(0) => 4,
                PathKind::Super(s) => s as usize * 5,
                PathKind::Crate => 5,
                PathKind::Abs => 2,
                PathKind::DollarCrate(_) => 0,
            }
        }

        fn find_local_import_locations<'db>(
            ctx: &'db FindPathCtx<'db>,
            item: ItemInNs,
            visited_modules: &mut Visited<'db>,
            mut cb: impl FnMut(&mut Visited<'db>, &Name, ModuleIdLt<'db>),
        ) {
            let db = ctx.db;
            let mut worklist = ctx
                .from_def_map
                .module(db, ctx.from)
                .children(db)
                .values()
                .copied()
                .chain(iter::successors(ctx.from.containing_module(db), |m| {
                    m.containing_module(db)
                }))
                .zip(iter::repeat(false))
                .collect::<Vec<_>>();
            let def_map = if ctx.crate_root == ctx.from {
                ctx.from_def_map
            } else {
                ctx.crate_root.def_map(db)
            };
            let mut block_def_map;
            let mut cursor = 0;
            while let Some(&mut (module, ref mut processed)) = worklist.get_mut(cursor) {
                cursor += 1;
                if !visited_modules.insert((item, module)) {
                    continue;
                }
                *processed = true;
                let data = if module.block(db).is_some() {
                    block_def_map = module.def_map(db);
                    &block_def_map.module(db, module)
                } else {
                    &def_map.module(db, module)
                };
                if let Some((name, vis, declared)) = data.scope(db).name_of(item)
                    && vis.is_visible_from(db, ctx.from)
                {
                    let is_pub_or_explicit = match vis {
                        Visibility::Module(_, VisibilityExplicitness::Explicit) => true,
                        Visibility::Module(_, VisibilityExplicitness::Implicit) => false,
                        Visibility::PubCrate(_) => true,
                        Visibility::Public => true,
                    };
                    if declared || is_pub_or_explicit {
                        cb(visited_modules, name, module);
                    }
                }
                for (module, vis) in data.scope(db).modules_in_scope() {
                    if module.krate(db) != ctx.from.krate(db) {
                        continue;
                    }
                    if visited_modules.contains(&(item, module)) {
                        continue;
                    }
                    if vis.is_visible_from(db, ctx.from) {
                        worklist.push((module, false));
                    }
                }
            }
            worklist.into_iter().filter(|&(_, processed)| processed).for_each(|(module, _)| {
                visited_modules.remove(&(item, module));
            });
        }
    }

    /// What the generator may put in a crate.
    #[derive(Clone, Copy)]
    pub(crate) struct Shape {
        /// Modules may be private (`mod m`), not only `pub mod m`.
        pub(crate) private_modules: bool,
        /// `use` without `pub`: an implicitly private import, which `find_path` discounts.
        pub(crate) private_imports: bool,
    }

    struct GenModule {
        parent: Option<usize>,
        name: String,
        is_pub: bool,
        lines: Vec<String>,
        names: HashSet<String>,
    }

    /// One generated crate: its modules (0 the root) and the items and modules it declares, each
    /// as its path from the crate root.
    pub(crate) struct GenCrate {
        modules: Vec<GenModule>,
        /// Paths of declared items, relative to the crate root (`m1::m3::mS2`).
        pub(crate) items: Vec<String>,
    }

    impl GenCrate {
        fn module_path(&self, module: usize) -> String {
            let mut names = Vec::new();
            let mut at = Some(module);
            while let Some(m) = at {
                if m != 0 {
                    names.push(self.modules[m].name.as_str());
                }
                at = self.modules[m].parent;
            }
            names.reverse();
            names.join("::")
        }

        fn join(prefix: &str, path: &str) -> String {
            if path.is_empty() { prefix.to_owned() } else { format!("{prefix}::{path}") }
        }

        fn render(&self, module: usize, out: &mut String) {
            for line in &self.modules[module].lines {
                out.push_str(line);
                out.push('\n');
            }
            for (child, data) in self.modules.iter().enumerate() {
                if data.parent == Some(module) {
                    let vis = if data.is_pub { "pub " } else { "" };
                    format_to!(out, "{vis}mod {} {{\n", data.name);
                    self.render(child, out);
                    out.push_str("}\n");
                }
            }
        }

        pub(crate) fn source(&self) -> String {
            let mut out = String::new();
            self.render(0, &mut out);
            out
        }
    }

    /// A crate tagged `tag` (its names start with it, so no two crates share one): a random
    /// tree of modules, structs, enums and functions, then `use`s of its own items and modules,
    /// globs among them, and of the items of `deps` (by crate name).
    pub(crate) fn gen_crate(
        rng: &mut SplitMix64,
        tag: char,
        shape: Shape,
        deps: &[(&str, &GenCrate)],
    ) -> GenCrate {
        let root = GenModule {
            parent: None,
            name: String::new(),
            is_pub: true,
            lines: Vec::new(),
            names: HashSet::default(),
        };
        let mut krate = GenCrate { modules: vec![root], items: Vec::new() };
        for i in 0..1 + rng.index(7) {
            let parent = rng.index(krate.modules.len());
            let name = format!("{tag}m{i}");
            krate.modules[parent].names.insert(name.clone());
            krate.modules.push(GenModule {
                parent: Some(parent),
                name,
                is_pub: !shape.private_modules || rng.below(100) < 70,
                lines: Vec::new(),
                names: HashSet::default(),
            });
        }
        for i in 0..2 + rng.index(8) {
            let module = rng.index(krate.modules.len());
            let vis = if rng.below(100) < 80 { "pub " } else { "" };
            // `#[macro_export]` puts a macro at the crate root, whatever module declares it.
            let mut at = module;
            let (name, line) = match rng.index(5) {
                0 => {
                    let name = format!("{tag}S{i}");
                    let line = format!("{vis}struct {name};");
                    (name, line)
                }
                1 => {
                    let name = format!("{tag}E{i}");
                    let line = format!("{vis}enum {name} {{ {tag}V{i}a, {tag}V{i}b }}");
                    (name, line)
                }
                2 => {
                    let name = format!("{tag}T{i}");
                    let line = format!(
                        "{vis}trait {name} {{ fn {tag}t{i}(); const {tag}C{i}: u8; type {tag}Y{i}; }}"
                    );
                    (name, line)
                }
                3 => {
                    at = 0;
                    let name = format!("{tag}mac{i}");
                    let line = format!("#[macro_export]\nmacro_rules! {name} {{ () => {{}} }}");
                    (name, line)
                }
                _ => {
                    let name = format!("{tag}f{i}");
                    let line = format!("{vis}fn {name}() {{}}");
                    (name, line)
                }
            };
            let path = GenCrate::join(&krate.module_path(at), &name);
            krate.modules[at].names.insert(name);
            krate.modules[module].lines.push(line);
            krate.items.push(path);
        }
        let mut alias = 0;
        for _ in 0..rng.index(7) {
            let into = rng.index(krate.modules.len());
            let vis = match rng.index(10) {
                0..=4 => "pub ",
                5 | 6 => "pub(crate) ",
                7 => "pub(self) ",
                _ if shape.private_imports => "",
                _ => "pub ",
            };
            let from_dep = !deps.is_empty() && rng.below(100) < 30;
            let (prefix, source): (&str, &GenCrate) = if from_dep {
                let (name, dep) = deps[rng.index(deps.len())];
                (name, dep)
            } else {
                ("crate", &krate)
            };
            let line = match rng.index(3) {
                0 if !source.items.is_empty() => {
                    let item = &source.items[rng.index(source.items.len())];
                    let last = item.rsplit("::").next().unwrap().to_owned();
                    let path = GenCrate::join(prefix, item);
                    if rng.below(100) < 30 || krate.modules[into].names.contains(&last) {
                        alias += 1;
                        let renamed = format!("{tag}A{alias}");
                        krate.modules[into].names.insert(renamed.clone());
                        format!("{vis}use {path} as {renamed};")
                    } else {
                        krate.modules[into].names.insert(last);
                        format!("{vis}use {path};")
                    }
                }
                1 if source.modules.len() > 1 => {
                    let module = 1 + rng.index(source.modules.len() - 1);
                    let path = GenCrate::join(prefix, &source.module_path(module));
                    alias += 1;
                    let renamed = format!("{tag}A{alias}");
                    krate.modules[into].names.insert(renamed.clone());
                    format!("{vis}use {path} as {renamed};")
                }
                _ => {
                    let module = rng.index(source.modules.len());
                    let path = GenCrate::join(prefix, &source.module_path(module));
                    format!("{vis}use {path}::*;")
                }
            };
            krate.modules[into].lines.push(line);
        }
        krate
    }

    /// A fixture of three crates: `std` (with a 2018 prelude re-exporting some of its items),
    /// `dep` using `std`, and `main` using both.
    pub(crate) fn gen_fixture(seed: u64, shape: Shape) -> String {
        let mut rng = SplitMix64::new(seed);
        let mut std_crate = gen_crate(&mut rng, 's', shape, &[]);
        let mut prelude = String::from("pub mod prelude {\npub mod rust_2018 {\npub struct sP0;\n");
        for item in std_crate.items.iter().filter(|_| rng.below(100) < 40) {
            format_to!(prelude, "pub use crate::{item};\n");
        }
        prelude.push_str("}\n}\n");
        std_crate.modules[0].lines.push(prelude);
        let dep = gen_crate(&mut rng, 'd', shape, &[("std", &std_crate)]);
        let main = gen_crate(&mut rng, 'm', shape, &[("dep", &dep), ("std", &std_crate)]);
        format!(
            "//- /main.rs edition:2018 crate:main deps:dep,std\n{}\
             //- /dep.rs crate:dep deps:std\n{}\
             //- /std.rs crate:std\n{}",
            main.source(),
            dep.source(),
            std_crate.source(),
        )
    }

    fn crate_named(db: &TestDB, name: &str) -> Crate {
        base_db::all_crates(db)
            .iter()
            .copied()
            .find(|krate| {
                krate
                    .extra_data(db)
                    .display_name
                    .as_ref()
                    .is_some_and(|it| it.crate_name().as_str() == name)
            })
            .expect("a crate of the fixture")
    }

    /// Every item the crates of `db` declare, in both namespaces (one of them is usually empty,
    /// which `find_path` must answer `None` for alike), and every enum variant.
    fn declared_items(db: &TestDB) -> Vec<ItemInNs> {
        let mut items = Vec::new();
        for &krate in base_db::all_crates(db).iter() {
            for (_, data) in crate::nameres::crate_def_map(db, krate).modules(db) {
                for def in data.scope(db).declarations() {
                    items.push(ItemInNs::Types(def));
                    items.push(ItemInNs::Values(def));
                    if let ModuleDefId::AdtId(crate::AdtId::EnumId(enum_)) = def {
                        for &(variant, _) in enum_.enum_variants(db).variants.values() {
                            items.push(ItemInNs::Types(variant.into()));
                        }
                    }
                }
            }
        }
        items
    }

    fn resolves_to(db: &TestDB, from: crate::ModuleId, path: &ModPath, item: ItemInNs) -> bool {
        let (def_map, local_def_map) = from.local_def_map(db);
        let (per_ns, unresolved) = def_map.resolve_path(
            local_def_map,
            db,
            from,
            path,
            crate::item_scope::BuiltinShadowMode::Module,
            None,
        );
        unresolved.is_none()
            && match item {
                ItemInNs::Types(def) => per_ns.take_types() == Some(def),
                ItemInNs::Values(def) => per_ns.take_values() == Some(def),
                ItemInNs::Macros(def) => per_ns.take_macros() == Some(def),
            }
    }

    const CONFIGS: [FindPathConfig; 3] = [
        FindPathConfig {
            prefer_no_std: false,
            prefer_prelude: false,
            prefer_absolute: false,
            allow_unstable: false,
        },
        FindPathConfig {
            prefer_no_std: false,
            prefer_prelude: true,
            prefer_absolute: true,
            allow_unstable: false,
        },
        FindPathConfig {
            prefer_no_std: true,
            prefer_prelude: false,
            prefer_absolute: false,
            allow_unstable: true,
        },
    ];

    /// The arena-backed `find_path`, with one scratch reused across every call, gives the same
    /// path as the old one for every item, from every module of `main`, every prefix kind and
    /// config, over seeded random crates with re-exports, renames, globs and a prelude; and
    /// every path it gives resolves back to the item.
    #[test]
    fn differential_against_reference() {
        let shapes = [
            Shape { private_modules: true, private_imports: true },
            Shape { private_modules: false, private_imports: true },
            Shape { private_modules: true, private_imports: false },
        ];
        let mut cases = Vec::new();
        for prefix in [PrefixKind::Plain, PrefixKind::ByCrate, PrefixKind::BySelf] {
            for ignore_local_imports in [false, true] {
                cases.push((prefix, ignore_local_imports, CONFIGS[0]));
            }
        }
        cases.push((PrefixKind::Plain, false, CONFIGS[1]));
        cases.push((PrefixKind::Plain, false, CONFIGS[2]));
        for seed in 0..16u64 {
            let shape = shapes[seed as usize % shapes.len()];
            let fixture = gen_fixture(seed, shape);
            let db = fixture::db(&fixture).into_db();
            let main = crate_named(&db, "main");
            let items = declared_items(&db);
            let mut scratch = FindPathScratch::default();
            let modules: Vec<_> =
                crate::nameres::crate_def_map(&db, main).modules(&db).map(|(id, _)| id).collect();
            for &from in &modules {
                for &item in &items {
                    for &(prefix, ignore_local_imports, cfg) in &cases {
                        let new = find_path_with(
                            &mut scratch,
                            &db,
                            item,
                            from,
                            prefix,
                            ignore_local_imports,
                            cfg,
                        );
                        let old =
                            reference::find_path(&db, item, from, prefix, ignore_local_imports, cfg);
                        assert_eq!(
                            new, old,
                            "seed {seed}, {item:?} from {from:?}, {prefix:?}, \
                             ignore_local_imports {ignore_local_imports}, {cfg:?}\n{fixture}"
                        );
                        if let Some(path) = new {
                            assert!(
                                resolves_to(&db, from, &path, item),
                                "seed {seed}: {} does not resolve to {item:?} from {from:?}\n\
                                 {fixture}",
                                path.display(&db, Edition::CURRENT),
                            );
                        }
                    }
                }
            }
        }
    }

    /// The fewest segments (`ModPath::len`) of a path to `target` from `from`, both of one crate,
    /// found by enumerating paths breadth first with `find_path`'s rules: a path starts at
    /// `crate`, `self`, `super` or a name in `from`'s scope, steps through modules named in a
    /// module's scope visibly from `from` (the first name of a def in a module is the one
    /// counted, and implicitly private imports are not), and ends at such a name of `target`.
    fn brute_force_len(db: &TestDB, from: crate::ModuleId, target: ModuleDefId) -> Option<usize> {
        use std::collections::VecDeque;

        use blaze_hash::HashMap;

        let def_map = from.def_map(db);
        let krate = from.krate(db);
        let counts = |item: &crate::per_ns::TypesItem| {
            item.vis.is_visible_from(db, from)
                && (item.import.is_none()
                    || !matches!(item.vis, Visibility::Module(_, VisibilityExplicitness::Implicit)))
        };
        // The first name of each def in `module`, in scope order.
        let first_names = |module: crate::ModuleId| {
            let mut seen = HashSet::default();
            def_map
                .module(db, module)
                .scope(db)
                .types()
                .filter(move |(_, item)| seen.insert(item.def))
                .map(|(_, item)| item)
                .collect::<Vec<_>>()
        };

        fn reach(
            dist: &mut HashMap<crate::ModuleId, usize>,
            queue: &mut VecDeque<crate::ModuleId>,
            module: crate::ModuleId,
            len: usize,
        ) {
            if let std::collections::hash_map::Entry::Vacant(slot) = dist.entry(module) {
                slot.insert(len);
                queue.push_back(module);
            }
        }

        let root = def_map.crate_root(db);
        if target == ModuleDefId::ModuleId(root) {
            return Some(1);
        }
        let in_scope =
            def_map.module(db, from).scope(db).types().any(|(_, item)| item.def == target);
        if in_scope {
            return Some(1);
        }

        // Breadth first, every edge one segment: `self` (no segment) is taken first, then the
        // one-segment starts, so each module is reached at its fewest segments.
        let mut dist = HashMap::default();
        let mut queue = VecDeque::new();
        reach(&mut dist, &mut queue, from, 0);
        reach(&mut dist, &mut queue, root, 1);
        if let Some(parent) = def_map.module(db, from).parent {
            reach(&mut dist, &mut queue, parent, 1);
        }
        for (_, item) in def_map.module(db, from).scope(db).types() {
            if let ModuleDefId::ModuleId(module) = item.def
                && module.krate(db) == krate
            {
                reach(&mut dist, &mut queue, module, 1);
            }
        }
        let mut best = None::<usize>;
        while let Some(module) = queue.pop_front() {
            let len = dist[&module];
            for item in first_names(module) {
                if !counts(&item) {
                    continue;
                }
                if item.def == target {
                    best = Some(best.map_or(len + 1, |best| best.min(len + 1)));
                }
                if let ModuleDefId::ModuleId(child) = item.def
                    && child.krate(db) == krate
                {
                    reach(&mut dist, &mut queue, child, len + 1);
                }
            }
        }
        match target {
            ModuleDefId::ModuleId(module) => dist.get(&module).copied(),
            _ => best,
        }
    }

    /// For the items of `main` (no enum variants: those go through their enum) and plain paths,
    /// `find_path` gives a path exactly as short as the shortest the brute force enumerates, and
    /// one whenever the brute force finds any. Modules are all `pub` here, so every module the
    /// brute force walks through is one `find_path`'s search reaches.
    #[test]
    fn shortest_against_brute_force() {
        let shape = Shape { private_modules: false, private_imports: true };
        for seed in 100..132u64 {
            let fixture = gen_fixture(seed, shape);
            let db = fixture::db(&fixture).into_db();
            let main = crate_named(&db, "main");
            let def_map = crate::nameres::crate_def_map(&db, main);
            let targets: Vec<ModuleDefId> = def_map
                .modules(&db)
                .flat_map(|(id, data)| {
                    data.scope(&db).declarations().chain(iter::once(ModuleDefId::ModuleId(id)))
                })
                .filter(|def| {
                    matches!(
                        def,
                        ModuleDefId::ModuleId(_)
                            | ModuleDefId::AdtId(_)
                            | ModuleDefId::TraitId(_)
                            | ModuleDefId::TypeAliasId(_)
                    )
                })
                .collect();
            for (from, _) in def_map.modules(&db) {
                for &target in &targets {
                    let found = find_path(
                        &db,
                        ItemInNs::Types(target),
                        from,
                        PrefixKind::Plain,
                        false,
                        CONFIGS[0],
                    );
                    let shortest = brute_force_len(&db, from, target);
                    assert_eq!(
                        found.map(|path| path.len()),
                        shortest,
                        "seed {seed}, {target:?} from {from:?}: found {:?}\n{fixture}",
                        found.map(|path| path.display(&db, Edition::CURRENT).to_string()),
                    );
                }
            }
        }
    }

    #[test]
    fn choice_layout() {
        assert_eq!(size_of::<Segment>(), 8);
        assert!(size_of::<Choice>() <= 28);
    }

    /// Budget: `find_path` for an item reached through the local import search (a re-export two
    /// modules away) and for one in another crate, each a second time on the same thread once the
    /// thread's scratch has grown and the path is interned: 0 allocations.
    #[test]
    fn a_warm_find_path_allocates_nothing() {
        let fx = fixture::db(
            r#"
//- /main.rs crate:main deps:dep
mod a {
    pub mod b {
        pub struct S;
    }
}
mod c {
    pub use crate::a::b::S as Again;
}
mod d {
    $0
}
//- /dep.rs crate:dep
pub mod x {
    pub struct Y;
}
"#,
        );
        let pos = fx.position();
        let db = fx.into_db();
        let module = db.module_at_position(pos);
        let (def_map, local_def_map) = module.local_def_map(&db);
        let item = |kind: PathKind, segments: &[&str]| {
            let path = ModPath::from_segments(
                kind,
                segments.iter().map(|it| Name::new_symbol_root(intern::Symbol::intern(it))),
            );
            let res = def_map
                .resolve_path(
                    local_def_map,
                    &db,
                    module,
                    &path,
                    crate::item_scope::BuiltinShadowMode::Module,
                    None,
                )
                .0;
            ItemInNs::Types(res.take_types().expect("the fixture's item resolves"))
        };
        let items =
            [item(PathKind::Crate, &["c", "Again"]), item(PathKind::Plain, &["dep", "x", "Y"])];
        let cfg = FindPathConfig {
            prefer_no_std: false,
            prefer_prelude: false,
            prefer_absolute: false,
            allow_unstable: false,
        };
        let find = || items.map(|it| find_path(&db, it, module, PrefixKind::Plain, false, cfg));
        let warm = find();
        assert!(warm.iter().all(Option::is_some), "a fixture item has no path: {warm:?}");
        let found = assert_at_most(0, "a warm `find_path`", find);
        assert_eq!(found, warm);
    }
}
