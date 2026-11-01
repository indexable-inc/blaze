//! Transforms syntax into paths in the store's columns, ideally with accounting for hygiene

#[cfg(test)]
mod tests;

use crate::{
    expr_store::{
        lower::{ExprCollector, ImplTraitLowering},
        path::{GenericArg, GenericArgsIdx, GenericArgsParentheses, PathIdx, slot, unslot},
    },
    type_ref::{LifetimeRef, LifetimeRefId, TypeRef},
};

use hir_expand::{
    mod_path::{ModPath, PathKind, resolve_crate_root},
    name::{AsName, Name},
};
use intern::sym;
use smallvec::SmallVec;
use syntax::{
    AstPtr, ast as tag,
    view::{self as ast, AstNode, HasGenericArgs, HasTypeBounds},
};

#[cfg(test)]
thread_local! {
    /// This is used to test `hir_segment_to_ast_segment_view()`. It's a hack, but it makes testing much easier.
    pub(super) static SEGMENT_LOWERING_MAP: std::cell::RefCell<blaze_hash::HashMap<AstPtr<tag::PathSegment>, usize>> = std::cell::RefCell::default();
    /// Whether lowering fills `SEGMENT_LOWERING_MAP`: only for its tests, so that a budget
    /// test's lowering allocates what a build's does.
    pub(super) static RECORD_SEGMENTS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Converts an `ast::Path` to a path of the store's columns. Works with use trees.
/// It correctly handles `$crate` based path from macro call.
// If you modify the logic of the lowering, make sure to check if `hir_segment_to_ast_segment_view()`
// also needs an update.
pub(super) fn lower_path(
    collector: &mut ExprCollector<'_>,
    path: ast::Path<'_>,
    impl_trait_lower_fn: &mut ImplTraitLowering<'_>,
) -> Option<PathIdx> {
    let segments_base = collector.stacks.path_segments.len();
    let args_base = collector.stacks.path_generic_args.len();
    let lowered =
        lower_path_on_stacks(collector, path, impl_trait_lower_fn, segments_base, args_base);
    collector.stacks.path_segments.truncate(segments_base);
    collector.stacks.path_generic_args.truncate(args_base);
    lowered
}

/// `lower_path` with the segments (last first while walking up the qualifiers) and the
/// segments' slot words ([`slot`]) on the collector's stacks above `s0` and `g0`. A path lowered
/// inside this one (in a generic arg or a `<T as Trait>`) pushes and pops above them. The
/// generic arguments are rows of the store's columns by then; the path is pushed last.
fn lower_path_on_stacks(
    collector: &mut ExprCollector<'_>,
    mut path: ast::Path<'_>,
    impl_trait_lower_fn: &mut ImplTraitLowering<'_>,
    s0: usize,
    g0: usize,
) -> Option<PathIdx> {
    let mut kind = PathKind::Plain;
    let mut type_anchor = None;
    #[cfg(test)]
    let mut ast_segments = Vec::new(); // alloc: tests only, for `SEGMENT_LOWERING_MAP`
    #[cfg(test)]
    let mut ast_segments_offset = 0;
    #[allow(unused_mut)]
    let mut push_segment = |_segment: ast::PathSegment<'_>, segments: &mut Vec<Name>, name| {
        #[cfg(test)]
        if RECORD_SEGMENTS.get() {
            ast_segments.push(AstPtr::<tag::PathSegment>::of(_segment));
        }
        segments.push(name);
    };

    let old_lifetimes_constrained_by_input = if collector.is_argument_lt_bound_scope() {
        Some(std::mem::take(&mut collector.named_lifetime_store.lifetimes_constrained_by_input))
    } else {
        None
    };

    loop {
        let Some(segment) = path.segment() else {
            collector.stacks.path_segments.push(Name::missing());
            // We can end up here if for `path::`
            match qualifier(path) {
                Some(it) => {
                    path = it;
                    continue;
                }
                None => break,
            }
        };

        if segment.coloncolon_token().is_some() {
            debug_assert!(path.qualifier().is_none()); // this can only occur at the first segment
            kind = PathKind::Abs;
        }

        match segment.kind()? {
            ast::PathSegmentKind::Name(name_ref) => {
                if name_ref.text() == "$crate" {
                    if path.qualifier().is_some() {
                        // FIXME: Report an error.
                        return None;
                    }
                    break kind = resolve_crate_root(
                        collector.db,
                        collector.expander.ctx_for_range(name_ref.syntax().text_range()),
                    )
                    .map(PathKind::DollarCrate)
                    .unwrap_or(PathKind::Crate);
                }
                let name = name_ref.as_name();
                let args = segment
                    .generic_arg_list()
                    .and_then(|it| collector.lower_generic_args(it, impl_trait_lower_fn))
                    .or_else(|| {
                        collector.lower_generic_args_from_fn_path(
                            segment.parenthesized_arg_list(),
                            segment.ret_type(),
                            impl_trait_lower_fn,
                        )
                    })
                    .or_else(|| {
                        segment.return_type_syntax().map(|_| collector.push_return_type_notation())
                    });
                if args.is_some() {
                    let segments = collector.stacks.path_segments.len() - s0;
                    collector.stacks.path_generic_args.resize(g0 + segments, 0);
                    collector.stacks.path_generic_args.push(slot(args));
                }
                push_segment(segment, &mut collector.stacks.path_segments, name);
            }
            ast::PathSegmentKind::SelfTypeKw => {
                push_segment(
                    segment,
                    &mut collector.stacks.path_segments,
                    Name::new_symbol_root(sym::Self_),
                );
            }
            ast::PathSegmentKind::Type { type_ref, trait_ref } => {
                debug_assert!(path.qualifier().is_none()); // this can only occur at the first segment

                let type_ref = type_ref?;
                let self_type = collector.for_path_type_projection(|collector| {
                    collector.lower_type_ref(type_ref, impl_trait_lower_fn)
                });

                match trait_ref {
                    // <T>::foo
                    None => {
                        type_anchor = Some(self_type);
                        kind = PathKind::Plain;
                    }
                    // <T as Trait<A>>::Foo desugars to Trait<Self=T, A>::Foo
                    Some(trait_ref) => {
                        let path = collector.for_path_type_projection(|collector| {
                            collector.lower_path_view(trait_ref.path()?, impl_trait_lower_fn)
                        })?;
                        let ptr = AstPtr::<tag::PathType>::of(trait_ref).upcast();
                        let Some(mod_path) = collector.store.path_at(path).mod_path().copied()
                        else {
                            collector.alloc_path(path, ptr);
                            return None;
                        };
                        let num_segments = mod_path.segments().len();
                        kind = mod_path.kind;

                        collector.stacks.path_segments.extend(mod_path.segments().iter().cloned().rev());
                        #[cfg(test)]
                        {
                            ast_segments_offset = mod_path.segments().len();
                        }
                        let segments = collector.stacks.path_segments.len() - s0;
                        // The trait's segments are the stack's `num_segments` from `first` on,
                        // its last segment first.
                        let first = g0 + segments - num_segments;
                        match collector.store.path_at(path).generic_args() {
                            Some(trait_args) => {
                                collector.stacks.path_generic_args.resize(first, 0);
                                collector
                                    .stacks.path_generic_args
                                    .extend(trait_args.slot_words().iter().rev().copied());
                            }
                            None => collector.stacks.path_generic_args.resize(g0 + segments, 0),
                        }
                        collector.alloc_path(path, ptr);

                        // Insert the type reference (T in the above example) as Self parameter for the trait
                        let last_segment = *collector.stacks.path_generic_args.get(first)?;
                        let with_self =
                            collector.store.paths_mut().with_self_type(unslot(last_segment), self_type);
                        collector.stacks.path_generic_args[first] = slot(Some(with_self));
                    }
                }
            }
            ast::PathSegmentKind::CrateKw => {
                if path.qualifier().is_some() {
                    // FIXME: Report an error.
                    return None;
                }
                kind = PathKind::Crate;
                break;
            }
            ast::PathSegmentKind::SelfKw => {
                if path.qualifier().is_some() {
                    // FIXME: Report an error.
                    return None;
                }
                // don't break out if `self` is the last segment of a path, this mean we got a
                // use tree like `foo::{self}` which we want to resolve as `foo`
                if collector.stacks.path_segments.len() > s0 {
                    kind = PathKind::SELF;
                    break;
                }
            }
            ast::PathSegmentKind::SuperKw => {
                let nested_super_count = if let PathKind::Super(n) = kind { n } else { 0 };
                kind = PathKind::Super(nested_super_count + 1);
            }
        }
        path = match qualifier(path) {
            Some(it) => it,
            None => break,
        };
    }
    collector.stacks.path_segments[s0..].reverse();
    let segment_count = collector.stacks.path_segments.len() - s0;
    if collector.stacks.path_generic_args.len() > g0 || type_anchor.is_some() {
        collector.stacks.path_generic_args.resize(g0 + segment_count, 0);
        collector.stacks.path_generic_args[g0..].reverse();
    }

    if segment_count == 0 && kind == PathKind::Plain && type_anchor.is_none() {
        // plain empty paths don't exist, this means we got a single `self` segment as our path
        kind = PathKind::SELF;
    }

    // handle local_inner_macros :
    // Basically, even in rustc it is quite hacky:
    // https://github.com/rust-lang/rust/blob/614f273e9388ddd7804d5cbc80b8865068a3744e/src/librustc_resolve/macros.rs#L456
    // We follow what it did anyway :)
    if segment_count == 1
        && kind == PathKind::Plain
        && let Some(_macro_call) = path.syntax().parent().and_then(ast::MacroCall::cast)
    {
        let syn_ctxt = collector.expander.ctx_for_range(path.segment()?.syntax().text_range());
        if let Some(macro_call_id) = syn_ctxt.outer_expn(collector.db)
            && hir_expand::MacroCallId::from(macro_call_id).loc(collector.db).def.local_inner
        {
            kind = match resolve_crate_root(collector.db, syn_ctxt) {
                Some(crate_root) => PathKind::DollarCrate(crate_root),
                None => PathKind::Crate,
            }
        }
    }

    #[cfg(test)]
    {
        ast_segments.reverse();
        SEGMENT_LOWERING_MAP
            .with_borrow_mut(|map| map.extend(ast_segments.into_iter().zip(ast_segments_offset..)));
    }

    if let Some(last_segment_args) = collector.stacks.path_generic_args[g0..].last_mut()
        && unslot(*last_segment_args)
            .is_some_and(|args| collector.store.paths().generic_args_at(args).has_self_type)
    {
        // Well-formed code cannot have `<T as Trait>` without an associated item after,
        // and this causes panics in hir-ty lowering.
        *last_segment_args = 0;
    }

    let mod_path = ModPath::from_segments(kind, collector.stacks.path_segments.drain(s0..));

    if let Some(old_lifetimes_constrained_by_input) = old_lifetimes_constrained_by_input {
        // The lifetime arguments of the last segment, copied out of the columns so the
        // resolution below can borrow the collector.
        let last_args = collector.stacks.path_generic_args[g0..].last().and_then(|&word| unslot(word));
        let lifetime_args: Option<SmallVec<[LifetimeRefId; 4]>> = last_args.map(|args| {
            let args = collector.store.paths().generic_args_at(args).args;
            args.iter()
                .filter_map(|arg| match *arg {
                    GenericArg::Lifetime(lt_ref) => Some(lt_ref),
                    GenericArg::Type(_) | GenericArg::Const(_) => None,
                })
                .collect() // alloc: inline up to 4 lifetimes, and only in an argument-bound scope
        });
        let type_alias_constrained_lifetimes = collector
            .get_constrained_lifetimes_if_type_alias(&mod_path, lifetime_args.as_deref());
        if let Some(lifetimes) = type_alias_constrained_lifetimes {
            collector.named_lifetime_store.lifetimes_constrained_by_input =
                old_lifetimes_constrained_by_input;
            collector.named_lifetime_store.lifetimes_constrained_by_input.extend(
                lifetimes.filter_map(|lt_ref| match &collector.store.lifetimes[lt_ref] {
                    // clone: the set owns interned names
                    LifetimeRef::Named(name) => Some(name.clone()),
                    _ => None,
                }),
            );
        } else {
            collector
                .named_lifetime_store
                .lifetimes_constrained_by_input
                .extend(old_lifetimes_constrained_by_input);
        }
    }

    let paths = collector.store.paths_mut();
    if type_anchor.is_none() && collector.stacks.path_generic_args.len() == g0 {
        Some(paths.push_bare(mod_path))
    } else {
        Some(paths.push_normal(mod_path, type_anchor, collector.stacks.path_generic_args.drain(g0..)))
    }
}

