//! Macro resolution, one module at one round (`nameres-core.md` §5): the old
//! `DefCollector::resolve_macros`, `expand_round`, `reseed_with_unresolved_attribute` and
//! `finish`'s unresolved-macro diagnostics, over a [`Sink`], and the [`macro_step`] query that
//! runs them for one module of a crate against the round's facts.
//!
//! A step's view holds its module (the scope the round's import fixed point left, cloned) and
//! the modules its expansions declare; every other module is a row of the round
//! (`Rows::Round`), each scope a fact. What the step's collections do becomes the ops of round
//! `r + 1`: of the module itself, of the crate root (`#[macro_export]`), of the modules it
//! declared. The module's own new directives stay with the step (`pending`), the per-item tables
//! too (`side`).

use std::{
    alloc::Allocator,
    cell::{Cell, RefCell},
};

use base_db::{Crate, Db};
use blaze_hash::HashMap;
use blaze_mem::Frame;
use cfg::CfgAtom;
use either::Either;
use hir_expand::{
    AttrMacroAttrIds, HirFileId, InFile, MacroCallId, MacroCallKind, MacroDefId, MacroDefKind,
    attrs::{Attr, AttrId},
    eager::EagerCall,
    mod_path::ModPath,
    name::Name,
};
use intern::sym;
use smallvec::SmallVec;
use span::FileAstId;
use syntax::ast;
use triomphe::Arc;

use crate::{
    AstId, AstIdWithPath, ItemContainerId, MacroId, ModuleDefId, ModuleId, UnresolvedMacro, UseId,
    item_scope::{BuiltinShadowMode, ImportOrExternCrate, ItemScope, PerNsGlobImports},
    item_tree::{AttrsOrCfgRef, ImportKind, ModItemId, ModItemKind, TreeId},
    macro_call_as_call_id,
    nameres::{
        DefMap, DiagnosticsByModule, LocalDefMap, MacroSubNs, ModuleData, ModuleOrigin,
        ModuleScope, ModulesMap, ModulesSlot,
        attr_resolution::{attr_macro_as_call_id, derive_macro_as_call_id},
        crate_def_map,
        diagnostics::DefDiagnostic,
        path_resolution::ResolveMode,
        sub_namespace_match,
        uses::{DeriveHelperUse, NameRecords},
    },
    per_ns::{Item, PerNs},
    visibility::Visibility,
};

use super::{
    FrameVec, collect,
    env::{ChildMode, ChildRequest, CollectCx, CrateEnv, SideTables, Sink},
    ops::{DeclOp, DeferredDerive, MacroOp, MacroOpKind, OpLog},
    queries::{
        self, FactKey, MacroStep, ModuleRow, RoundKey, RoundKind, RoundModules, Rows, RowsKey,
    },
};

/// What one [`resolve_round`] did: whether a call resolved (the old `resolve_macros` returning
/// `ReachedFixedPoint::No`), and whether it left an eager call for the error round.
pub(crate) struct RoundResult {
    pub(crate) progress: bool,
    pub(crate) deferred_eager: bool,
}

/// A macro call a round resolved: its module, depth, container, call and the item an
/// attribute macro is on.
type ResolvedCall = (ModuleId, u32, ItemContainerId, MacroCallId, Option<AstId<ast::Item>>);

#[derive(PartialEq, Eq)]
enum Resolved {
    Yes,
    No,
}

/// One `resolve_macros` over `pending` through `sink`: resolves each directive (all three
/// kinds, eager calls, the builtin-derive fast path and its deferral), retains the rest,
/// expands the resolved ones in one `collect::expand_all` batch, collects each expansion.
pub(crate) fn resolve_round<'db, 'f, S: Sink<'db>, A: Allocator + Clone>(
    cx: &CollectCx<'db, 'f>,
    sink: &mut S,
    side: &mut SideTables<A>,
    pending: &mut FrameVec<'f, MacroOp>,
    eager_with_errors: bool,
) -> RoundResult {
    let frame = cx.frame;
    // Directives the sink took before the round are the round's too.
    sink.take_macros(pending);
    let macros = std::mem::replace(pending, Vec::new_in(frame));
    let span = tracing::info_span!(
        "nameres resolve_round",
        pending = macros.len(),
        eager_with_errors,
        resolved = tracing::field::Empty,
        kept = tracing::field::Empty,
        progress = tracing::field::Empty,
    )
    .entered();
    let mut kept = Vec::with_capacity_in(macros.len(), frame);
    // The directives the round's own resolutions add (an attribute's item collected again, a
    // derive left unresolved), in the order they come: after the retained ones.
    let mut added = Vec::new_in(frame);
    let mut resolved: FrameVec<'f, ResolvedCall> = Vec::new_in(frame);
    let mut eager = Vec::new_in(frame);
    let mut eager_calls = Vec::new_in(frame);
    let mut progress = false;
    let mut deferred_eager = false;
    for directive in macros {
        let _reading = crate::nameres::delta::reads::reading(directive.module);
        let outcome = resolve_one(
            cx,
            sink,
            side,
            &directive,
            eager_with_errors,
            &mut Round {
                resolved: &mut resolved,
                eager: &mut eager,
                eager_calls: &mut eager_calls,
                added: &mut added,
                deferred_eager: &mut deferred_eager,
            },
        );
        match outcome {
            Resolved::Yes => progress = true,
            Resolved::No => kept.push(directive),
        }
    }
    kept.append(&mut added);
    *pending = kept;
    span.record("resolved", resolved.len());
    span.record("kept", pending.len());
    span.record("progress", progress);

    for (module, call, id) in eager {
        sink.apply(DeclOp::AddMacroInvoc { module, call, id });
    }

    expand_round(cx, sink.def_map().recursion_limit(), &resolved);
    for (module, depth, container, call, attr_macro_item) in resolved {
        let _reading = crate::nameres::delta::reads::reading(module);
        collect::collect_expansion(cx, sink, side, module, call, depth, container, attr_macro_item);
        sink.take_macros(pending);
    }

    RoundResult { progress, deferred_eager }
}

/// The lists one round of [`resolve_round`] fills while it goes through its directives.
struct Round<'a, 'f> {
    resolved: &'a mut FrameVec<'f, ResolvedCall>,
    /// The calls eager expansion made of the arguments of a call it resolved, for
    /// `add_macro_invoc` once the round's directives are done.
    eager: &'a mut FrameVec<'f, (ModuleId, AstId<ast::MacroCall>, MacroCallId)>,
    /// What one call's eager expansion made, before it moves into `eager`.
    eager_calls: &'a mut FrameVec<'f, EagerCall>,
    added: &'a mut FrameVec<'f, MacroOp>,
    deferred_eager: &'a mut bool,
}

/// What `path` names as a macro of the sub-namespace `subns` in `module`.
fn resolve_macro<'db, S: Sink<'db>>(
    sink: &S,
    module: ModuleId,
    path: &ModPath,
    subns: MacroSubNs,
) -> Option<(MacroId, MacroDefId)> {
    let db = sink.db();
    sink.def_map()
        .resolve_path_fp_with_macro(
            sink.local_def_map(),
            db,
            ResolveMode::Other,
            module,
            path,
            BuiltinShadowMode::Module,
            Some(subns),
        )
        .resolved_def
        .take_macros()
        .map(|it| (it, it.definition(db)))
}

