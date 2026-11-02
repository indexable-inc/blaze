//! This module implements a function to resolve a path `foo::bar::baz` to a
//! def, which is used within name resolution.
//!
//! When name resolution is finished, the result of resolving a path is either
//! `Some(def)` or `None`. However, when we are in the process of resolving imports
//! or macros, there's a third possibility:
//!
//!   I can't resolve this path right now, but I might be able to resolve this path
//!   later, when more macros are expanded.
//!
//! `ReachedFixedPoint` signals about this.
//!
//! A lookup allocates nothing: segments are read from the path's interned slice, a path that
//! continues in another def map (a `super` out of a block, a module of another crate) goes on
//! with a subslice instead of a new `ModPath`, and every segment is one `ItemScope::get`. The
//! first segment of a path with more segments is only looked for in the types namespace, the
//! only one the rest of the path reads.

use std::sync::LazyLock;

use base_db::Db;
use hir_expand::{
    mod_path::{ModPath, PathKind},
    name::Name,
};
use intern::Symbol;
use span::Edition;

use crate::{
    AdtId, EnumId, Lookup, ModuleDefId, ModuleId,
    item_scope::{BUILTIN_SCOPE, ImportOrExternCrate},
    nameres::{
        BlockInfo, BuiltinShadowMode, DefMap, LocalDefMap, MacroSubNs, crate_def_map,
        sub_namespace_match, trait_items::trait_item_per_ns,
    },
    per_ns::{PerNs, TypesItem},
    visibility::{RawVisibility, Visibility, VisibilityError, visibility_path},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ResolveMode {
    Import,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ReachedFixedPoint {
    Yes,
    No,
}

#[derive(Debug, Clone)]
pub(super) struct ResolvePathResult {
    pub(super) resolved_def: PerNs,
    /// The index of the last resolved segment, or `None` if the full path has been resolved.
    pub(super) segment_index: Option<usize>,
    pub(super) reached_fixedpoint: ReachedFixedPoint,
    pub(super) prefix_info: ResolvePathResultPrefixInfo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ResolvePathResultPrefixInfo {
    pub(crate) differing_crate: bool,
    /// Path of the form `Enum::Variant` (and not `Variant` alone).
    pub enum_variant: bool,
}

/// One thing a path lookup read, reported in order to the `reads` of
/// [`DefMap::resolve_path_fp_recording`]: a caller that retries the lookup when one of them is
/// written gets the answer a fresh lookup would. Reads of finished def maps (another crate, an
/// enclosing block) are reported too; the caller drops what it knows cannot change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum PathRead {
    /// `name` in the scope of `module`: one `ItemScope::get`.
    Slot(ModuleId, Name),
    /// The `macro_rules!` called `name` in the textual scope of `module` and the blocks around it.
    LegacyMacro(ModuleId, Name),
    /// `name` in the extern prelude.
    ExternPrelude(Name),
    /// `name` in the `macro_use` prelude.
    MacroUsePrelude(Name),
    /// Which module the std prelude is; a name read in it is then a `Slot` of that module.
    Prelude,
    /// Something without a single name: the children of a module, the declarations of a module.
    Unscoped,
}

impl ResolvePathResult {
    fn empty(reached_fixedpoint: ReachedFixedPoint) -> ResolvePathResult {
        ResolvePathResult::new(
            PerNs::none(),
            reached_fixedpoint,
            None,
            ResolvePathResultPrefixInfo::default(),
        )
    }

    fn new(
        resolved_def: PerNs,
        reached_fixedpoint: ReachedFixedPoint,
        segment_index: Option<usize>,
        prefix_info: ResolvePathResultPrefixInfo,
    ) -> ResolvePathResult {
        ResolvePathResult { resolved_def, segment_index, reached_fixedpoint, prefix_info }
    }

    /// Adds what the path means in an enclosing def map: names of the inner one win.
    fn merge(&mut self, new: ResolvePathResult) {
        self.resolved_def = self.resolved_def.or(new.resolved_def);
        if self.reached_fixedpoint == ReachedFixedPoint::No {
            self.reached_fixedpoint = new.reached_fixedpoint;
        }
        self.prefix_info.differing_crate |= new.prefix_info.differing_crate;
        self.prefix_info.enum_variant |= new.prefix_info.enum_variant;
        self.segment_index = match (self.segment_index, new.segment_index) {
            (Some(idx), None) => Some(idx),
            (Some(old), Some(new)) => Some(old.max(new)),
            (None, new) => new,
        };
    }
}

/// The arguments every step of one lookup shares, passed by value (32 bytes).
#[derive(Clone, Copy)]
struct PathQuery<'a> {
    local_def_map: &'a LocalDefMap,
    db: &'a Db,
    mode: ResolveMode,
    shadow: BuiltinShadowMode,
    expected_macro_subns: Option<MacroSubNs>,
    /// The def map the path resolves in is another crate's, reached by a path: complete, so
    /// `use Trait::item` lists even its own traits' items through `TraitItems`.
    collected: bool,
}

/// The name of blaze's test harness module (`blaze-engine`'s `test_harness::MODULE`).
static TEST_HARNESS: LazyLock<Symbol> = LazyLock::new(|| Symbol::intern("__blaze_test_harness"));

impl PerNs {
    pub(super) fn filter_macro(
        mut self,
        db: &Db,
        expected: Option<MacroSubNs>,
    ) -> Self {
        self.macros = self.macros.filter(|def| sub_namespace_match(db, def.def, expected));

        self
    }
}

impl DefMap {
    pub(crate) fn resolve_visibility(
        &self,
        local_def_map: &LocalDefMap,
        db: &Db,
        // module to import to
        original_module: ModuleId,
        // pub(path)
        //     ^^^^ this
        visibility: &RawVisibility,
        within_impl: bool,
    ) -> Result<Visibility, VisibilityError> {
        let vis = match visibility {
            RawVisibility::Module(path, explicitness) => {
                let path = visibility_path(path, self.data.edition)?;
                let (result, remaining) = self.resolve_path(
                    local_def_map,
                    db,
                    original_module,
                    &path,
                    BuiltinShadowMode::Module,
                    None,
                );
                if remaining.is_some() {
                    return Err(VisibilityError::Unresolved);
                }
                let types = result.take_types().ok_or(VisibilityError::Unresolved)?;
                let m = match types {
                    ModuleDefId::ModuleId(m) => m,
                    found => return Err(VisibilityError::NotAModule { found }),
                };
                if !Visibility::Module(m, *explicitness).is_visible_from_def_map(
                    db,
                    self,
                    original_module,
                ) {
                    return Err(VisibilityError::NotAncestor { module: m });
                }
                let mut vis = Visibility::Module(m, *explicitness);

                // In block expressions, `self` normally refers to the containing non-block module, and
                // `super` to its parent (etc.). However, visibilities must only refer to a module in the
                // DefMap they're written in, so we restrict them when that happens.
                if let Visibility::Module(m, mv) = vis {
                    // ...unless we're resolving visibility for an associated item in an impl.
                    if self.block_id() != m.block(db) && !within_impl {
                        vis = Visibility::Module(self.root, mv);
                    }
                }
                vis
            }
            RawVisibility::PubSelf(explicitness) => {
                Visibility::Module(original_module, *explicitness)
            }
            RawVisibility::Public => Visibility::Public,
            RawVisibility::PubCrate => Visibility::PubCrate(self.krate),
        };
        Ok(vis)
    }

    // Returns Yes if we are sure that additions to `ItemMap` wouldn't change
    // the result.
    pub(super) fn resolve_path_fp_with_macro(
        &self,
        local_def_map: &LocalDefMap,
        db: &Db,
        mode: ResolveMode,
        // module to import to
        original_module: ModuleId,
        path: &ModPath,
        shadow: BuiltinShadowMode,
        // Pass `MacroSubNs` if we know we're resolving macro names and which kind of macro we're
        // resolving them to. Pass `None` otherwise, e.g. when we're resolving import paths.
        expected_macro_subns: Option<MacroSubNs>,
    ) -> ResolvePathResult {
        let q = PathQuery { local_def_map, db, mode, shadow, expected_macro_subns, collected: false };
        self.resolve_fp(q, original_module, path.kind, path.segments(), &mut |_| {})
    }

    /// [`Self::resolve_path_fp_with_macro`], calling `reads` with everything the lookup read,
    /// in order.
    pub(super) fn resolve_path_fp_recording(
        &self,
        local_def_map: &LocalDefMap,
        db: &Db,
        mode: ResolveMode,
        original_module: ModuleId,
        path: &ModPath,
        shadow: BuiltinShadowMode,
        expected_macro_subns: Option<MacroSubNs>,
        reads: &mut impl FnMut(PathRead),
    ) -> ResolvePathResult {
        let q = PathQuery { local_def_map, db, mode, shadow, expected_macro_subns, collected: false };
        self.resolve_fp(q, original_module, path.kind, path.segments(), reads)
    }

    pub(super) fn resolve_path_fp_with_macro_single(
        &self,
        local_def_map: &LocalDefMap,
        db: &Db,
        mode: ResolveMode,
        original_module: ModuleId,
        path: &ModPath,
        shadow: BuiltinShadowMode,
        expected_macro_subns: Option<MacroSubNs>,
    ) -> ResolvePathResult {
        let q = PathQuery { local_def_map, db, mode, shadow, expected_macro_subns, collected: false };
        self.resolve_single(q, original_module, path.kind, path.segments(), &mut |_| {})
    }

    /// `segments` of a path of `kind`, as [`Self::resolve_path_fp_with_macro`] resolves them
    /// (`around`: in the def maps around a block too) or
    /// [`Self::resolve_path_fp_with_macro_single`] does: what [`super::uses`] resolves each
    /// prefix of a path with.
    pub(super) fn resolve_segments(
        &self,
        q: (&LocalDefMap, &Db, ResolveMode),
        original_module: ModuleId,
        kind: PathKind,
        segments: &[Name],
        shadow: BuiltinShadowMode,
        expected_macro_subns: Option<MacroSubNs>,
        around: bool,
    ) -> ResolvePathResult {
        let (local_def_map, db, mode) = q;
        let q = PathQuery { local_def_map, db, mode, shadow, expected_macro_subns, collected: false };
        match around {
            true => self.resolve_fp(q, original_module, kind, segments, &mut |_| {}),
            false => self.resolve_single(q, original_module, kind, segments, &mut |_| {}),
        }
    }

    /// The path in this def map and then, for a block, in the def maps around it. The loop runs
    /// once per enclosing block, with no closure or allocation per step.
    fn resolve_fp<R: FnMut(PathRead)>(
        &self,
        q: PathQuery<'_>,
        mut original_module: ModuleId,
        kind: PathKind,
        segments: &[Name],
        reads: &mut R,
    ) -> ResolvePathResult {
        let mut result = self.resolve_single(q, original_module, kind, segments, reads);

        if self.block.is_none() {
            // If we're in the root `DefMap`, we can resolve the path directly.
            return result;
        }

        let mut current_map = self;
        loop {
            match current_map.block {
                Some(block) if original_module == current_map.root => {
                    // Block modules "inherit" names from its parent module.
                    original_module = block.parent;
                    current_map = block.parent.def_map(q.db);
                }
                // Proper (non-block) modules, including those in block `DefMap`s, don't.
                _ => {
                    if original_module != current_map.root && current_map.block.is_some() {
                        // A module inside a block. Do not resolve items declared in upper blocks, but we do need to get
                        // the prelude items (which are not inserted into blocks because they can be overridden there).
                        original_module = current_map.root;
                        current_map = crate_def_map(q.db, self.krate);

                        let new = current_map.resolve_in_all_preludes(
                            q,
                            original_module,
                            kind,
                            segments,
                            reads,
                        );
                        result.merge(new);
                    }

                    return result;
                }
            }

            let new = current_map.resolve_single(q, original_module, kind, segments, reads);
            result.merge(new);
        }
    }

    fn resolve_single<R: FnMut(PathRead)>(
        &self,
        q: PathQuery<'_>,
        original_module: ModuleId,
        kind: PathKind,
        segments: &[Name],
        reads: &mut R,
    ) -> ResolvePathResult {
        let db = q.db;
        let types_only = segments.len() > 1;
        // What the path's kind and first segments name, and how many segments that took.
        let (curr_per_ns, resolved) = match kind {
            PathKind::DollarCrate(krate) => {
                let root = if krate == self.krate {
                    cov_mark::hit!(macro_dollar_crate_self);
                    self.crate_root(db)
                } else {
                    cov_mark::hit!(macro_dollar_crate_other);
                    crate_def_map(db, krate).root
                };
                (PerNs::types(root.into(), Visibility::Public, None), 0)
            }
            PathKind::Crate => (PerNs::types(self.crate_root(db).into(), Visibility::Public, None), 0),
            // plain import or absolute path in 2015: crate-relative with
            // fallback to extern prelude (with the simplification in
            // rust-lang/rust#57745)
            PathKind::Plain | PathKind::Abs
                if self.data.edition == Edition::Edition2015
                    && (kind == PathKind::Abs || q.mode == ResolveMode::Import) =>
            {
                match segments.first() {
                    Some(segment) => (
                        self.resolve_name_in_crate_root_or_extern_prelude(
                            q.local_def_map,
                            db,
                            original_module,
                            segment,
                            types_only,
                            reads,
                        ),
                        1,
                    ),
                    // `use ::*;` names the crate root itself.
                    None => {
                        (PerNs::types(self.crate_root(db).into(), Visibility::Public, None), 0)
                    }
                }
            }
            PathKind::Plain => {
                let Some(segment) = segments.first() else {
                    return ResolvePathResult::empty(ReachedFixedPoint::Yes);
                };
                // The first segment may be a builtin type. If the path has more
                // than one segment, we first try resolving it as a module
                // anyway.
                // FIXME: If the next segment doesn't resolve in the module and
                // BuiltinShadowMode wasn't Module, then we need to try
                // resolving it as a builtin.
                let prefer_module = if types_only { BuiltinShadowMode::Module } else { q.shadow };
                let res = self.resolve_name_in_module(
                    q.local_def_map,
                    db,
                    original_module,
                    segment,
                    prefer_module,
                    q.expected_macro_subns,
                    types_only,
                    reads,
                );
                (res, 1)
            }
            PathKind::Super(lvl) => {
                // Adjust `local_id` to `self`, i.e. the nearest non-block module.
                let (mut def_map, mut local_id) =
                    adjust_to_nearest_non_block_module(db, self, original_module);

                // Go up the module tree but skip block modules as `super` always refers to the
                // nearest non-block module.
                for _ in 0..lvl {
                    // Loop invariant: at the beginning of each loop, `local_id` must refer to a
                    // non-block module.
                    match def_map.module(db, local_id).parent {
                        Some(parent) => {
                            (def_map, local_id) =
                                adjust_to_nearest_non_block_module(db, def_map, parent);
                        }
                        None => {
                            stdx::always!(def_map.block.is_none());
                            return ResolvePathResult::empty(ReachedFixedPoint::Yes);
                        }
                    }
                }

                if self.block != def_map.block {
                    // If we have a different `DefMap` from `self` (the original `DefMap` we started
                    // with), resolve the remaining path segments in that `DefMap`, as `self::..`.
                    // This is the same crate, so the local def map is the same.
                    return def_map.resolve_fp(q, local_id, PathKind::SELF, segments, reads);
                }

                (PerNs::types(local_id.into(), Visibility::Public, None), 0)
            }
            // 2018-style absolute path -- only extern prelude
            PathKind::Abs => {
                let Some(segment) = segments.first() else {
                    return ResolvePathResult::empty(ReachedFixedPoint::Yes);
                };
                let res = self.resolve_name_in_extern_prelude(q.local_def_map, segment, reads);
                if res.is_none() {
                    // extern crate declarations can add to the extern prelude
                    return ResolvePathResult::empty(ReachedFixedPoint::No);
                }
                (res, 1)
            }
        };

        self.resolve_remaining_segments(q, segments, resolved, curr_per_ns, original_module, reads)
    }

    /// Resolves a path only in the preludes, without accounting for item scopes.
    fn resolve_in_all_preludes<R: FnMut(PathRead)>(
        &self,
        q: PathQuery<'_>,
        original_module: ModuleId,
        kind: PathKind,
        segments: &[Name],
        reads: &mut R,
    ) -> ResolvePathResult {
        let types_only = segments.len() > 1;
        let (curr_per_ns, resolved) = match kind {
            // plain import or absolute path in 2015: crate-relative with
            // fallback to extern prelude (with the simplification in
            // rust-lang/rust#57745)
            PathKind::Plain | PathKind::Abs
                if self.data.edition == Edition::Edition2015
                    && (kind == PathKind::Abs || q.mode == ResolveMode::Import) =>
            {
                let Some(segment) = segments.first() else {
                    return ResolvePathResult::empty(ReachedFixedPoint::Yes);
                };
                (self.resolve_name_in_extern_prelude(q.local_def_map, segment, reads), 1)
            }
            PathKind::Plain => {
                let Some(segment) = segments.first() else {
                    return ResolvePathResult::empty(ReachedFixedPoint::Yes);
                };
                let res = self.resolve_name_in_all_preludes(
                    q.local_def_map,
                    q.db,
                    segment,
                    types_only,
                    reads,
                );
                (res, 1)
            }
            PathKind::Abs => {
                let Some(segment) = segments.first() else {
                    return ResolvePathResult::empty(ReachedFixedPoint::Yes);
                };
                let res = self.resolve_name_in_extern_prelude(q.local_def_map, segment, reads);
                if res.is_none() {
                    return ResolvePathResult::empty(ReachedFixedPoint::No);
                }
                (res, 1)
            }
            PathKind::DollarCrate(_) | PathKind::Crate | PathKind::Super(_) => {
                return ResolvePathResult::empty(ReachedFixedPoint::Yes);
            }
        };

        self.resolve_remaining_segments(q, segments, resolved, curr_per_ns, original_module, reads)
    }

    /// Whether `module` is blaze's test harness, which names every test of its crate as the
    /// harness rustc writes does: a private one too.
    fn is_test_harness<R: FnMut(PathRead)>(
        &self,
        db: &Db,
        module: ModuleId,
        reads: &mut R,
    ) -> bool {
        let Some(name) = module.name(db) else { return false };
        if *name.symbol() != *TEST_HARNESS {
            return false;
        }
        let def_map = match module.krate(db) == self.krate && module.block(db) == self.block_id() {
            true => self,
            false => module.def_map(db),
        };
        let Some(parent) = def_map.module(db, module).parent else { return false };
        reads(PathRead::Unscoped);
        def_map.module(db, parent).children(db).get(&name) == Some(&module)
    }

    /// The visibility `e` is declared with. Its variants are that visible, even when the
    /// path reached the enum through a more private import. `reached` is the binding the path
    /// reached the enum by, under the name `named`.
    fn declared_enum_visibility<R: FnMut(PathRead)>(
        &self,
        db: &Db,
        e: EnumId,
        reached: TypesItem,
        named: Option<&Name>,
        reads: &mut R,
    ) -> Option<Visibility> {
        // A binding with no import is the declaration (`ItemScope::declare`): a module's scope
        // holds one binding per name and namespace, and a declared item is bound there only.
        if reached.import.is_none() {
            return Some(reached.vis);
        }
        let module = e.lookup(db).container;
        let module_data = if module.krate(db) == self.krate && module.block(db) == self.block_id() {
            self.module(db, module)
        } else {
            module.def_map(db).module(db, module)
        };
        let def = ModuleDefId::from(e);
        let is_declaration = |item: &TypesItem| item.def == def && item.import.is_none();
        // Not renamed on the way: the declaration is bound under the name the path used.
        if let Some(name) = named {
            reads(PathRead::Slot(module, *name));
            if let Some(item) = module_data.name_in_scope(db, name).types
                && is_declaration(&item)
            {
                return Some(item.vis);
            }
        }
        reads(PathRead::Unscoped);
        module_data
            .scope(db)
            .types()
            .find(|(_, item)| is_declaration(item))
            .map(|(_, item)| item.vis)
    }

    /// `segments[resolved..]`, from `curr_per_ns`, the meaning of `segments[..resolved]`.
    fn resolve_remaining_segments<R: FnMut(PathRead)>(
        &self,
        q: PathQuery<'_>,
        segments: &[Name],
        resolved: usize,
        mut curr_per_ns: PerNs,
        original_module: ModuleId,
        reads: &mut R,
    ) -> ResolvePathResult {
        let db = q.db;
        for (i, segment) in segments.iter().enumerate().skip(resolved) {
            let Some(curr) = curr_per_ns.take_types_full() else {
                // we still have path segments left, but the path so far
                // didn't resolve in the types namespace => no resolution
                // (don't break here because `curr_per_ns` might contain
                // something in the value namespace, and it would be wrong
                // to return that)
                return ResolvePathResult::empty(ReachedFixedPoint::No);
            };
            // resolve segment in curr

            curr_per_ns = match curr.def {
                ModuleDefId::ModuleId(module) => {
                    if module.krate(db) != self.krate {
                        let def_map = module.def_map(db);
                        // Macro sub-namespaces only matter when resolving single-segment paths
                        // because `macro_use` and other preludes should be taken into account. At
                        // this point, we know we're resolving a multi-segment path so macro kind
                        // expectation is discarded.
                        let q = PathQuery {
                            local_def_map: LocalDefMap::EMPTY,
                            expected_macro_subns: None,
                            collected: true,
                            ..q
                        };
                        let resolution =
                            def_map.resolve_fp(q, module, PathKind::SELF, &segments[i..], reads);
                        // That resolves from the other crate's root. What a crate of procedural
                        // macros has besides its macros is its own, its proc-macro functions too.
                        let resolved_def = match def_map.data.proc_macro {
                            true => resolution.resolved_def.filter_visibility(|vis| {
                                vis.is_visible_from_def_map(db, self, original_module)
                            }),
                            false => resolution.resolved_def,
                        };
                        return ResolvePathResult::new(
                            resolved_def,
                            ReachedFixedPoint::Yes,
                            resolution.segment_index.map(|s| s + i),
                            ResolvePathResultPrefixInfo {
                                differing_crate: true,
                                enum_variant: resolution.prefix_info.enum_variant,
                            },
                        );
                    }

                    let def_map = if module.block(db) == self.block_id() {
                        self
                    } else {
                        module.def_map(db)
                    };

                    // Since it is a qualified path here, it should not contains legacy macros
                    reads(PathRead::Slot(module, *segment));
                    def_map.name_in_module(db, module, segment)
                }
                ModuleDefId::AdtId(AdtId::EnumId(e)) => {
                    // enum variant
                    cov_mark::hit!(can_import_enum_variant);

                    let named = i.checked_sub(1).map(|prev| &segments[prev]);
                    let vis =
                        self.declared_enum_visibility(db, e, curr, named, reads).unwrap_or(curr.vis);
                    // A variant is no binding of the enum's module: a write touching the enum
                    // wakes this read (`enum_glob_wakes`), as it wakes a glob of the enum.
                    crate::nameres::delta::reads::note(
                        crate::Lookup::lookup(&e, db).container,
                        Some(crate::nameres::delta::reads::enum_glob()),
                    );
                    let res = e.enum_variants(db).variants.get(segment).map(|&(variant, shape)| {
                        if shape.has_value_ns_ctor() {
                            PerNs::both(variant.into(), variant.into(), vis, None)
                        } else {
                            PerNs::types(variant.into(), vis, None)
                        }
                    });
                    // FIXME: Need to filter visibility here and below? Not sure.
                    return match res {
                        // Enum variants are in value namespace, segments left => no resolution.
                        Some(_) if i + 1 < segments.len() => {
                            ResolvePathResult::empty(ReachedFixedPoint::No)
                        }
                        Some(res) => ResolvePathResult::new(
                            res,
                            ReachedFixedPoint::Yes,
                            None,
                            ResolvePathResultPrefixInfo {
                                enum_variant: true,
                                ..ResolvePathResultPrefixInfo::default()
                            },
                        ),
                        None => ResolvePathResult::new(
                            PerNs::types(e.into(), curr.vis, curr.import),
                            ReachedFixedPoint::Yes,
                            Some(i),
                            ResolvePathResultPrefixInfo::default(),
                        ),
                    };
                }
                ModuleDefId::TraitId(trait_)
                    if q.mode == ResolveMode::Import && i + 1 == segments.len() =>
                {
                    // `use Trait::item`: an import reads a trait as rustc's module of its
                    // associated items. Whether the crate may (E0658) is the importer's to
                    // report. Items the trait's def map cannot list yet bind nothing, and the
                    // importer reports `ItemsFromMacros`. An item is no binding of the trait's
                    // module: a write touching the trait wakes this read (`enum_glob_wakes`).
                    crate::nameres::delta::reads::note(
                        crate::Lookup::lookup(&trait_, db).container,
                        Some(crate::nameres::delta::reads::trait_import()),
                    );
                    let mut res = PerNs::none();
                    let listed = self.for_each_trait_item_for_import(db, trait_, q.collected, |name, item| {
                        if name == *segment {
                            res = res.or(trait_item_per_ns(item, curr.vis));
                        }
                    });
                    if listed.is_err() {
                        res = PerNs::none();
                    }
                    return ResolvePathResult::new(
                        res,
                        ReachedFixedPoint::Yes,
                        None,
                        ResolvePathResultPrefixInfo::default(),
                    );
                }
                def => {
                    // could be an inherent method call in UFCS form
                    // (`Struct::method`), or some other kind of associated item, resolved by
                    // type inference.
                    return ResolvePathResult::new(
                        PerNs::types(def, curr.vis, curr.import),
                        ReachedFixedPoint::Yes,
                        Some(i),
                        ResolvePathResultPrefixInfo::default(),
                    );
                }
            };

            let visible = curr_per_ns
                .filter_visibility(|vis| vis.is_visible_from_def_map(db, self, original_module));
            // A name can be a private module and a public function at once: the harness keeps
            // the module.
            curr_per_ns = match visible != curr_per_ns
                && self.is_test_harness(db, original_module, reads)
            {
                true => curr_per_ns,
                false => visible,
            };
        }

        ResolvePathResult::new(
            curr_per_ns,
            ReachedFixedPoint::Yes,
            None,
            ResolvePathResultPrefixInfo::default(),
        )
    }

    /// The first segment of a plain path. With `types_only` (the path has more segments, which
    /// read only the types namespace of this one), the lookup stops at the first source that
    /// has a type and skips the macro sources.
    fn resolve_name_in_module<R: FnMut(PathRead)>(
        &self,
        local_def_map: &LocalDefMap,
        db: &Db,
        module: ModuleId,
        name: &Name,
        shadow: BuiltinShadowMode,
        expected_macro_subns: Option<MacroSubNs>,
        types_only: bool,
        reads: &mut R,
    ) -> PerNs {
        // Resolve in:
        //  - legacy scope of macro
        //  - current module / scope
        //  - extern prelude / macro_use prelude
        //  - std prelude
        // A module declared in a block is in the textual scope of the macros before the block.
        let from_legacy_macro = match types_only {
            true => PerNs::none(),
            false => {
                reads(PathRead::LegacyMacro(module, *name));
                self.legacy_macro_in_textual_scope(db, module, name)
                    .filter(|&id| sub_namespace_match(db, id, expected_macro_subns))
                    .map_or_else(PerNs::none, |m| PerNs::macros(m, Visibility::Public, None))
            }
        };
        reads(PathRead::Slot(module, *name));
        let from_scope = self.name_in_module(db, module, name);
        let from_scope = match types_only {
            true => from_scope,
            false => from_scope.filter_macro(db, expected_macro_subns),
        };
        let from_builtin = match self.block {
            Some(_) => {
                // Only resolve to builtins in the root `DefMap`.
                PerNs::none()
            }
            None => BUILTIN_SCOPE.get(name).copied().unwrap_or_else(PerNs::none),
        };
        let from_scope_or_builtin = match shadow {
            BuiltinShadowMode::Module => from_scope.or(from_builtin),
            BuiltinShadowMode::Other => match from_scope.take_types() {
                Some(ModuleDefId::ModuleId(_)) => from_builtin.or(from_scope),
                Some(_) | None => from_scope.or(from_builtin),
            },
        };

        // Each source below adds the namespaces the ones before left empty.
        let done = |res: &PerNs| if types_only { res.types.is_some() } else { res.is_full() };
        // Don't resolve the extern prelude and the std prelude in pseudo-modules of blocks,
        // because they might have been shadowed by local names.
        let in_block_root = self.block.is_some() && module == self.root;
        let mut res = from_legacy_macro.or(from_scope_or_builtin);
        if done(&res) {
            return res;
        }
        if !in_block_root {
            res = res.or(self.resolve_name_in_extern_prelude(local_def_map, name, reads));
            if done(&res) {
                return res;
            }
        }
        if !types_only {
            res = res.or(self.resolve_in_macro_use_prelude(name, reads));
            if done(&res) {
                return res;
            }
        }
        if !in_block_root {
            res = res.or(self.resolve_in_prelude_reading(db, name, reads));
        }
        res
    }

    fn resolve_name_in_all_preludes<R: FnMut(PathRead)>(
        &self,
        local_def_map: &LocalDefMap,
        db: &Db,
        name: &Name,
        types_only: bool,
        reads: &mut R,
    ) -> PerNs {
        // Resolve in:
        //  - extern prelude / macro_use prelude
        //  - std prelude
        let done = |res: &PerNs| if types_only { res.types.is_some() } else { res.is_full() };
        let mut res = self.resolve_name_in_extern_prelude(local_def_map, name, reads);
        if done(&res) {
            return res;
        }
        if !types_only {
            res = res.or(self.resolve_in_macro_use_prelude(name, reads));
            if done(&res) {
                return res;
            }
        }
        res.or(self.resolve_in_prelude_reading(db, name, reads))
    }

    fn resolve_name_in_extern_prelude<R: FnMut(PathRead)>(
        &self,
        local_def_map: &LocalDefMap,
        name: &Name,
        reads: &mut R,
    ) -> PerNs {
        reads(PathRead::ExternPrelude(*name));
        local_def_map.extern_prelude.get(name).map_or(PerNs::none(), |&(it, extern_crate)| {
            PerNs::types(
                it.into(),
                Visibility::Public,
                extern_crate.map(ImportOrExternCrate::ExternCrate),
            )
        })
    }

    fn resolve_in_macro_use_prelude<R: FnMut(PathRead)>(&self, name: &Name, reads: &mut R) -> PerNs {
        reads(PathRead::MacroUsePrelude(*name));
        self.macro_use_prelude.get(name).map_or(PerNs::none(), |&(it, extern_crate)| {
            PerNs::macros(
                it,
                Visibility::Public,
                extern_crate.map(ImportOrExternCrate::ExternCrate),
            )
        })
    }

    fn resolve_name_in_crate_root_or_extern_prelude<R: FnMut(PathRead)>(
        &self,
        local_def_map: &LocalDefMap,
        db: &Db,
        module: ModuleId,
        name: &Name,
        types_only: bool,
        reads: &mut R,
    ) -> PerNs {
        let from_crate_root = match self.block {
            Some(_) => {
                let def_map = self.crate_root(db).def_map(db);
                reads(PathRead::Slot(def_map.root, *name));
                def_map.name_in_module(db, def_map.root, name)
            }
            None => {
                reads(PathRead::Slot(self.root, *name));
                self.name_in_module(db, self.root, name)
            }
        };
        let done = if types_only { from_crate_root.types.is_some() } else { from_crate_root.is_full() };
        // Don't resolve the extern prelude in the pseudo-module of a block.
        if done || (self.block.is_some() && module == self.root) {
            return from_crate_root;
        }
        from_crate_root.or(self.resolve_name_in_extern_prelude(local_def_map, name, reads))
    }

    pub(super) fn resolve_in_prelude(&self, db: &Db, name: &Name) -> PerNs {
        self.resolve_in_prelude_reading(db, name, &mut |_| {})
    }

    fn resolve_in_prelude_reading<R: FnMut(PathRead)>(
        &self,
        db: &Db,
        name: &Name,
        reads: &mut R,
    ) -> PerNs {
        reads(PathRead::Prelude);
        if let Some((prelude, _use)) = self.prelude {
            let def_map = if prelude.krate(db) == self.krate { self } else { prelude.def_map(db) };
            reads(PathRead::Slot(prelude, *name));
            def_map.name_in_module(db, prelude, name)
        } else {
            PerNs::none()
        }
    }
}

/// Given a block module, returns its nearest non-block module and the `DefMap` it belongs to.
#[inline]
fn adjust_to_nearest_non_block_module<'db>(
    db: &'db Db,
    mut def_map: &'db DefMap,
    mut local_id: ModuleId,
) -> (&'db DefMap, ModuleId) {
    if def_map.root_module_id() != local_id {
        // if we aren't the root, we are either not a block module, or a non-block module inside a
        // block def map.
        return (def_map, local_id);
    }
    while let Some(BlockInfo { parent, .. }) = def_map.block {
        def_map = parent.def_map(db);
        local_id = parent;
        if def_map.root_module_id() != local_id {
            return (def_map, local_id);
        }
    }
    (def_map, local_id)
}
