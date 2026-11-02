//! rustc's `PostExpansionVisitor`: the legacy gates of the crate once it is expanded and `cfg`
//! removed what it removes, in the order rustc's walk comes to them: `#[doc(..)]`'s unstable
//! flavours, negative and `default` impls, auto traits, trait aliases, `macro`, `impl Trait` in
//! type aliases and associated types, `type const`, LLVM intrinsics, extern types, pattern
//! types (`!` is stable since 1.100.0), binders with more than lifetimes, `try` blocks, `f16`
//! and `f128` literals, `X..` in slice patterns, associated type defaults and `default`
//! associated items.
//!
//! Follows rustc's `compiler/rustc_ast_passes/src/feature_gate.rs` (`PostExpansionVisitor`,
//! `check_impl_trait`, `check_late_bound_lifetime_defs`), `compiler/rustc_ast_passes/src/
//! diagnostics.rs` (`ForbiddenBound`, `ForbiddenConstParam`), `compiler/rustc_ast/src/visit.rs`
//! (the order of the walk: a node's gate before what is in it, its attributes first) and
//! `compiler/rustc_ast/src/attr/mod.rs` (`first_attr_value_str_by_name`) (MIT or Apache-2.0).

use std::ops::ControlFlow;

use blaze_hash::HashMap;
use hir::{HirFileId, InFile};
use syntax::{
    NodeOrToken, SyntaxKind, SyntaxNodePtr, T, TextRange, WalkEvent,
    view::{self as ast, AstNode as _, AstToken as _},
};

use super::pre::{child, token};
use crate::{
    checks::features::{Features, allows_unstable, feature_err},
    cx::{Cx, item_range_view as item_range},
    lints::syntax::{Walk, source_fact},
    model::{Diag, MultiSpan, Span},
};

/// An error of the visitor, and whether rustc stops there (`emit_fatal`).
pub(super) struct Posted {
    pub(super) diag: Diag,
    pub(super) fatal: bool,
}

/// The visitor's errors, in the order rustc's walk of the expanded crate gives them.
pub(super) fn visit(cx: &Cx<'_, '_>, walk: &Walk<'_>, features: &Features) -> Vec<Posted> {
    // alloc: empty, allocates only when a gate is hit
    let mut visitor = Visitor { cx, walk, features, found: Vec::new() };
    for tree in &walk.trees {
        for &node in &tree.nodes {
            visitor.node(tree.file, node);
        }
    }
    if visitor.found.is_empty() {
        // alloc: empty, allocates nothing
        return Vec::new();
    }
    let places = Places::new(cx);
    let mut keyed: Vec<((Vec<(u32, u32)>, usize), Posted)> = visitor
        .found
        .into_iter()
        .enumerate()
        .map(|(seq, (anchor, posted))| ((places.of(cx, anchor.file_id, anchor.value.text_range()), seq), posted))
        // alloc: on the error path (the gates hit, in rustc's order)
        .collect();
    keyed.sort_by(|a, b| a.0.cmp(&b.0));
    // alloc: as above
    keyed.into_iter().map(|(_, posted)| posted).collect()
}

struct Visitor<'a, 'b, 'db, 'w> {
    cx: &'a Cx<'b, 'db>,
    walk: &'a Walk<'w>,
    features: &'a Features,
    /// Each error with the node rustc's walk is at when it gives it.
    found: Vec<(InFile<SyntaxNodePtr>, Posted)>,
}