/// [`resolve_macro`]'s definition, or a `macro_rules!` only the textual scope has.
fn resolve_macro_def<'db, S: Sink<'db>>(
    sink: &S,
    module: ModuleId,
    path: &ModPath,
    subns: MacroSubNs,
) -> Option<MacroDefId> {
    let db = sink.db();
    resolve_macro(sink, module, path, subns).map(|(_, it)| it).or_else(|| {
        // A `macro_rules!` only the textual scope has, named where a call that resolves late
        // (an eager one, as `include!` from the prelude) names it: rquickjs-sys's
        // `include!(concat!("bindings/", bindings_env!("TARGET"), ".rs"))`, after an `include!`
        // from `OUT_DIR` that defines `bindings_env!`.
        let name = path.as_ident()?;
        sink.def_map()
            .legacy_macro_in_textual_scope(db, module, name)
            .filter(|&id| sub_namespace_match(db, id, Some(subns)))
            .map(|it| it.definition(db))
    })
}

/// The old `retain` closure of `resolve_macros` for one directive.
fn resolve_one<'db, 'f, S: Sink<'db>, A: Allocator + Clone>(
    cx: &CollectCx<'db, 'f>,
    sink: &mut S,
    side: &mut SideTables<A>,
    directive: &MacroOp,
    eager_with_errors: bool,
    round: &mut Round<'_, 'f>,
) -> Resolved {
    let db = cx.db;
    let krate = cx.krate;
    let module = directive.module;
    let push_resolved = |resolved: &mut FrameVec<'f, ResolvedCall>, directive: &MacroOp, call_id| {
        let attr_macro_item = match &directive.kind {
            MacroOpKind::Attr { ast_id, .. } => Some(ast_id.ast_id),
            MacroOpKind::FnLike { .. } | MacroOpKind::Derive { .. } => None,
        };
        resolved.push((directive.module, directive.depth, directive.container, call_id, attr_macro_item));
    };

    match &directive.kind {
        MacroOpKind::FnLike { ast_id, expand_to, ctxt: call_site } => {
            let view: &S = sink;
            let unresolved = Cell::new(false);
            let named = RefCell::new(Vec::new_in(cx.frame));
            let buffered = round.eager.len();
            let call_id = macro_call_as_call_id(
                db,
                ast_id.ast_id,
                &ast_id.path,
                *call_site,
                *expand_to,
                krate,
                directive.depth,
                view.def_map().recursion_limit(),
                |path| {
                    let def = resolve_macro_def(view, module, path, MacroSubNs::Bang);
                    unresolved.set(unresolved.get() || def.is_none());
                    if let Some(def) = def
                        && path != &ast_id.path
                    {
                        // clone: an eager argument's path, kept for `NameRecords`.
                        named.borrow_mut().push((path.clone(), def));
                    }
                    def
                },
                round.eager_calls,
            );
            round.eager.extend(
                round
                    .eager_calls
                    .drain(..)
                    .map(|(ptr, call_id)| (module, ptr.map(|(_, it)| it), call_id)),
            );
            if call_id.is_ok() && unresolved.get() && !eager_with_errors {
                // A macro its arguments name may be defined by one not expanded yet.
                round.eager.truncate(buffered);
                *round.deferred_eager = true;
                return Resolved::No;
            }
            // FIXME: Expansion error
            if let Ok(call_id) = call_id
                && let Some(call_id) = call_id.value
            {
                sink.apply(DeclOp::AddMacroInvoc { module, call: ast_id.ast_id, id: call_id });
                sink.apply(DeclOp::ResolvedMacroPath {
                    module,
                    // clone: the call's path, kept for `NameRecords`.
                    path: ast_id.path.clone(),
                    bang: true,
                    def: call_id.loc(db).def,
                    call: Some(call_id),
                });
                for (path, def) in named.into_inner() {
                    sink.apply(DeclOp::ResolvedMacroPath { module, path, bang: true, def, call: None });
                }
                push_resolved(round.resolved, directive, call_id);
                return Resolved::Yes;
            }
        }
        MacroOpKind::Derive { ast_id, derive_attr, derive_pos, ctxt: call_site, derive_macro_id } => {
            let view: &S = sink;
            let id = derive_macro_as_call_id(
                db,
                ast_id,
                *derive_attr,
                *derive_pos as u32,
                *call_site,
                krate,
                |path| resolve_macro(view, module, path, MacroSubNs::Attr),
                *derive_macro_id,
                directive.depth,
            );

            if let Ok((macro_id, def_id, call_id)) = id {
                sink.apply(DeclOp::ResolvedMacroPath {
                    module,
                    // clone: the derive's path, kept for `NameRecords`.
                    path: ast_id.path.clone(),
                    bang: false,
                    def: def_id,
                    call: Some(call_id),
                });
                sink.apply(DeclOp::SetDeriveMacroInvoc {
                    module,
                    adt: ast_id.ast_id,
                    call: Either::Left(call_id),
                    attr: *derive_attr,
                    idx: *derive_pos,
                });
                // Record its helper attributes.
                if def_id.krate != krate
                    && let Some(helpers) = crate_def_map(db, def_id.krate).data.exported_derives.get(&macro_id)
                {
                    side.derive_helpers_of(ast_id.ast_id.map(|it| it.upcast()))
                        .extend(helpers.iter().map(|&name| (name, macro_id, Either::Left(call_id))));
                }

                push_resolved(round.resolved, directive, call_id);
                return Resolved::Yes;
            }
        }
        MacroOpKind::Attr { ast_id: file_ast_id, mod_item, attr_id, attr, tree, .. } => {
            let &AstIdWithPath { ast_id, ref path } = file_ast_id;
            let file_id = ast_id.file_id;
            let recollect_without = |sink: &mut S, side: &mut SideTables<A>, added: &mut FrameVec<'f, MacroOp>| {
                recollect(cx, sink, side, added, directive, file_id, *mod_item, *attr_id, *tree);
                Resolved::Yes
            };

            if let Some(ident) = path.as_ident()
                && let Some(helpers) = side.derive_helpers.get(&ast_id)
                && let Some(&(_, derive, _)) = helpers.iter().find(|(it, ..)| it == ident)
            {
                cov_mark::hit!(resolved_derive_helper);
                sink.apply(DeclOp::DeriveHelperUse(DeriveHelperUse {
                    item: ast_id,
                    attr: *attr_id,
                    derive,
                    before_derive: false,
                }));
                // Resolved to derive helper. Collect the item's attributes again, starting
                // after the derive helper.
                return recollect_without(sink, side, round.added);
            }

            // rustc's `DeriveHelpersCompat`: a helper of a derive the item's later `#[derive]`s
            // name, which rustc accepts with the `legacy_derive_helpers` lint.
            if let Some(ident) = path.as_ident()
                && let Some(AttrsOrCfgRef::Enabled { attrs }) = tree.item_tree(db, krate).attrs(*mod_item)
            {
                let view: &S = sink;
                let helper_of = |derive_path: &ModPath| {
                    let (macro_id, def) = resolve_macro(view, module, derive_path, MacroSubNs::Attr)?;
                    if def.krate == krate {
                        return None;
                    }
                    let exported = &crate_def_map(db, def.krate).data.exported_derives;
                    exported.get(&macro_id).is_some_and(|helpers| helpers.contains(ident)).then_some(macro_id)
                };
                let derive = attrs
                    .iter_after(Some(*attr_id))
                    .filter(|(_, later)| later.path.as_ident().is_some_and(|it| *it == sym::derive))
                    .filter_map(|(_, later)| later.parse_path_comma_token_tree(db))
                    .flatten()
                    .find_map(|(derive_path, ..)| helper_of(&derive_path));
                if let Some(derive) = derive {
                    sink.apply(DeclOp::DeriveHelperUse(DeriveHelperUse {
                        item: ast_id,
                        attr: *attr_id,
                        derive,
                        before_derive: true,
                    }));
                    return recollect_without(sink, side, round.added);
                }
            }

            let def = match resolve_macro_def(&*sink, module, path, MacroSubNs::Attr) {
                Some(def) if def.is_attribute() => def,
                _ => return Resolved::No,
            };

            // Skip #[test]/#[bench]/#[test_case] expansion, which would merely result in more
            // memory usage due to duplicating functions into macro expansions, but only if
            // `cfg(test)` is active, otherwise they are expanded to nothing and this can impact
            // e.g. diagnostics (due to things being cfg'ed out).
            if matches!(
                def.kind,
                MacroDefKind::BuiltInAttr(_, expander)
                if expander.is_test() || expander.is_bench() || expander.is_test_case()
            ) && cx.cfg_options.check_atom(&CfgAtom::Flag(sym::test))
            {
                sink.apply(DeclOp::ResolvedMacroPath {
                    module,
                    // clone: the attribute's path, kept for `NameRecords`.
                    path: path.clone(),
                    bang: false,
                    def,
                    call: None,
                });
                return recollect_without(sink, side, round.added);
            }

            if matches!(def, MacroDefId { kind: MacroDefKind::BuiltInAttr(_, exp), .. } if exp.is_derive())
            {
                // Resolved to `#[derive]`, we don't actually expand this attribute like normal
                // (as that would just be an identity expansion with extra output). Instead we
                // treat derive attributes special and apply them separately.
                let item_tree = tree.item_tree(db, krate);
                let ast_adt_id: FileAstId<ast::Adt> = match item_tree.index(*mod_item) {
                    ModItemKind::Struct(ast_id, _) => ast_id.upcast(),
                    ModItemKind::Union(ast_id, _) => ast_id.upcast(),
                    ModItemKind::Enum(ast_id, _) => ast_id.upcast(),
                    _ => {
                        let diag = DefDiagnostic::invalid_derive_target(module, ast_id, *attr_id);
                        sink.apply(DeclOp::Diagnostic(diag));
                        return recollect_without(sink, side, round.added);
                    }
                };

                let ast_id = ast_id.with_value(ast_adt_id);

                match attr.parse_path_comma_token_tree(db) {
                    Some(derive_macros) => {
                        let call_id = attr_call(cx, side, file_ast_id, attr, *attr_id, def, directive.depth);
                        sink.apply(DeclOp::ResolvedMacroPath {
                            module,
                            // clone: the attribute's path, kept for `NameRecords`.
                            path: path.clone(),
                            bang: false,
                            def,
                            call: Some(call_id),
                        });
                        let mut derive_call_ids = SmallVec::new();
                        for (idx, (path, call_site, _)) in derive_macros.enumerate() {
                            let view: &S = sink;
                            let ast_id = AstIdWithPath::new(file_id, ast_id.value, path);

                            derive_call_ids.push(None);

                            // Try to resolve the derive immediately. If we succeed, we can also
                            // use the fast path for builtin derives. If not, we cannot use it,
                            // as it can cause the ADT to become interned while the derive is
                            // still unresolved, which will cause it to get forgotten.
                            let id = derive_macro_as_call_id(
                                db,
                                &ast_id,
                                *attr_id,
                                idx as u32,
                                call_site.ctx,
                                krate,
                                |path| resolve_macro(view, module, path, MacroSubNs::Attr),
                                call_id,
                                directive.depth,
                            );

                            let ast_id_without_path = ast_id.ast_id;
                            let derive = MacroOp {
                                module,
                                depth: directive.depth + 1,
                                kind: MacroOpKind::Derive {
                                    ast_id,
                                    derive_attr: *attr_id,
                                    derive_pos: idx,
                                    ctxt: call_site.ctx,
                                    derive_macro_id: call_id,
                                },
                                container: directive.container,
                            };

                            if let Ok((macro_id, def_id, call_id)) = id {
                                if let MacroOpKind::Derive { ast_id, .. } = &derive.kind {
                                    sink.apply(DeclOp::ResolvedMacroPath {
                                        module,
                                        // clone: the derive's path, kept for `NameRecords`.
                                        path: ast_id.path.clone(),
                                        bang: false,
                                        def: def_id,
                                        call: Some(call_id),
                                    });
                                }
                                let (mut helpers_start, mut helpers_end) = (0, 0);
                                // Record its helper attributes.
                                if def_id.krate != krate
                                    && let Some(helpers) = crate_def_map(db, def_id.krate)
                                        .data
                                        .exported_derives
                                        .get(&macro_id)
                                {
                                    let derive_helpers =
                                        side.derive_helpers_of(ast_id_without_path.map(|it| it.upcast()));
                                    helpers_start = derive_helpers.len();
                                    derive_helpers.extend(
                                        helpers.iter().map(|&name| (name, macro_id, Either::Left(call_id))),
                                    );
                                    helpers_end = derive_helpers.len();
                                }

                                if cx.builtin_derive_fast_path
                                    && let MacroDefKind::BuiltInDerive(_, builtin_derive) = def_id.kind
                                    && crate::builtin_derive::has_builtin_derive_impl(builtin_derive)
                                {
                                    side.deferred_derives_of(ast_id_without_path.upcast())
                                        .push(DeferredDerive {
                                            call_id,
                                            derive: builtin_derive,
                                            module_id: derive.module,
                                            container: derive.container,
                                            depth: derive.depth,
                                            derive_attr_id: *attr_id,
                                            derive_index: idx as u32,
                                            helpers_range: helpers_start..helpers_end,
                                        });
                                } else {
                                    push_resolved(round.resolved, &derive, call_id);
                                    *derive_call_ids.last_mut().expect("pushed above") =
                                        Some(Either::Left(call_id));
                                }
                            } else {
                                round.added.push(derive);
                            }
                        }

                        // We treat the #[derive] macro as an attribute call, but we do not
                        // resolve it for nameres collection. This is just a trick to be able to
                        // resolve the input to derives as proper paths in `Semantics`. Check the
                        // comment in [`builtin_attr_macro`].
                        sink.apply(DeclOp::InitDeriveAttribute {
                            module,
                            adt: ast_id,
                            attr: *attr_id,
                            attr_call: call_id,
                            derive_calls: derive_call_ids,
                        });
                    }
                    None => {
                        let diag = DefDiagnostic::malformed_derive(module, ast_id, *attr_id);
                        sink.apply(DeclOp::Diagnostic(diag));
                    }
                }

                return recollect_without(sink, side, round.added);
            }

            if let MacroDefKind::ProcMacro(_, exp, _) = def.kind {
                // If there's no expander for the proc macro (e.g. because proc macros are
                // disabled, or building the proc macro crate failed), report this and skip
                // expansion like we would if it was disabled
                if let Some(err) = exp.as_expand_error(db, def.krate) {
                    sink.apply(DeclOp::Diagnostic(DefDiagnostic::macro_error(
                        module,
                        ast_id,
                        ModPath::clone(path),
                        err,
                    )));
                    return recollect_without(sink, side, round.added);
                }
            }

            // Clear deferred derives for this item, unfortunately we cannot use them due to the
            // attribute.
            if let Some(deferred_derives) = side.deferred_derives.remove(&ast_id) {
                round.resolved.extend(deferred_derives.into_iter().map(|derive| {
                    (derive.module_id, derive.depth, derive.container, derive.call_id, Some(ast_id))
                }));
            }

            let call_id = attr_call(cx, side, file_ast_id, attr, *attr_id, def, directive.depth);
            sink.apply(DeclOp::ResolvedMacroPath {
                module,
                // clone: the attribute's path, kept for `NameRecords`.
                path: path.clone(),
                bang: false,
                def,
                call: Some(call_id),
            });
            sink.apply(DeclOp::AddAttrMacroInvoc { module, item: ast_id, id: call_id });

            push_resolved(round.resolved, directive, call_id);
            return Resolved::Yes;
        }
    }

    Resolved::No
}

