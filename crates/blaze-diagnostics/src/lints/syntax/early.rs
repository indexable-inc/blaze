//! The rest of rustc's early lint pass and of the lints the lexer, the parser, expansion and
//! resolution buffer: those no other part of [`crate::lints::syntax`] gives.
//!
//! - `builtin`: `while_true`, `unsafe_code`, `special_module_name`, `anonymous_parameters`,
//!   `ellipsis_inclusive_range_patterns` (and its 2021 error, E0783), `double_negations`,
//!   `ambiguous_negative_literals`, `redundant_semicolons`.
//! - `labels`: `unused_labels`, and resolution's warning of a label that shadows another.
//! - `lexer`: `text_direction_codepoint_in_comment`, `text_direction_codepoint_in_literal`,
//!   the reserved-prefix and guarded-string migration lints, `keyword_idents`, and the
//!   identifier lints (`non_ascii_idents`, `confusable_idents`, `mixed_script_confusables`,
//!   `uncommon_codepoints`).
//! - `macros`: `semicolon_in_expressions_from_macros`, `incomplete_include`,
//!   `meta_variable_misuse`, `edition_2024_expr_fragment_specifier`,
//!   `rust_2021_incompatible_or_patterns`, `duplicate_macro_attributes`, `bad_asm_style`.
//! - `validation`: what `rustc_ast_passes` and the parser buffer
//!   (`deprecated_where_clause_location`, `missing_abi`, `missing_unsafe_on_extern`, ..), and
//!   `non_camel_case_types`.
//!
//! Follows rustc's `compiler/rustc_lint/src/lib.rs` (`BuiltinCombinedEarlyLintPass`,
//! `BuiltinCombinedPreExpansionLintPass`) and `compiler/rustc_lint/src/early.rs` (MIT or
//! Apache-2.0).

mod builtin;
mod labels;
mod lexer;
mod macros;
pub(crate) mod validation;

use blaze_hash::HashMap;
use hir::{HirFileId, InFile};
use span::SyntaxContext;
use syntax::{SyntaxKind, SyntaxNodePtr, TextRange, view as ast};

use crate::{
    cx::Cx,
    lints::syntax::{Emitted, Tree, Walk},
};

/// rustc's combined early lint pass (`BuiltinCombinedEarlyLintPass`, one AST walk with every
/// pass's methods inlined), and better: one walk over each tree's pre-order node column for
/// every part here, AST validation and the parser's lints included (rustc walks the AST for
/// those apart). Each node is read once; each part dispatches on its kind with a `match`.
/// What is not of the nodes goes apart: the lexer over each file's text, expansions once per
/// tree, `unused_labels` over each body's expression column.
pub(crate) fn lints<'db>(cx: &Cx<'_, 'db>, walk: &Walk<'db>) -> Vec<Emitted> {
    let mut out = lexer::lints(cx, walk);
    let expansions = Expansions::of(cx, walk);
    let mut builtin = builtin::Builtin::new(cx, &expansions);
    let mut validation = validation::Validation::new(cx, walk);
    let krate = macros::crate_at(cx, walk);
    builtin.krate(cx, walk, &mut out);
    for (index, tree) in walk.trees.iter().enumerate() {
        let index = index as u32;
        builtin.tree(cx, tree, &mut out);
        for (&node, &kind) in tree.nodes.iter().zip(&tree.kinds) {
            builtin.node(cx, walk, tree, node, kind, &mut out);
            validation.node(cx, tree, index, node, &mut out);
            if matches!(kind, SyntaxKind::MACRO_RULES | SyntaxKind::MACRO_DEF) {
                macros::definition(cx, krate, tree, node, &mut out);
            }
        }
        validation.tree(cx, tree, index);
    }
    out.extend(validation.finish());
    out.extend(macros::lints(cx, walk));
    out.extend(labels::unused(cx));
    out
}

/// The syntax context rustc's span of `range` in `file` has: that of its first token
/// (`Span::to` keeps the context of where a span starts).
pub(crate) fn ctxt(cx: &Cx<'_, '_>, file: HirFileId, range: TextRange) -> SyntaxContext {
    file.span_map(cx.db).span_for_range(range).ctx
}

/// rustc's `span.from_expansion()` for a range of a tree: it is not written, whole, in a file
/// or in what a macro call was handed.
pub(crate) fn range_from_expansion(cx: &Cx<'_, '_>, range: InFile<TextRange>) -> bool {
    match range.file_id.macro_file() {
        None => false,
        Some(_) => !range.original_node_file_range_opt(cx.db).is_some_and(|(_, ctxt)| ctxt.is_root()),
    }
}

/// rustc's `span.allows_unsafe()`: the span is of the expansion of a macro defined with
/// `#[allow_internal_unsafe]`. A file's own text is of no expansion: no span map is read.
pub(crate) fn allows_unsafe(cx: &Cx<'_, '_>, file: HirFileId, range: TextRange) -> bool {
    file.macro_file().is_some()
        && ctxt(cx, file, range)
        .outer_expn(cx.db)
        .is_some_and(|call| hir_expand::MacroCallId::from(call).loc(cx.db).def.allow_internal_unsafe)
}

/// The trees of the walk by the macro call (or the item or attribute of an attribute macro or
/// derive) they are the expansion of: one hash probe per lookup, built once per crate with
/// its size known.
pub(crate) struct Expansions<'w, 'db> {
    by_call: HashMap<InFile<SyntaxNodePtr>, &'w Tree<'db>>,
}

impl<'w, 'db> Expansions<'w, 'db> {
    pub(crate) fn of(cx: &Cx<'_, '_>, walk: &'w Walk<'db>) -> Expansions<'w, 'db> {
        let mut by_call = HashMap::with_capacity_and_hasher(walk.trees.len(), Default::default());
        for tree in &walk.trees {
            let Some(call) = tree.call else { continue };
            let node = call.loc(cx.db).to_node_view(cx.db);
            by_call.insert(node.map(ast::ptr), tree);
        }
        Expansions { by_call }
    }

    /// The tree `call` of `file` expands to, if the walk has it.
    pub(crate) fn of_call(&self, file: HirFileId, call: ast::Node<'_>) -> Option<&'w Tree<'db>> {
        self.by_call.get(&InFile::new(file, ast::ptr(call))).copied()
    }
}

#[cfg(test)]
mod tests {
    /// On a crate with nothing to lint, the walk over every node of every tree allocates
    /// nothing per node: what the pass allocates, less what `unused_labels` does (the crate's
    /// list of bodies, from hir-def's `every_body`), is the same whatever the crate's size.
    #[test]
    fn the_walk_allocates_nothing_per_node() {
        let used = |fns: usize| {
            let mut used = None;
            crate::lints::with_cx(&crate::lints::quiet_crate(fns), &[], |cx| {
                let walk = cx.walk();
                assert!(super::lints(cx, walk).is_empty());
                let (found, all) = blaze_alloc_count::measure(|| super::lints(cx, walk));
                assert!(found.is_empty());
                let (_, labels) = blaze_alloc_count::measure(|| super::labels::unused(cx));
                used = Some(all.allocations - labels.allocations);
            });
            used.unwrap()
        };
        let (small, large) = (used(4), used(400));
        assert_eq!(small, large, "the early pass's allocations do not grow with the crate");
    }
}