impl Visitor<'_, '_, '_, '_> {
    /// The gates rustc's visitor checks as it comes to `node`, before what is in it.
    fn node(&mut self, file: HirFileId, node: ast::Node<'_>) {
        match node.kind() {
            SyntaxKind::ATTR => self.attr(file, node),
            SyntaxKind::IMPL => self.impl_(file, node),
            SyntaxKind::TRAIT => {
                if token(node, T![=]).is_some() {
                    self.gate(file, node, "trait_alias", item_range(node), "trait aliases are experimental", None);
                } else if token(node, T![auto]).is_some() {
                    let explain = "auto traits are experimental and possibly buggy";
                    self.gate(file, node, "auto_traits", item_range(node), explain, None);
                }
            }
            SyntaxKind::MACRO_DEF => {
                self.gate(file, node, "decl_macro", item_range(node), "`macro` is experimental", None);
            }
            SyntaxKind::TYPE_ALIAS | SyntaxKind::CONST | SyntaxKind::FN | SyntaxKind::STATIC => {
                match node.parent().map(|it| it.kind()) {
                    Some(SyntaxKind::ASSOC_ITEM_LIST) => self.assoc_item(file, node),
                    Some(SyntaxKind::EXTERN_ITEM_LIST) => self.foreign_item(file, node),
                    _ => self.item(file, node),
                }
            }
            SyntaxKind::FOR_BINDER => self.binder(file, node),
            SyntaxKind::PATTERN_TYPE => {
                self.gate(file, node, "pattern_types", node.text_range(), "pattern types are unstable", None);
            }
            SyntaxKind::BLOCK_EXPR => {
                if let Some(modifier) = child(node, SyntaxKind::TRY_BLOCK_MODIFIER)
                    && !modifier.children_with_tokens().filter_map(NodeOrToken::into_token).any(|it| {
                        it.kind() == T![bikeshed] || (it.kind() == SyntaxKind::IDENT && it.text() == "bikeshed")
                    })
                {
                    let at = TextRange::new(modifier.text_range().start(), node.text_range().end());
                    self.gate(file, node, "try_blocks", at, "`try` expression is experimental", None);
                }
            }
            SyntaxKind::LITERAL => self.literal(file, node),
            SyntaxKind::SLICE_PAT => {
                for pat in node.children().filter(|it| ast::Pat::can_cast(it.kind())) {
                    // `IdentPat::pat`: the pattern after its `@`.
                    let sub = match ast::IdentPat::can_cast(pat.kind()) {
                        true => pat.children().find(|it| ast::Pat::can_cast(it.kind())),
                        false => None,
                    };
                    if half_open(sub.unwrap_or(pat)) {
                        let explain = "`X..` patterns in slices are experimental";
                        self.gate(file, node, "half_open_range_patterns_in_slices", pat.text_range(), explain, None);
                    }
                }
            }
            _ => {}
        }
    }

    /// rustc's `gate!`: the E0658 error of `feature` at `at`, unless it is enabled or the
    /// macro `at` comes from allows it, given as the walk is at `anchor`.
    fn gate(
        &mut self,
        file: HirFileId,
        anchor: ast::Node<'_>,
        feature: &str,
        at: TextRange,
        explain: &str,
        help: Option<&str>,
    ) {
        if self.features.enabled(feature) || allows_unstable(self.cx, file, at, feature) {
            return;
        }
        let mut diag = feature_err(self.features, feature, self.cx.span(InFile::new(file, at)), explain);
        if let Some(help) = help {
            diag.help(help);
        }
        self.push(file, anchor, diag, false);
    }

    fn push(&mut self, file: HirFileId, anchor: ast::Node<'_>, mut diag: Diag, fatal: bool) {
        let at = InFile::new(file, ast::ptr(anchor));
        source_fact(self.cx, &mut diag, at);
        self.found.push((at, Posted { diag, fatal }));
    }

    /// `visit_attribute`: the unstable flavours of `#[doc(..)]`, at the attribute (at what
    /// `cfg_attr` gives, for an attribute it gives).
    fn attr(&mut self, file: HirFileId, node: ast::Node<'_>) {
        let Some(attr) = ast::Attr::cast(node) else { return };
        let top = attr.meta();
        let cfg = self.walk.cfg;
        hir_expand::attrs::expand_cfg_attr_view::<()>(std::iter::once(attr), || cfg, |meta, _| {
            let ast::Meta::TokenTreeMeta(list) = meta else { return ControlFlow::Continue(()) };
            let is_doc = list.path().and_then(|it| it.as_single_name_ref()).is_some_and(|it| it.text() == "doc");
            let Some(tt) = list.token_tree().filter(|_| is_doc) else { return ControlFlow::Continue(()) };
            let at = match top {
                Some(top) if top.syntax() == meta.syntax() => node.text_range(),
                _ => meta.syntax().text_range(),
            };
            nested_names(tt, |name| {
                if let Some((feature, explain)) = doc_gate(name) {
                    self.gate(file, node, feature, at, explain, None);
                }
            });
            ControlFlow::Continue(())
        });
    }