/// The attribute macro call of `attr` on its item, every attribute expanded on the item
/// before it censored (`prev_active_attrs`).
fn attr_call<A: Allocator + Clone>(
    cx: &CollectCx<'_, '_>,
    side: &mut SideTables<A>,
    item: &AstIdWithPath<ast::Item>,
    attr: &Attr,
    attr_id: AttrId,
    def: MacroDefId,
    depth: u32,
) -> MacroCallId {
    let active_attrs = side.prev_active_attrs.entry(item.ast_id).or_default();
    active_attrs.push(attr_id);
    attr_macro_as_call_id(
        cx.db,
        item,
        attr,
        AttrMacroAttrIds::from_many(active_attrs),
        cx.krate,
        def,
        depth,
    )
}

/// Collects the item of an attribute directive again, its attributes up to and including
/// `attr_id` skipped (the old `recollect_without`).
#[allow(clippy::too_many_arguments)]
fn recollect<'db, 'f, S: Sink<'db>, A: Allocator + Clone>(
    cx: &CollectCx<'db, 'f>,
    sink: &mut S,
    side: &mut SideTables<A>,
    added: &mut FrameVec<'f, MacroOp>,
    directive: &MacroOp,
    file_id: HirFileId,
    mod_item: ModItemId,
    attr_id: AttrId,
    tree: TreeId,
) {
    let mod_dir = side.mod_dirs[&directive.module];
    side.skip_attrs.insert(InFile::new(file_id, mod_item), attr_id);
    collect::collect_items(
        cx,
        sink,
        side,
        directive.module,
        tree,
        directive.depth,
        mod_dir,
        &[mod_item],
        directive.container,
    );
    sink.take_macros(added);
}