impl ExprCollector<'_> {
    /// The empty arguments of a return type notation, `method(..)`.
    fn push_return_type_notation(&mut self) -> GenericArgsIdx {
        self.store.paths_mut().push_generic_args(
            [],
            false,
            [],
            GenericArgsParentheses::ReturnTypeNotation,
        )
    }

    /// Collect `GenericArgs` from the parts of a fn-like path, i.e. `Fn(X, Y)
    /// -> Z` (which desugars to `Fn<(X, Y), Output=Z>`).
    pub(in crate::expr_store) fn lower_generic_args_from_fn_path(
        &mut self,
        args: Option<ast::ParenthesizedArgList<'_>>,
        ret_type: Option<ast::RetType<'_>>,
        impl_trait_lower_fn: &mut ImplTraitLowering<'_>,
    ) -> Option<GenericArgsIdx> {
        let params = args?;
        // The lifetimes left out in `Fn(..)` sugar are its own binder's.
        let (tuple, output) = self.without_impl_header_lifetimes(|this| {
            let mark = this.store.list_mark();
            for param in params.type_args() {
                let type_ref = this.lower_type_ref_opt(param.ty(), impl_trait_lower_fn);
                this.store.push_list_type(type_ref);
            }
            let tuple = this.with_lists(mark, |this, lists| {
                this.store.alloc_type_ref(TypeRef::Tuple(lists.types_since(mark)))
            });
            let output = match ret_type {
                Some(ret_type) => this.lower_type_ref_opt(ret_type.ty(), impl_trait_lower_fn),
                // -> ()
                None => this.alloc_type_ref_desugared(TypeRef::UNIT),
            };
            (tuple, output)
        });
        let paths = self.store.paths_mut();
        let binding = paths.binding(Name::new_symbol_root(sym::Output), None, Some(output), []);
        Some(paths.push_generic_args(
            [GenericArg::Type(tuple)],
            false,
            [binding],
            GenericArgsParentheses::ParenSugar,
        ))
    }

    /// The generic arguments of a segment, as a row of the store's columns. Its arguments and
    /// binding rows are collected on the collector's stacks (nested arguments push and pop above
    /// them), so a binding's own arguments are always an earlier row.
    pub(super) fn lower_generic_args(
        &mut self,
        node: ast::GenericArgList<'_>,
        impl_trait_lower_fn: &mut ImplTraitLowering<'_>,
    ) -> Option<GenericArgsIdx> {
        // This needs to be kept in sync with `hir_generic_arg_to_ast_view()`.
        let args_base = self.stacks.generic_args.len();
        let bindings_base = self.stacks.assoc_bindings.len();
        for generic_arg in node.generic_args() {
            match generic_arg {
                ast::GenericArg::TypeArg(type_arg) => {
                    let type_ref = self.lower_type_ref_opt(type_arg.ty(), impl_trait_lower_fn);
                    self.stacks.generic_args.push(GenericArg::Type(type_ref));
                }
                ast::GenericArg::AssocTypeArg(assoc_type_arg) => {
                    // This needs to be kept in sync with `hir_assoc_type_binding_to_ast_view()`.
                    if assoc_type_arg.param_list().is_some() {
                        // We currently ignore associated return type bounds.
                        continue;
                    }
                    if let Some(name_ref) = assoc_type_arg.name_ref() {
                        // Nested impl traits like `impl Foo<Assoc = impl Bar>` are allowed
                        self.with_outer_impl_trait_scope(false, |this| {
                            let name = name_ref.as_name();
                            let args = assoc_type_arg
                                .generic_arg_list()
                                .and_then(|args| this.lower_generic_args(args, impl_trait_lower_fn))
                                .or_else(|| {
                                    assoc_type_arg
                                        .return_type_syntax()
                                        .map(|_| this.push_return_type_notation())
                                });
                            let type_ref = assoc_type_arg
                                .ty()
                                .map(|it| this.lower_type_ref(it, impl_trait_lower_fn));
                            let bounds = this.type_bounds_from_ast(
                                assoc_type_arg.type_bound_list(),
                                impl_trait_lower_fn,
                            );
                            let binding = this.store.binding_with_bounds(name, args, type_ref, bounds);
                            this.stacks.assoc_bindings.push(binding);
                        });
                    }
                }
                ast::GenericArg::LifetimeArg(lifetime_arg) => {
                    if let Some(lifetime) = lifetime_arg.lifetime() {
                        let lifetime_ref = self.lower_lifetime_ref(lifetime);
                        self.stacks.generic_args.push(GenericArg::Lifetime(lifetime_ref))
                    }
                }
                ast::GenericArg::ConstArg(arg) => {
                    let arg = self.lower_const_arg(arg);
                    self.stacks.generic_args.push(GenericArg::Const(arg))
                }
            }
        }

        if self.stacks.generic_args.len() == args_base && self.stacks.assoc_bindings.len() == bindings_base {
            return None;
        }
        Some(self.store.paths_mut().push_generic_args(
            self.stacks.generic_args.drain(args_base..),
            false,
            self.stacks.assoc_bindings.drain(bindings_base..),
            GenericArgsParentheses::No,
        ))
    }
}