    /// `visit_item` of an impl: a negative one (`!` to the trait's path), a `default` one.
    fn impl_(&mut self, file: HirFileId, node: ast::Node<'_>) {
        if let Some(bang) = token(node, T![!])
            && let Some(trait_) = node
                .children()
                .find(|it| ast::Type::can_cast(it.kind()) && it.text_range().start() >= bang.text_range().end())
        {
            let at = TextRange::new(bang.text_range().start(), trait_.text_range().end());
            let explain = "negative impls are experimental";
            self.gate(file, node, "negative_impls", at, explain, Some("use marker types for now"));
        }
        if token(node, T![default]).is_some() && token(node, T![for]).is_some() {
            self.gate(file, node, "specialization", item_range(node), "specialization is experimental", None);
        }
    }

    /// `visit_item` of a type alias (`impl Trait` in it) and of a `type const`.
    fn item(&mut self, file: HirFileId, node: ast::Node<'_>) {
        match node.kind() {
            SyntaxKind::TYPE_ALIAS => {
                if let Some(ty) = ast::TypeAlias::cast(node).and_then(|it| it.ty()) {
                    self.impl_trait(file, node, ty.syntax(), false);
                }
            }
            SyntaxKind::CONST if token(node, T![type]).is_some() => {
                let explain = "top-level `type const` are unstable";
                self.gate(file, node, "min_generic_const_args", item_range(node), explain, None);
            }
            _ => {}
        }
    }

    /// `visit_assoc_item`: associated type defaults, `impl Trait` in an associated type, an
    /// associated `type const`, and `default` (a function's needs only `min_specialization`).
    fn assoc_item(&mut self, file: HirFileId, node: ast::Node<'_>) {
        let in_trait = node.parent().and_then(|it| it.parent()).is_some_and(|it| it.kind() == SyntaxKind::TRAIT);
        let at = item_range(node);
        let defaults = "associated type defaults are unstable";
        match node.kind() {
            SyntaxKind::TYPE_ALIAS => {
                let ty = ast::TypeAlias::cast(node).and_then(|it| it.ty());
                if in_trait && ty.is_some() {
                    self.gate(file, node, "associated_type_defaults", at, defaults, None);
                }
                if let Some(ty) = ty {
                    self.impl_trait(file, node, ty.syntax(), true);
                }
            }
            SyntaxKind::CONST if token(node, T![type]).is_some() => {
                let explain = "associated `type const` are unstable";
                self.gate(file, node, "min_generic_const_args", at, explain, None);
                if in_trait && token(node, T![=]).is_some() {
                    self.gate(file, node, "associated_type_defaults", at, defaults, None);
                }
            }
            _ => {}
        }
        if token(node, T![default]).is_some() {
            let is_fn = node.kind() == SyntaxKind::FN;
            let enabled = self.features.enabled("specialization")
                || (is_fn && self.features.enabled("min_specialization"));
            if !enabled && !allows_unstable(self.cx, file, at, "specialization") {
                let span = self.cx.span(InFile::new(file, at));
                let diag = feature_err(self.features, "specialization", span, "specialization is experimental");
                self.push(file, node, diag, false);
            }
        }
    }

    /// `visit_foreign_item`: a function or static linked to an LLVM intrinsic, an extern type.
    fn foreign_item(&mut self, file: HirFileId, node: ast::Node<'_>) {
        match node.kind() {
            SyntaxKind::FN | SyntaxKind::STATIC if self.links_to_llvm(node) => {
                let explain = "linking to LLVM intrinsics is experimental";
                self.gate(file, node, "link_llvm_intrinsics", item_range(node), explain, None);
            }
            SyntaxKind::TYPE_ALIAS => {
                self.gate(file, node, "extern_types", item_range(node), "extern types are experimental", None);
            }
            _ => {}
        }
    }