/// Expands the macros a round resolved as one batch of plane tasks, before
/// `collect_expansion` collects them one at a time in the round's order. An expansion is a
/// query of its call alone, so which task runs it when changes no result, and collecting in the
/// round's order keeps the def map the same whatever the scheduling.
///
/// A proc macro that keeps a session sees its calls in the order rustc makes them, so its
/// calls are left to the collection, in order.
fn expand_round(cx: &CollectCx<'_, '_>, limit: u32, resolved: &[ResolvedCall]) {
    if resolved.len() < 2 {
        return;
    }
    let db = cx.db;
    let in_session = |call: MacroCallId| {
        let def = call.loc(db).def;
        matches!(
            def.kind,
            MacroDefKind::ProcMacro(_, exp, _) if exp.requires_session(db, def.krate)
        )
    };
    let mut calls = Vec::with_capacity_in(resolved.len(), cx.frame);
    calls.extend(
        resolved
            .iter()
            .filter(|&&(_, depth, ..)| depth <= limit)
            .map(|&(_, _, _, call, _)| call)
            .filter(|&call| !in_session(call)),
    );
    collect::expand_all(db, cx.krate, &calls);
}

/// The first `Attr` directive's position, for `RoundOutcome::first_attr`.
pub(crate) fn first_attr<'p>(pending: impl IntoIterator<Item = &'p MacroOp>) -> Option<u32> {
    pending
        .into_iter()
        .position(|it| matches!(it.kind, MacroOpKind::Attr { .. }))
        .map(|at| u32::try_from(at).expect("a module has 2^32 macro directives"))
}

/// `reseed_with_unresolved_attribute` of `pending[index]`: reports the attribute unresolved,
/// skips it and every attribute before it on the item, and collects the item as if they were
/// not there.
pub(crate) fn reseed_at<'db, 'f, S: Sink<'db>, A: Allocator + Clone>(
    cx: &CollectCx<'db, 'f>,
    sink: &mut S,
    side: &mut SideTables<A>,
    pending: &mut FrameVec<'f, MacroOp>,
    index: u32,
) {
    cov_mark::hit!(unresolved_attribute_fallback);
    sink.take_macros(pending);
    let at = index as usize;
    let directive = &pending[at];
    let &MacroOp { module, depth, container, .. } = directive;
    let MacroOpKind::Attr { ast_id, attr_id, attr, mod_item, tree, .. } = &directive.kind else {
        panic!("nameres facts: the reseed of {module:?} names directive {at}, which is not an attribute")
    };
    let (mod_item, tree, attr_id) = (*mod_item, *tree, *attr_id);
    sink.apply(DeclOp::Diagnostic(DefDiagnostic::unresolved_macro_call(
        module,
        MacroCallKind::Attr {
            ast_id: ast_id.ast_id,
            attr_args: None,
            censored_attr_ids: AttrMacroAttrIds::from_one(attr_id),
        },
        ModPath::clone(&attr.path),
    )));
    side.skip_attrs.insert(ast_id.ast_id.with_value(mod_item), attr_id);

    let mod_dir = side.mod_dirs[&module];
    collect::collect_items(cx, sink, side, module, tree, depth, mod_dir, &[mod_item], container);
    sink.take_macros(pending);
    pending.swap_remove(at);
}

/// `finish`'s unresolved-macro diagnostics for `pending`, as `Diagnostic` ops through `sink`.
/// Attribute directives are diagnosed by their reseed.
pub(crate) fn unresolved_macro_diagnostics<'db, 'p, S: Sink<'db>>(
    cx: &CollectCx<'db, '_>,
    sink: &mut S,
    pending: impl IntoIterator<Item = &'p MacroOp>,
) {
    let db = cx.db;
    for directive in pending {
        let module = directive.module;
        match &directive.kind {
            MacroOpKind::FnLike { ast_id, expand_to, ctxt: call_site } => {
                // FIXME: we shouldn't need to re-resolve the macro here just to get the
                // unresolved error!
                let view: &S = sink;
                let resolved = macro_call_as_call_id(
                    db,
                    ast_id.ast_id,
                    &ast_id.path,
                    *call_site,
                    *expand_to,
                    cx.krate,
                    directive.depth,
                    view.def_map().recursion_limit(),
                    |path| resolve_macro(view, module, path, MacroSubNs::Bang).map(|(_, it)| it),
                    // Global until `macro_call_as_call_id` takes `Vec<EagerCall, A>`.
                    // alloc: none unless an eager input holds macro calls; only the error is kept
                    &mut Vec::new(),
                );
                if let Err(UnresolvedMacro { path }) = resolved {
                    sink.apply(DeclOp::Diagnostic(DefDiagnostic::unresolved_macro_call(
                        module,
                        MacroCallKind::FnLike { ast_id: ast_id.ast_id, expand_to: *expand_to, eager: None },
                        path,
                    )));
                }
            }
            MacroOpKind::Derive { ast_id, derive_attr, derive_pos, derive_macro_id, .. } => {
                sink.apply(DeclOp::Diagnostic(DefDiagnostic::unresolved_macro_call(
                    module,
                    MacroCallKind::Derive {
                        ast_id: ast_id.ast_id,
                        derive_attr_index: *derive_attr,
                        derive_index: *derive_pos as u32,
                        derive_macro_id: *derive_macro_id,
                    },
                    ModPath::clone(&ast_id.path),
                )));
            }
            MacroOpKind::Attr { .. } => {}
        }
    }
}