fn qualifier(path: ast::Path<'_>) -> Option<ast::Path<'_>> {
    if let Some(q) = path.qualifier() {
        return Some(q);
    }
    // FIXME: this bottom up traversal is not too precise.
    // Should we handle do a top-down analysis, recording results?
    let use_tree_list =
        path.syntax().ancestor_where(ast::UseTreeList::can_cast).and_then(ast::UseTreeList::cast)?;
    let use_tree = use_tree_list.parent_use_tree();
    use_tree.path()
}

/// This function finds the AST segment that corresponds to the HIR segment
/// with index `segment_idx` on the path that is lowered from `path`.
pub fn hir_segment_to_ast_segment_view(
    path: ast::Path<'_>,
    segment_idx: u32,
) -> Option<ast::PathSegment<'_>> {
    // Too tightly coupled to `lower_path()`, but unfortunately we cannot decouple them,
    // as keeping source maps for all paths segments will have a severe impact on memory usage.

    let mut segments = path.segments();
    let first = segments.next();
    if let Some(ast::PathSegmentKind::Type { trait_ref: Some(trait_ref), .. }) =
        first.and_then(|it| it.kind())
    {
        return find_segment(trait_ref.path()?.segments().chain(segments), segment_idx);
    }
    return find_segment(first.into_iter().chain(segments), segment_idx);

    fn find_segment<'t>(
        segments: impl Iterator<Item = ast::PathSegment<'t>>,
        segment_idx: u32,
    ) -> Option<ast::PathSegment<'t>> {
        segments
            .filter(|segment| match segment.kind() {
                Some(
                    ast::PathSegmentKind::CrateKw
                    | ast::PathSegmentKind::SelfKw
                    | ast::PathSegmentKind::SuperKw
                    | ast::PathSegmentKind::Type { .. },
                )
                | None => false,
                Some(ast::PathSegmentKind::Name(name)) => name.text() != "$crate",
                Some(ast::PathSegmentKind::SelfTypeKw) => true,
            })
            .nth(segment_idx as usize)
    }
}