    /// Whether the first `#[link_name]` of `node` is a string that starts with `llvm.`.
    fn links_to_llvm(&self, node: ast::Node<'_>) -> bool {
        let Some(owner) = ast::AnyHasAttrs::cast(node) else { return false };
        for (meta, _) in self.walk.attrs(owner) {
            let named = meta.path().and_then(|it| it.as_single_name_ref()).is_some_and(|it| it.text() == "link_name");
            if !named {
                continue;
            }
            let ast::Meta::KeyValueMeta(kv) = meta else { return false };
            let Some(ast::Expr::Literal(lit)) = kv.expr() else { return false };
            let ast::LiteralKind::String(text) = lit.kind() else { return false };
            return text.value().is_ok_and(|it| it.starts_with("llvm."));
        }
        false
    }

    /// `check_impl_trait`: each `impl Trait` in the type `ty` of `item`, not in what is an
    /// anonymous constant (an array's length, a const argument).
    fn impl_trait(&mut self, file: HirFileId, item: ast::Node<'_>, ty: ast::Node<'_>, in_associated_ty: bool) {
        let (feature, explain) = match in_associated_ty {
            true => ("impl_trait_in_assoc_type", "`impl Trait` in associated types is unstable"),
            false => ("type_alias_impl_trait", "`impl Trait` in type aliases is unstable"),
        };
        let mut events = ty.preorder();
        while let Some(event) = events.next() {
            let WalkEvent::Enter(node) = event else { continue };
            match node.kind() {
                SyntaxKind::CONST_ARG => events.skip_subtree(),
                SyntaxKind::IMPL_TRAIT_TYPE => self.gate(file, item, feature, node.text_range(), explain, None),
                _ => {}
            }
        }
    }

    /// `check_late_bound_lifetime_defs` of a `for<..>` binder: parameters that are not
    /// lifetimes, const parameters even with `non_lifetime_binders`, bounds (a trait bound
    /// stops rustc).
    fn binder(&mut self, file: HirFileId, node: ast::Node<'_>) {
        let Some(params) = child(node, SyntaxKind::GENERIC_PARAM_LIST) else { return };
        // The name of each parameter that is not a lifetime, and whether it is a const's.
        let non_lifetimes = || {
            params.children().filter_map(|param| {
                let is_const = match param.kind() {
                    SyntaxKind::TYPE_PARAM => false,
                    SyntaxKind::CONST_PARAM => true,
                    _ => return None,
                };
                Some((child(param, SyntaxKind::NAME)?.text_range(), is_const))
            })
        };
        let feature = "non_lifetime_binders";
        let cx = self.cx;
        let span_of = |range: TextRange| cx.span(InFile::new(file, range));
        if !self.features.enabled(feature) {
            let spans: Vec<Span> = non_lifetimes()
                .filter(|(it, _)| !allows_unstable(cx, file, *it, feature))
                .map(|(it, _)| span_of(it))
                // alloc: empty unless a parameter is gated
                .collect();
            if !spans.is_empty() {
                // alloc: empty, allocates nothing
                let spans = MultiSpan { primary: spans, labels: Vec::new() };
                let diag = feature_err(self.features, feature, spans, "only lifetime parameters can be used in this context");
                self.push(file, node, diag, false);
            }
        } else if let Some(diag) = multi_error(
            "late-bound const parameters cannot be used currently",
            non_lifetimes().filter(|(_, is_const)| *is_const).map(|(it, _)| span_of(it)),
        ) {
            self.push(file, node, diag, false);
        }
        for param in params.children() {
            let Some(list) = child(param, SyntaxKind::TYPE_BOUND_LIST) else { continue };
            let bounds = || list.children().filter(|it| it.kind() == SyntaxKind::TYPE_BOUND);
            let fatal = bounds().any(|it| {
                child(it, SyntaxKind::LIFETIME).is_none() && child(it, SyntaxKind::USE_BOUND_GENERIC_ARGS).is_none()
            });
            let spans = bounds().map(|it| span_of(it.text_range()));
            if let Some(diag) = multi_error("bounds cannot be used in this context", spans) {
                self.push(file, node, diag, fatal);
            }
        }
    }