// The step.

/// One module's macro resolution at one round (`nameres-core.md` §5). By the round's kind:
/// `Normal` and `EagerWithErrors` run [`resolve_round`]; `Reseed` runs [`reseed_at`] in the
/// module it names; `Final` runs nothing. The driver asks it only of a module the round steps
/// (`RoundModules::STEP`); asked of another, it carries the module's directives and tables on
/// without building a view.
pub(crate) fn macro_step(db: &Db, frame: &Frame, key: FactKey) -> MacroStep {
    let (module, krate, round) = (key.module(db), key.krate(db), key.round(db));
    let round_key = RoundKey::new(db, krate, round);
    let modules = queries::round_modules(db, round_key);
    let Some(at) = modules.at(module) else {
        panic!("nameres facts: a macro step of {module:?} at round {round}, which the round does not have")
    };
    let row = &modules.rows[at as usize];
    let kind = queries::round_kind(db, round_key);
    let carried = Carried::at(db, krate, modules, at);

    let reseed = match kind {
        RoundKind::Reseed { module: reseeded, index } if reseeded == module => Some(index),
        _ => None,
    };
    let resolves = matches!(kind, RoundKind::Normal | RoundKind::EagerWithErrors);
    if reseed.is_none() && (!resolves || carried.is_empty()) {
        return MacroStep {
            ops: OpLog::default(),
            pending: carried.pending_box(),
            side: carried.side(),
            new_modules: Box::default(),
            progress: false,
            deferred_eager: false,
        };
    }

    let cx = collect::cx(db, frame, krate, false);
    let mut pending = carried.pending_in(frame);
    let mut side = carried.side();
    // clone: the step writes its module's tables, so it makes its own copy of them, once.
    let tables = Arc::make_mut(&mut side);
    let mut sink = StepSink::new(&cx, round_key, at, row);
    let (progress, deferred_eager) = match reseed {
        Some(index) => {
            reseed_at(&cx, &mut sink, tables, &mut pending, index);
            (true, false)
        }
        None => {
            let result = resolve_round(
                &cx,
                &mut sink,
                tables,
                &mut pending,
                kind == RoundKind::EagerWithErrors,
            );
            (result.progress, result.deferred_eager)
        }
    };
    sink.into_step(pending, side, progress, deferred_eager)
}

/// Where a step's directives and tables come from (`nameres-core.md` §3, "Pending directives",
/// "Side tables"): the module's last step, or, before its first, what it started with. A
/// module gains no directive after its start (`DeclOp::clone_resolution_op` and
/// `clone_root_op` carry none), so a round it does not step at leaves them as they were.
pub(crate) struct Carried<'a> {
    module: ModuleId,
    /// The directives the module's last step left.
    before: &'a [MacroOp],
    /// Before its first step, the log whose `Macro` ops of the module it started with: its
    /// round-0 log, or what it gained at the round it was declared at.
    log: Option<&'a OpLog>,
    /// Its tables so far; `None` for a module declared after round 0 that has not stepped,
    /// which starts from empty tables and the directory its `SetModDir` in `log` names.
    side: Option<&'a Arc<SideTables>>,
}

impl<'a> Carried<'a> {
    /// What the module at `at` of `modules` (a round of `krate`) has as the round starts.
    pub(crate) fn at(db: &'a Db, krate: Crate, modules: &RoundModules, at: u32) -> Carried<'a> {
        let module = modules.order[at as usize];
        if let Some(last) = modules.last_step(at) {
            let last = queries::macro_step(db, FactKey::new(db, module, krate, last));
            return Carried { module, before: &last.pending, log: None, side: Some(&last.side) };
        }
        let row = &modules.rows[at as usize];
        if row.since == 0 {
            let decl = row.decl.unwrap_or_else(|| {
                panic!("nameres facts: {module:?} is a module of round 0 without its round-0 key")
            });
            let decls = queries::module_decls(db, decl);
            return Carried { module, before: &[], log: Some(&decls.ops), side: Some(&decls.side) };
        }
        let gained = &queries::module_round(db, FactKey::new(db, module, krate, row.since)).ops;
        Carried { module, before: &[], log: Some(gained), side: None }
    }

    fn new_directives(&self) -> impl Iterator<Item = &'a MacroOp> + 'a {
        let module = self.module;
        self.log.into_iter().flat_map(|log| log.iter()).filter_map(move |op| match op {
            DeclOp::Macro(it) if it.module == module => Some(it),
            _ => None,
        })
    }

    /// The directives the module has: the last step's, then the new ones.
    pub(crate) fn directives(&self) -> impl Iterator<Item = &'a MacroOp> + 'a {
        self.before.iter().chain(self.new_directives())
    }

    /// The module's tables as they are.
    pub(crate) fn tables(&self) -> Arc<SideTables> {
        self.side()
    }

    fn is_empty(&self) -> bool {
        self.before.is_empty() && self.new_directives().next().is_none()
    }

    fn len(&self) -> usize {
        self.before.len() + self.new_directives().count()
    }

    /// The list a step works on, in its frame: the last step's directives, then the new ones.
    fn pending_in<'f>(&self, frame: &'f Frame) -> FrameVec<'f, MacroOp> {
        let mut pending = Vec::with_capacity_in(self.len(), frame);
        // clone: the directives are the values of earlier queries; the step owns its own list.
        pending.extend(self.before.iter().cloned());
        // clone: as above.
        pending.extend(self.new_directives().cloned());
        pending
    }

    /// The directives of a step that runs nothing, as its value, at exact size.
    fn pending_box(&self) -> Box<[MacroOp]> {
        // alloc: the step's value, exact; none when the module has no directive
        let mut pending = Vec::with_capacity(self.len());
        // clone: the step's value carries the directives on; the earlier values keep theirs.
        pending.extend(self.directives().cloned());
        pending.into_boxed_slice()
    }

    /// The step's tables: the carried ones, shared until the step writes them.
    fn side(&self) -> Arc<SideTables> {
        match self.side {
            // clone: a reference count; a step that writes the tables copies them then.
            Some(side) => Arc::clone(side),
            None => {
                let mut side = SideTables::default();
                for op in self.log.into_iter().flat_map(OpLog::iter) {
                    if let &DeclOp::SetModDir { module, dir } = op
                        && module == self.module
                    {
                        side.mod_dirs.insert(module, dir);
                    }
                }
                // alloc: once per module an expansion declares, at its first step.
                Arc::new(side)
            }
        }
    }
}