    /// `visit_expr` of a literal with the suffix `f16` or `f128` (in an expression or a
    /// pattern; the visitor does not walk into attributes).
    fn literal(&mut self, file: HirFileId, node: ast::Node<'_>) {
        let gate_of = |suffix: &str| match suffix {
            "f16" => Some(("f16", "the type `f16` is unstable")),
            "f128" => Some(("f128", "the type `f128` is unstable")),
            _ => None,
        };
        // The gate of the first numeric token with a suffix.
        let gate = node.children_with_tokens().filter_map(NodeOrToken::into_token).find_map(|token| {
            match token.kind() {
                SyntaxKind::INT_NUMBER => ast::IntNumber::cast(token)?.suffix().map(gate_of),
                SyntaxKind::FLOAT_NUMBER => ast::FloatNumber::cast(token)?.suffix().map(gate_of),
                _ => None,
            }
        });
        let Some(Some((feature, explain))) = gate else { return };
        if node.ancestors().any(|it| it.kind() == SyntaxKind::ATTR) {
            return;
        }
        self.gate(file, node, feature, item_range(node), explain, None);
    }
}

/// An error with no code at each of `spans`, if there is one.
fn multi_error(message: &str, spans: impl Iterator<Item = Span>) -> Option<Diag> {
    // alloc: empty unless there is an error
    let primary: Vec<Span> = spans.collect();
    // clone: the error's span
    let first = primary.first()?.clone();
    let mut diag = Diag::error(None, message, first);
    diag.span.primary = primary;
    Some(diag)
}

/// Whether `pat` is a range pattern with a start and no end (`X..`).
fn half_open(pat: ast::Node<'_>) -> bool {
    if pat.kind() != SyntaxKind::RANGE_PAT {
        return false;
    }
    let Some(op) = pat
        .children_with_tokens()
        .filter_map(NodeOrToken::into_token)
        .find(|it| matches!(it.kind(), T![..] | T![..=] | T![...]))
    else {
        return false;
    };
    let op_start = op.text_range().start();
    let (mut before, mut after) = (false, false);
    for it in pat.children().filter(|it| ast::Pat::can_cast(it.kind())) {
        before |= it.text_range().end() <= op_start;
        after |= it.text_range().start() >= op.text_range().end();
    }
    before && !after
}

/// The names of the items of the list `tt` of `#[doc(..)]`: rustc's `meta_item_list`, each
/// item's name when its path is one word.
fn nested_names(tt: ast::TokenTree<'_>, mut f: impl FnMut(&str)) {
    let mut elements = tt.syntax().children_with_tokens().filter(|it| !it.kind().is_trivia()).peekable();
    // The opening delimiter.
    elements.next();
    let mut at_start = true;
    while let Some(element) = elements.next() {
        match element {
            NodeOrToken::Token(token) if token.kind() == T![,] => {
                at_start = true;
                continue;
            }
            NodeOrToken::Token(token) if at_start && token.kind() == SyntaxKind::IDENT => {
                let path_goes_on = elements.peek().is_some_and(|it| matches!(it.kind(), T![::] | T![:]));
                if !path_goes_on {
                    f(token.text().strip_prefix("r#").unwrap_or(token.text()));
                }
            }
            _ => {}
        }
        at_start = false;
    }
}

/// The gate of `#[doc(name)]`: its feature and what rustc says of it.
fn doc_gate(name: &str) -> Option<(&'static str, &'static str)> {
    Some(match name {
        "cfg" => ("doc_cfg", "`#[doc(cfg)]` is experimental"),
        "auto_cfg" => ("doc_cfg", "`#[doc(auto_cfg)]` is experimental"),
        "masked" => ("doc_masked", "`#[doc(masked)]` is experimental"),
        "notable_trait" => ("doc_notable_trait", "`#[doc(notable_trait)]` is experimental"),
        "attribute" => ("rustdoc_internals", "`#[doc(attribute)]` is meant for internal use only"),
        "keyword" => ("rustdoc_internals", "`#[doc(keyword)]` is meant for internal use only"),
        "fake_variadic" => ("rustdoc_internals", "`#[doc(fake_variadic)]` is meant for internal use only"),
        "search_unbox" => ("rustdoc_internals", "`#[doc(search_unbox)]` is meant for internal use only"),
        _ => return None,
    })
}