/// The sink of a macro step: a view of one module of a round, the modules its expansions
/// declare, and the ops they make.
struct StepSink<'db, 'f> {
    db: &'db Db,
    frame: &'f Frame,
    /// The step's module.
    module: ModuleId,
    round: u32,
    /// The view: the module and those it declares in `ModulesSlot::Unit`'s own, every other
    /// module of the round through `Rows::Round`.
    def_map: DefMap,
    local: LocalDefMap,
    /// The crate env the round starts from, and the one the step's crate-level ops made.
    env: &'db CrateEnv,
    env_now: Option<CrateEnv>,
    /// The std prelude the crate's own `#[prelude_import]` resolved to (core's `prelude::v1`),
    /// as each component installs it: the env has only the injected one, and this one stays
    /// over it when an expansion changes the env.
    own_prelude: Option<(ModuleId, Option<UseId>)>,
    /// The glob marks of the module's scope, read off it at the first `Update`.
    marks: Option<PerNsGlobImports<&'f Frame>>,
    /// Every op but the module's own directives; [`StepSink::into_step`] makes the [`OpLog`].
    log: FrameVec<'f, DeclOp>,
    /// The module's own new directives, for `take_macros`.
    macros: FrameVec<'f, MacroOp>,
    /// The plain imports the step collected: module, the name it binds, its op in `log`.
    imports: FrameVec<'f, (ModuleId, Name, u32)>,
    /// The module's plain imports the round's fixed point left unresolved.
    unresolved: &'db [(Name, ModPath)],
    new_modules: FrameVec<'f, (ModuleId, ModuleRow)>,
}

impl<'db, 'f> StepSink<'db, 'f> {
    /// The step of the module at `at` of `round`. A module can step at a round that does not run
    /// its component (its calls wait on a module that does not change): it starts from its
    /// facts as they last changed, and the imports its component's last run left unresolved.
    fn new(cx: &CollectCx<'db, 'f>, round: RoundKey, at: u32, row: &ModuleRow) -> StepSink<'db, 'f> {
        let db = cx.db;
        let frame = cx.frame;
        let krate = round.krate(db);
        let module = queries::round_modules(db, round).order[at as usize];
        let handle = queries::fact_handle(db, queries::fact_key(db, round, at));
        let ran = FactKey::new(db, module, krate, queries::ran_by(db, round, at));
        let scope = <ModuleScope<'_> as blaze_query::db::FromId>::from_id(handle);
        let data = ModuleData::with_scope(
            row.origin,
            row.visibility,
            row.parent,
            // One module per step that has a directive (`nameres-core.md` §5).
            // clone: the step's own module, which its expansions write.
            scope.scope(db).clone(),
            // clone: as above.
            scope.children(db).clone(),
        );
        let mut own = ModulesMap::new();
        own.insert(module, data);
        let rows = RowsKey::new(db, Rows::Round(round));
        let env = queries::round_env(db, round).env(db);
        let mut def_map = DefMap {
            krate,
            block: None,
            root: queries::crate_root(db, krate),
            modules: ModulesSlot::Unit { own, rows: blaze_query::db::AsId::as_id(&rows) },
            prelude: None,
            macro_use_prelude: HashMap::default(),
            derive_helpers_in_scope: HashMap::default(),
            session_macros: false,
            names: NameRecords::default(),
            diagnostics: Vec::new(), // alloc: empty, a view keeps no diagnostics
            diagnostics_by_module: DiagnosticsByModule::default(),
            records: None,
            // clone: one count of the crate's shared data, once per step.
            data: cx.data.clone(),
        };
        env.install(&mut def_map);
        let own_prelude = super::unit::round_prelude(db, round);
        if own_prelude.is_some() {
            def_map.prelude = own_prelude;
        }
        StepSink {
            db,
            frame,
            module,
            round: round.round(db),
            def_map,
            local: env.local_def_map(),
            env,
            env_now: None,
            own_prelude,
            marks: None,
            log: Vec::new_in(frame),
            macros: Vec::new_in(frame),
            imports: Vec::new_in(frame),
            unresolved: queries::unresolved_named(db, ran),
            new_modules: Vec::new_in(frame),
        }
    }

    fn into_step(
        self,
        pending: FrameVec<'_, MacroOp>,
        side: Arc<SideTables>,
        progress: bool,
        deferred_eager: bool,
    ) -> MacroStep {
        MacroStep {
            ops: OpLog::from_frame(self.log),
            // A frame vector's `IntoIter` is `TrustedLen`: each box is one exact allocation.
            // alloc: the step's value
            pending: pending.into_iter().collect(),
            // alloc: as above
            new_modules: self.new_modules.into_iter().collect(),
            side,
            progress,
            deferred_eager,
        }
    }

    fn own(&self) -> &ModulesMap {
        match &self.def_map.modules {
            ModulesSlot::Unit { own, .. } => own,
            ModulesSlot::Building(_)
            | ModulesSlot::Collected(_)
            | ModulesSlot::Maintained { .. }
            | ModulesSlot::Published(_) => {
                unreachable!("a macro step's view is a resolver view")
            }
        }
    }

    fn holds(&self, module: ModuleId) -> bool {
        self.own().contains_key(&module)
    }

    fn scope(&self, module: ModuleId) -> &ItemScope {
        self.own()[module].scope(self.db)
    }

    fn scope_mut(&mut self, module: ModuleId) -> &mut ItemScope {
        self.def_map.modules.building()[module].scope_mut()
    }

    fn env(&self) -> &CrateEnv {
        self.env_now.as_ref().unwrap_or(self.env)
    }

    /// A module an expansion declares joins the view, its textual scope its parent's where it
    /// is declared (`push_child_module`). Its id was made by the collection running inside this
    /// query (`collect.rs`), under `nameres-core.md` §3's invariant: it goes only into this
    /// step's value and the queries and def map built from it.
    fn declare(
        &mut self,
        module: ModuleId,
        origin: ModuleOrigin,
        visibility: Visibility,
        parent: Option<ModuleId>,
        inherited: &[(Name, Box<[MacroId]>)],
    ) {
        if self.holds(module) {
            return;
        }
        let mut data = ModuleData::new(origin, visibility, parent);
        for (name, macs) in inherited {
            for &mac in macs {
                data.scope_mut().define_legacy_macro(*name, mac);
            }
        }
        self.def_map.modules.building().insert(module, data);
        let row = ModuleRow { origin, visibility, parent, since: self.round + 1, decl: None };
        self.new_modules.push((module, row));
    }

    /// `DefCollector::update` of a module the view holds, as far as the module itself goes:
    /// its importers are other modules, which see the op when the next round replays it.
    fn update(
        &mut self,
        module: ModuleId,
        resolutions: &[(Option<Name>, PerNs)],
        vis: Visibility,
        import: Option<ImportOrExternCrate>,
    ) {
        assert!(
            !matches!(import, Some(ImportOrExternCrate::Glob(_))),
            "nameres facts: a glob import collected in the macro step of {:?}",
            self.module
        );
        let db = self.db;
        if self.marks.is_none() {
            // Before the first write, so the scope is the one the round left.
            let marks = PerNsGlobImports::of_scope_in(self.module, self.scope(self.module), self.frame);
            self.marks = Some(marks);
        }
        for &(name, res) in resolutions {
            match name {
                Some(name) => {
                    let mut defs = res;
                    if let Some(def) = defs.types.as_mut() {
                        def.vis = def.vis.min(db, vis, &self.def_map).unwrap_or(vis);
                    }
                    if let Some(def) = defs.values.as_mut() {
                        def.vis = def.vis.min(db, vis, &self.def_map).unwrap_or(vis);
                    }
                    if let Some(def) = defs.macros.as_mut() {
                        def.vis = def.vis.min(db, vis, &self.def_map).unwrap_or(vis);
                    }
                    let marks = self.marks.as_mut().expect("read off the scope above");
                    self.def_map.modules.building()[module].scope_mut().push_res_with_import(
                        marks,
                        (module, name),
                        defs,
                        import,
                    );
                }
                None => {
                    let tr = match res.take_types_full() {
                        Some(Item { def: ModuleDefId::TraitId(tr), vis: _, import: _ }) => tr,
                        Some(other) => {
                            tracing::debug!("non-trait `_` import of {:?}", other);
                            continue;
                        }
                        None => continue,
                    };
                    let should_update = match self.scope(module).unnamed_trait_vis(tr) {
                        None => true,
                        Some(old_vis) => {
                            let max_vis =
                                old_vis.max(db, vis, &self.def_map).unwrap_or_else(|| {
                                    panic!("`Tr as _` imports with unrelated visibilities {old_vis:?} and {vis:?} (trait {tr:?})");
                                });
                            max_vis != old_vis
                        }
                    };
                    if should_update {
                        self.scope_mut(module).push_unnamed_trait_through(tr, vis, import);
                    }
                }
            }
        }
    }

    /// What `op` does to the view.
    fn apply_view(&mut self, op: &DeclOp) {
        if let Some(module) = op.target()
            && !matches!(op, DeclOp::Init { .. })
            && !self.holds(module)
        {
            // A module of the round the view reads as a fact (the crate root a
            // `#[macro_export]` writes): it gains the op at the next round.
            return;
        }
        match *op {
            DeclOp::Init { module, origin, visibility, parent, ref inherited } => {
                self.declare(module, origin, visibility, parent, inherited);
            }
            DeclOp::PushChild { parent, name, child } => {
                self.def_map.modules.building()[parent].children_mut().insert(name, child);
            }
            DeclOp::Declare { module, def } => self.scope_mut(module).declare(def),
            DeclOp::Update { module, ref resolutions, vis, import } => {
                self.update(module, resolutions, vis, import);
            }
            DeclOp::DefineImpl { module, imp, is_trait_impl, is_reservation } => {
                self.scope_mut(module).define_impl(imp, is_trait_impl, is_reservation);
            }
            DeclOp::DefineBuiltinDeriveImpl { module, imp } => {
                self.scope_mut(module).define_builtin_derive_impl(imp);
            }
            DeclOp::DefineExternBlock { module, id } => self.scope_mut(module).define_extern_block(id),
            DeclOp::DefineExternCrateDecl { module, id } => {
                self.scope_mut(module).define_extern_crate_decl(id);
            }
            DeclOp::DefineUseDecl { module, id } => self.scope_mut(module).define_use_decl(id),
            DeclOp::DefineUnnamedConst { module, id } => {
                self.scope_mut(module).define_unnamed_const(id);
            }
            DeclOp::DefineLegacyMacro { module, name, mac } => {
                self.scope_mut(module).define_legacy_macro(name, mac);
            }
            DeclOp::AddMacroInvoc { module, call, id } => {
                self.scope_mut(module).add_macro_invoc(call, id);
            }
            DeclOp::AddAttrMacroInvoc { module, item, id } => {
                self.scope_mut(module).add_attr_macro_invoc(item, id);
            }
            DeclOp::SetDeriveMacroInvoc { module, adt, call, attr, idx } => {
                self.scope_mut(module).set_derive_macro_invoc(adt, call, attr, idx);
            }
            DeclOp::InitDeriveAttribute { module, adt, attr, attr_call, ref derive_calls } => {
                // clone: inline up to four ids; the view and the log each keep the list.
                let derive_calls = derive_calls.clone();
                self.scope_mut(module).init_derive_attribute(adt, attr, attr_call, derive_calls);
            }
            DeclOp::ExternPrelude { .. } | DeclOp::MacroUsePrelude { .. } | DeclOp::Prelude { .. } => {
                if let Some(next) = self.env().after(op) {
                    self.local = next.local_def_map();
                    next.install(&mut self.def_map);
                    if self.own_prelude.is_some() {
                        self.def_map.prelude = self.own_prelude;
                    }
                    self.env_now = Some(next);
                }
            }
            DeclOp::Splice { .. }
            | DeclOp::SetModDir { .. }
            | DeclOp::Import(_)
            | DeclOp::Macro(_)
            | DeclOp::UnresolvedExternCrate { .. }
            | DeclOp::ExportedDerive { .. }
            | DeclOp::ProcMacroFn { .. }
            | DeclOp::NotIntroducing(_)
            | DeclOp::ResolvedMacroPath { .. }
            | DeclOp::DeriveHelperUse(_)
            | DeclOp::Diagnostic(_)
            | DeclOp::SessionMacros => {}
        }
    }
}

impl<'db> Sink<'db> for StepSink<'db, '_> {
    fn db(&self) -> &'db Db {
        self.db
    }

    fn def_map(&self) -> &DefMap {
        &self.def_map
    }

    fn local_def_map(&self) -> &LocalDefMap {
        &self.local
    }

    fn apply(&mut self, op: DeclOp) {
        let op = match op {
            DeclOp::Macro(directive) if directive.module == self.module => {
                self.macros.push(directive);
                return;
            }
            op => op,
        };
        self.apply_view(&op);
        if let DeclOp::Import(import) = &op
            && import.source.kind == ImportKind::Plain
            && let Some(name) = import.bound_name()
        {
            let at = u32::try_from(self.log.len()).expect("a step makes 2^32 ops");
            self.imports.push((import.module, name, at));
        }
        self.log.push(op);
    }

    fn find_unresolved_named<T>(
        &self,
        module: ModuleId,
        name: Name,
        mut f: impl FnMut(&ModPath) -> Option<T>,
    ) -> Option<T> {
        if module == self.module {
            for (bound, path) in self.unresolved {
                if *bound == name
                    && let Some(it) = f(path)
                {
                    return Some(it);
                }
            }
        }
        for &(importer, bound, at) in &self.imports {
            if importer != module || bound != name {
                continue;
            }
            let DeclOp::Import(import) = &self.log[at as usize] else {
                unreachable!("`imports` points at the step's `Import` ops")
            };
            if let Some(it) = f(&import.path) {
                return Some(it);
            }
        }
        None
    }

    fn child(&mut self, request: ChildRequest<'_>) -> ChildMode {
        self.declare(
            request.child,
            request.origin,
            request.visibility,
            Some(request.parent),
            request.inherited,
        );
        ChildMode::Inline
    }

    fn take_macros(&mut self, into: &mut FrameVec<'_, MacroOp>) {
        into.extend(self.macros.drain(..));
    }
}

#[cfg(test)]
mod tests {
    use base_db::Crate;
    use hir_expand::fixture;

    use super::*;
    use crate::{
        nameres::{diagnostics::DefDiagnosticKind, facts::queries::module_round},
        test_db::{TestDB, TestDBExt},
    };