/// Where rustc's walk of the expanded crate comes to a node: a module's file where its `mod`
/// item is, an expansion where its macro is called (what a derive makes, after the item it
/// is on), then where in its own tree the node is, outer nodes before those in them.
struct Places {
    files: HashMap<HirFileId, Vec<(u32, u32)>>,
}

impl Places {
    fn new(cx: &Cx<'_, '_>) -> Places {
        let mut files: HashMap<HirFileId, Vec<(u32, u32)>> = HashMap::default();
        for module in crate::checks::modules(cx) {
            let Some(file) = module.as_source_file_id(cx.db) else { continue };
            // alloc: on the error path (`Places` is made only when a gate is hit)
            let mut place = Vec::new();
            if let Some(declared) = module.declaration_source_range(cx.db) {
                let declared = declared.original_node_file_range_rooted(cx.db);
                place = files.get(&HirFileId::from(declared.file_id)).cloned().unwrap_or_default();
                place.push(entered(declared.range));
            }
            files.entry(HirFileId::from(file)).or_insert(place);
        }
        Places { files }
    }

    fn of(&self, cx: &Cx<'_, '_>, file: HirFileId, range: TextRange) -> Vec<(u32, u32)> {
        let mut file = file;
        // alloc: on the error path, as above
        let mut chain = vec![entered(range)];
        while let Some(call) = file.macro_file() {
            let loc = call.loc(cx.db);
            let node = loc.to_node_view(cx.db);
            let adt = match loc.kind {
                hir_expand::MacroCallKind::Derive { .. } => {
                    node.value.ancestors().find(|it| ast::Adt::can_cast(it.kind()))
                }
                _ => None,
            };
            chain.push(match adt {
                Some(adt) => (u32::from(adt.text_range().end()), 1),
                None => entered(node.value.text_range()),
            });
            file = node.file_id;
        }
        let mut place = self.files.get(&file).cloned().unwrap_or_default();
        place.extend(chain.into_iter().rev());
        place
    }
}

/// The place of entering a node of `range`: outer nodes before the nodes in them.
fn entered(range: TextRange) -> (u32, u32) {
    (u32::from(range.start()), u32::MAX - u32::from(range.end()))
}

#[cfg(test)]
mod tests {
    use syntax::{Edition, Parse, SourceFile, view::AstNode};

    use super::*;

    fn parse(text: &str) -> Parse<SourceFile> {
        SourceFile::parse(text, Edition::Edition2024)
    }

    fn first<'t, N: AstNode<'t>>(parse: &'t Parse<SourceFile>) -> N {
        parse.root().descendants().find_map(N::cast).expect("the node")
    }

    #[test]
    fn the_items_of_a_doc_list_are_named_by_their_one_word_paths() {
        let parse = parse("#[doc(cfg(unix), masked, a::b, alias = \"x\", r#keyword)] fn f() {}");
        let attr: ast::Attr<'_> = first(&parse);
        let Some(ast::Meta::TokenTreeMeta(list)) = attr.meta() else { panic!("a list") };
        let mut names = Vec::new();
        nested_names(list.token_tree().unwrap(), |name| names.push(name.to_owned()));
        assert_eq!(names, ["cfg", "masked", "alias", "keyword"]);
    }

    #[test]
    fn only_a_range_with_a_start_and_no_end_is_half_open() {
        let parse = parse("fn f() { let [1.., ..2, 3..4, x @ 5..] = a; }");
        let slice: ast::SlicePat<'_> = first(&parse);
        let open: Vec<bool> = slice
            .pats()
            .map(|pat| {
                let inner = ast::IdentPat::cast(pat.syntax()).and_then(|it| it.pat()).unwrap_or(pat);
                half_open(inner.syntax())
            })
            .collect();
        assert_eq!(open, [true, false, false, true]);
    }

    #[test]
    fn rustcs_doc_flavours_are_gated_as_it_says() {
        assert_eq!(doc_gate("cfg"), Some(("doc_cfg", "`#[doc(cfg)]` is experimental")));
        assert_eq!(doc_gate("keyword").map(|it| it.0), Some("rustdoc_internals"));
        assert_eq!(doc_gate("alias"), None);
    }
}