    /// The crate root's round-0 child named `name`, or the crate root for `""`.
    fn module_named(db: &TestDB, krate: Crate, name: &str) -> ModuleId {
        let root = queries::crate_root(db, krate);
        if name.is_empty() {
            return root;
        }
        queries::module_decls(db, queries::root_decl(db, krate))
            .ops
            .iter()
            .find_map(|op| match op {
                DeclOp::PushChild { parent, name: it, child } if *parent == root && it.as_str() == name => {
                    Some(*child)
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("the crate root declares no module `{name}`"))
    }

    fn step(db: &TestDB, module: ModuleId, krate: Crate, round: u32) -> &MacroStep {
        queries::macro_step(db, FactKey::new(db, module, krate, round))
    }

    fn binds(op: &DeclOp, module: ModuleId, name: &str) -> bool {
        matches!(op, DeclOp::Update { module: it, resolutions, .. }
            if *it == module && resolutions.iter().any(|(n, _)| n.is_some_and(|n| n.as_str() == name)))
    }

    #[test]
    fn a_call_of_another_modules_macro_resolves_at_round_zero_and_its_items_join_round_one() {
        let fx = fixture::db(
            r#"
//- /lib.rs
mod a;
mod b;
//- /a.rs
#[macro_export]
macro_rules! make { () => { pub struct Made; } }
//- /b.rs
crate::make!();
"#,
        );
        let _ = fx.files().to_vec();
        let db = fx.into_db();
        let krate = db.fetch_test_crate();
        let b = module_named(&db, krate, "b");
        let first = step(&db, b, krate, 0);
        assert!(first.progress, "`crate::make!` resolves against round 0's facts");
        assert!(first.pending.is_empty(), "{:?}", first.pending);
        assert!(first.ops.iter().any(|op| matches!(op, DeclOp::AddMacroInvoc { module, .. } if *module == b)));
        assert!(first.ops.iter().any(|op| binds(op, b, "Made")), "{:?}", first.ops);
        let gained = module_round(&db, FactKey::new(&db, b, krate, 1));
        assert!(gained.ops.iter().any(|op| binds(op, b, "Made")), "{:?}", gained.ops);
    }

    #[test]
    fn an_eager_call_naming_a_macro_not_yet_defined_is_deferred() {
        let fx = fixture::db(
            r#"
//- /lib.rs
#[rustc_builtin_macro]
macro_rules! concat { () => {} }
concat!(not_yet!());
"#,
        );
        let _ = fx.files().to_vec();
        let db = fx.into_db();
        let krate = db.fetch_test_crate();
        let root = module_named(&db, krate, "");
        let first = step(&db, root, krate, 0);
        assert!(first.deferred_eager, "`not_yet!` may come from an expansion still to run");
        assert!(!first.progress);
        assert_eq!(first.pending.len(), 1, "{:?}", first.pending);
        assert!(
            !first.ops.iter().any(|op| matches!(op, DeclOp::AddMacroInvoc { .. })),
            "a deferred call records no invocation: {:?}",
            first.ops
        );
    }

    #[test]
    fn an_eager_calls_nested_calls_are_recorded_after_it_in_order() {
        let fx = fixture::db(
            r#"
//- /lib.rs
#[rustc_builtin_macro]
#[macro_export]
macro_rules! concat { () => {} }
mod a;
mod b;
//- /a.rs
#[macro_export]
macro_rules! one { () => { "1" } }
#[macro_export]
macro_rules! two { () => { "2" } }
//- /b.rs
crate::concat!(crate::one!(), crate::two!());
"#,
        );
        let _ = fx.files().to_vec();
        let db = fx.into_db();
        let krate = db.fetch_test_crate();
        let b = module_named(&db, krate, "b");
        let first = step(&db, b, krate, 0);
        assert!(!first.deferred_eager, "both nested macros resolve at round 0");
        let invocs: Vec<MacroCallId> = first
            .ops
            .iter()
            .filter_map(|op| match op {
                DeclOp::AddMacroInvoc { module, id, .. } if *module == b => Some(*id),
                _ => None,
            })
            .collect();
        assert_eq!(invocs.len(), 3, "the call, then its two nested calls: {:?}", first.ops);
        assert!(invocs[0].is_eager(&db), "the outer call is recorded first");
        let nested: Vec<String> = invocs[1..]
            .iter()
            .map(|id| id.parse_macro_expansion(&db).value.0.root().to_string())
            .collect();
        assert_eq!(nested, ["\"1\"", "\"2\""]);
    }

    #[test]
    fn an_unresolved_attribute_is_found_and_its_reseed_collects_the_item() {
        let fx = fixture::db(
            r#"
//- /lib.rs
#[not_an_attribute]
struct S;
"#,
        );
        let _ = fx.files().to_vec();
        let db = fx.into_db();
        let krate = db.fetch_test_crate();
        let root = module_named(&db, krate, "");
        let key = FactKey::new(&db, root, krate, 0);
        let first = queries::macro_step(&db, key);
        assert!(!first.progress);
        assert_eq!(first_attr(first.pending.iter()), Some(0), "{:?}", first.pending);

        let step = blaze_mem::frame::task(blaze_mem::Tag::Nameres, |frame| {
            let cx = collect::cx(&db, frame, krate, false);
            let round = RoundKey::new(&db, krate, 0);
            let modules = queries::round_modules(&db, round);
            let at = modules.at(root).expect("the root is a module of round 0");
            let mut sink = StepSink::new(&cx, round, at, &modules.rows[at as usize]);
            let mut pending = Vec::with_capacity_in(first.pending.len(), frame);
            pending.extend(first.pending.iter().cloned());
            let mut side = Arc::clone(&first.side);
            reseed_at(&cx, &mut sink, Arc::make_mut(&mut side), &mut pending, 0);
            assert!(pending.is_empty(), "{pending:?}");
            assert!(!side.skip_attrs.is_empty(), "the attribute is skipped from now on");
            sink.into_step(pending, side, true, false)
        });
        assert!(
            step.ops.iter().any(|op| matches!(op, DeclOp::Diagnostic(it)
                if matches!(it.kind, DefDiagnosticKind::UnresolvedMacroCall { .. }))),
            "{:?}",
            step.ops
        );
        assert!(step.ops.iter().any(|op| binds(op, root, "S")), "{:?}", step.ops);
    }

    #[test]
    fn a_step_with_nothing_pending_makes_no_op() {
        let fx = fixture::db(
            r#"
//- /lib.rs
mod a;
pub struct X;
//- /a.rs
pub fn f() {}
"#,
        );
        let _ = fx.files().to_vec();
        let db = fx.into_db();
        let krate = db.fetch_test_crate();
        let modules = queries::round_modules(&db, RoundKey::new(&db, krate, 0));
        assert_eq!(modules.stepping().count(), 0, "no module has a directive, so none steps");
        for &module in modules.order.iter() {
            let it = step(&db, module, krate, 0);
            assert!(it.ops.is_empty(), "{module:?}: {:?}", it.ops);
            assert!(it.pending.is_empty() && it.new_modules.is_empty());
            assert!(!it.progress && !it.deferred_eager);
            assert!(it.side.mod_dirs.contains_key(&module), "the tables are carried on");
            let decl = modules.row(module).and_then(|row| row.decl).expect("a module of round 0");
            assert!(
                Arc::ptr_eq(&it.side, &queries::module_decls(&db, decl).side),
                "{module:?}: a step with nothing to do shares its tables, it does not copy them"
            );
        }
    }
}
