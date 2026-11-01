//! What rustc's early lint pass, its expansion and its attribute checks lint on the source as
//! written: the crate walked as rustc's early lint pass walks its expanded AST ([`Walk`]),
//! each lint emitted in the order rustc emits it ([`Pass`]).
//!
//! - `check_cfg`: `--check-cfg` ([`CheckCfg`]), `unexpected_cfgs`,
//!   `explicit_builtin_cfgs_in_flags`.
//! - `delims`: `unused_parens`, `unused_braces`.
//! - `attrs`: `unused_attributes`, `ill_formed_attribute_input`, `conflicting_repr_hints`,
//!   `unused_doc_comments`, the feature lints (`incomplete_features`, `internal_features`,
//!   `unstable_features`).
//! - `early`: the rest of the early lint pass (`while_true`, `non_camel_case_types`,
//!   `keyword_idents`, `redundant_semicolons`, ..) and of the lints the lexer, the parser,
//!   expansion and resolution buffer (`text_direction_codepoint_in_comment`,
//!   `semicolon_in_expressions_from_macros`, `unused_labels`, ..).
//!
//! Follows rustc's `compiler/rustc_interface/src/passes.rs` (`pre_expansion_lint` before
//! expansion, `early_lint_checks` after it), `compiler/rustc_lint/src/early.rs`
//! (`EarlyContextAndPass`: `with_lint_attrs` and `check_id`, which emits the lints buffered
//! for a node before the pass's own there; the pre-order walk of `rustc_ast::visit`) and
//! `compiler/rustc_expand/src/config.rs` (`StripUnconfigured`: what `cfg` removes before any
//! lint sees it) (MIT or Apache-2.0).

/// `pub(crate)` for its table of rustc's language features, which `crate::checks` reads too.
pub(crate) mod attrs;
mod check_cfg;
mod delims;
mod early;

pub(crate) use self::early::validation::pretty;
pub use check_cfg::CheckCfg;

use std::collections::hash_map::Entry;

use blaze_hash::HashMap;
use blaze_ids::radix;
use blaze_items::{Fact, ItemId};
use cfg::{CfgExpr, CfgOptions};
use hir::{HirFileId, InFile, MacroCallId, ModuleSourceView};
use hir_def::AdtId;
use syntax::{
    FlatTree, SyntaxKind, SyntaxNodePtr, TextRange, WalkEvent,
    view::{self as ast, AstNode as _, HasAttrs as _, HasName as _},
};

use crate::{
    cx::Cx,
    lints::CommandLine,
    lower::{Found, Phase},
    model::Diag,
};

/// When rustc emits a lint of this module, relative to its walk of the crate. Lints of one
/// [`Phase`] are emitted by pass: the pre-expansion ones first, then the rest by where the
/// walk comes to their node, a node's buffered lints before the pass's own there and those of
/// leaving it after everything in it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Pass {
    /// rustc's pre-expansion lint pass (`pre_expansion_lint`: `keyword_idents`), run on the
    /// crate as parsed, before anything is expanded or resolved.
    PreExpansion,
    /// A lint the lexer, the parser, expansion or resolution buffered for a node
    /// (`buffer_lint`), emitted as the early lint pass comes to the node (`check_id`).
    Buffered,
    /// The early lint pass's own as it comes to a node (`check_item`, `check_expr`, ..), and
    /// `check_attr`'s.
    Early,
    /// The early lint pass's own as it leaves a node (`check_item_post`, `check_crate_post`,
    /// ..).
    Post,
    /// What attribute parsing buffers as the AST is lowered, emitted after every other
    /// diagnostic of its phase, by the item it is of (`emit_delayed_lints`, after
    /// `check_mod_attrs`).
    Delayed,
}

/// When a [`Pass::Buffered`] lint was buffered: those of one node come in the order rustc
/// buffered them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Stage {
    /// As the session is configured from the command line (`build_configuration`), before
    /// anything is parsed.
    Configured,
    /// By the lexer or the parser, before expansion.
    Parsed,
    /// As expansion collects a node: stripping its `cfg`s, and for a macro call checking its
    /// attributes (`check_attributes` of `collect_bang`), before the call is expanded.
    Collected,
    /// As a macro call is expanded or what it expands to is parsed, or after: resolution and
    /// AST validation.
    #[default]
    Expanded,
}

/// A diagnostic found by one of this module's parts, and when rustc emits it.
#[derive(Clone, Debug)]
pub(crate) struct Emitted {
    pub(crate) found: Found,
    pub(crate) pass: Pass,
    pub(crate) stage: Stage,
    /// The node rustc's walk emits it at; with none, it comes before those of its pass that
    /// have one (what resolution says as it runs, before the early lint pass).
    pub(crate) place: Option<InFile<SyntaxNodePtr>>,
}

impl Emitted {
    /// The lint `diag` of `phase`, its level decided at `at`, emitted in `pass` as rustc's walk
    /// comes to `at`.
    pub(crate) fn new(diag: Diag, phase: Phase, pass: Pass, at: InFile<SyntaxNodePtr>) -> Emitted {
        let found = Found { pre_expansion: pass == Pass::PreExpansion, ..Found::lint(diag, phase, at) };
        Emitted { found, pass, stage: Stage::default(), place: Some(at) }
    }

    /// `diag`, an error or warning that is no lint's, emitted in `pass` as rustc's walk comes
    /// to `at` (E0783 of the early lint pass).
    pub(crate) fn placed(diag: Diag, phase: Phase, pass: Pass, at: InFile<SyntaxNodePtr>) -> Emitted {
        Emitted { found: Found::new(diag, phase), pass, stage: Stage::default(), place: Some(at) }
    }

    /// `diag`, an error or warning that is no lint's, emitted in `pass` before the walk (the
    /// label-shadowing warning of late resolution).
    pub(crate) fn unplaced(diag: Diag, phase: Phase, pass: Pass) -> Emitted {
        Emitted { found: Found::new(diag, phase), pass, stage: Stage::default(), place: None }
    }

    /// This, buffered at `stage`.
    pub(crate) fn buffered(self, stage: Stage) -> Emitted {
        Emitted { stage, ..self }
    }
}

/// A syntax tree rustc's early lint pass walks: a file of the crate's modules, or what a macro
/// call, an attribute macro or a derive in one expands to. Its nodes are views of the tree the
/// database keeps, borrowed for `'db`, of a tree `cx.vsema` handed out.
pub(crate) struct Tree<'db> {
    pub(crate) file: HirFileId,
    pub(crate) root: ast::Node<'db>,
    /// The macro call it is the expansion of, when it is one.
    pub(crate) call: Option<MacroCallId>,
    /// Its nodes rustc's walk comes to, in pre-order: not those `cfg` removes, nor an item an
    /// attribute macro replaces (what that expands to is a tree of its own).
    pub(crate) nodes: Vec<ast::Node<'db>>,
    /// The kind of each of `nodes`, by index: a pass that looks at a few kinds scans these 2
    /// bytes a node, not the nodes ([`Tree::of_kind`]).
    pub(crate) kinds: Vec<SyntaxKind>,
    /// Of each of `nodes`, by index, whether it may have an attribute, its own or inner: false
    /// is certain. An attribute is a child of its owner or of a list at most two nodes below it
    /// (`fn f() { #![..] }`), so this is true of the three nodes above each `ATTR`.
    pub(crate) attrs: Vec<bool>,
    /// The outermost nodes `cfg` removes: their `cfg`s are evaluated, nothing in them is.
    pub(crate) off: Vec<ast::Node<'db>>,
    /// The items an attribute macro replaces with its expansion.
    pub(crate) replaced: Vec<ast::Node<'db>>,
    /// The ranges of `off` and `replaced`, disjoint, by where they start.
    skips: Vec<TextRange>,
    /// Where the walk comes to the tree: the places of the `mod` items and macro calls it is
    /// reached through, outermost first.
    place: Vec<(u32, u32)>,
}

impl<'db> Tree<'db> {
    /// Whether the tree is a file's own, not a macro's expansion.
    pub(crate) fn in_file(&self) -> bool {
        self.call.is_none()
    }

    /// Whether `range` is in what rustc's walk never sees: a node `cfg` removes or an item an
    /// attribute macro replaces.
    pub(crate) fn skipped(&self, range: TextRange) -> bool {
        let after = self.skips.partition_point(|it| it.start() <= range.start());
        after > 0 && self.skips[after - 1].contains_range(range)
    }

    /// The nodes whose kind `wanted` holds, in pre-order: a scan of the kind column, each node
    /// read only when it is wanted.
    pub(crate) fn of_kind(&self, wanted: impl Fn(SyntaxKind) -> bool) -> impl Iterator<Item = ast::Node<'db>> {
        self.kinds.iter().zip(&self.nodes).filter(move |(kind, _)| wanted(**kind)).map(|(_, &node)| node)
    }

    /// The nodes whose kind `wanted` holds and which may have an attribute ([`Tree::attrs`]),
    /// in pre-order: a scan of two columns, each node read only when it is wanted.
    pub(crate) fn with_attrs(&self, wanted: impl Fn(SyntaxKind) -> bool) -> impl Iterator<Item = ast::Node<'db>> {
        (self.kinds.iter().zip(&self.attrs).zip(&self.nodes))
            .filter(move |((kind, attrs), _)| **attrs && wanted(**kind))
            .map(|(_, &node)| node)
    }

    /// `node` of the tree, as the [`InFile`] the rest of the crate takes.
    pub(crate) fn ptr(&self, node: ast::Node<'_>) -> InFile<SyntaxNodePtr> {
        InFile::new(self.file, ast::ptr(node))
    }
}

/// The crate as rustc's early lint pass walks it: every file of its modules and every
/// expansion in them, in the order the walk comes to them, each with what `cfg` keeps of it.
pub(crate) struct Walk<'db> {
    pub(crate) trees: Vec<Tree<'db>>,
    /// The crate's cfg: `--cfg` and the target's.
    pub(crate) cfg: &'db CfgOptions,
    index: HashMap<HirFileId, usize>,
    /// Each module of the crate by the order rustc checks it in (`checks::modules`).
    modules: HashMap<hir::Module, usize>,
}

impl<'db> Walk<'db> {
    pub(crate) fn of(cx: &Cx<'_, 'db>) -> Walk<'db> {
        let modules = crate::checks::modules(cx);
        let mut walk = Walk {
            // alloc: setup, the walk is made once per check (`Cx::walk`)
            trees: Vec::new(),
            cfg: cx.krate.cfg(cx.db),
            index: HashMap::default(),
            // alloc: setup, as above
            modules: modules.iter().enumerate().map(|(rank, it)| (*it, rank)).collect(),
        };
        let mut places: HashMap<HirFileId, Vec<(u32, u32)>> = HashMap::default();
        for module in modules {
            let Some(file) = module.as_source_file_id(cx.db) else { continue };
            let file_id = HirFileId::from(file);
            if walk.index.contains_key(&file_id) {
                continue;
            }
            // alloc: setup, once per file of the crate
            let mut place = Vec::new();
            if let Some(declared) = module.declaration_source_range(cx.db) {
                let declared = declared.original_node_file_range_rooted(cx.db);
                place = places.get(&HirFileId::from(declared.file_id)).cloned().unwrap_or_default();
                place.push(entered(declared.range));
            }
            // clone: setup, as above
            places.insert(file_id, place.clone());
            let root = cx.vsema.parse(file).syntax();
            walk.add(cx, file_id, root, place);
        }
        walk
    }

    /// Adds the tree `root` of `file`, and the trees of what the macros in it expand to.
    fn add(&mut self, cx: &Cx<'_, 'db>, file: HirFileId, root: ast::Node<'db>, place: Vec<(u32, u32)>) {
        // The tree's index is taken now: what it expands to is added after it is pushed.
        match self.index.entry(file) {
            Entry::Occupied(_) => return,
            Entry::Vacant(it) => it.insert(self.trees.len()),
        };
        let capacity = node_count(root);
        // alloc: setup, once per tree of the walk, the tree's columns, sized once
        let mut nodes = Vec::with_capacity(capacity);
        // alloc: as above
        let mut kinds = Vec::with_capacity(capacity);
        // alloc: as above
        let mut attrs = Vec::with_capacity(capacity);
        // The indices in `nodes` of the nodes the walk is in, innermost last: deeper than 64 is
        // rare in written Rust.
        // alloc: setup, once per tree of the walk
        let mut open: Vec<u32> = Vec::with_capacity(64);
        // alloc: setup, once per tree of the walk, the tree's columns
        let mut off = Vec::new();
        // alloc: as above
        let mut replaced = Vec::new();
        // alloc: as above
        let mut expansions: Vec<((u32, u32), InFile<ast::Node<'db>>)> = Vec::new();
        let mut events = root.preorder();
        while let Some(event) = events.next() {
            let node = match event {
                WalkEvent::Enter(node) => node,
                WalkEvent::Leave(node) => {
                    if open.last().is_some_and(|&at| nodes[at as usize] == node) {
                        open.pop();
                    }
                    continue;
                }
            };
            let kind = node.kind();
            if let Some(owner) = ast::AnyHasAttrs::cast(node)
                && !self.on(owner)
            {
                off.push(node);
                events.skip_subtree();
                continue;
            }
            if let Some(item) = ast::Item::cast(node)
                && item.attrs().next().is_some()
                && let Some(expanded) = cx.vsema.expand_attr_macro(item)
            {
                expansions.push((entered(node.text_range()), expanded.value));
                replaced.push(node);
                events.skip_subtree();
                continue;
            }
            if kind == SyntaxKind::MACRO_CALL
                && let Some(call) = ast::MacroCall::cast(node)
                && let Some(expanded) = cx.vsema.expand_macro_call(call)
            {
                expansions.push((entered(node.text_range()), expanded));
            }
            if kind == SyntaxKind::MODULE
                && let Some(module) = self.block_module(cx, file, node)
            {
                expansions.push((entered(node.text_range()), module));
            }
            if ast::Adt::can_cast(kind)
                && let Some(owner) = ast::AnyHasAttrs::cast(node)
            {
                // What a derive makes comes after the item it is on.
                let after = (u32::from(node.text_range().end()), 1);
                for (meta, _) in self.attrs(owner) {
                    let is_derive = matches!(meta, ast::Meta::TokenTreeMeta(it)
                        if it.path().and_then(|it| it.as_single_name_ref()).is_some_and(|it| it.text() == "derive"));
                    if !is_derive {
                        continue;
                    }
                    let Some(derived) = cx.vsema.expand_derive_macro(meta) else { continue };
                    for root in derived.into_iter().flatten() {
                        let file = cx.vsema.hir_file_for(root.value);
                        if file.is_macro() {
                            expansions.push((after, InFile::new(file, root.value)));
                        }
                    }
                }
            }
            // What holds an attribute is in `nodes`: the walk skips no node above one it enters.
            if kind == SyntaxKind::ATTR {
                for &at in open.iter().rev().take(3) {
                    attrs[at as usize] = true;
                }
            }
            open.push(u32::try_from(nodes.len()).expect("a tree has fewer than 2^32 nodes"));
            nodes.push(node);
            kinds.push(kind);
            attrs.push(false);
        }
        // alloc: setup, once per tree, as above
        let mut skips: Vec<TextRange> = off.iter().chain(&replaced).map(|it| it.text_range()).collect();
        radix::sort_copy_by_key(&mut skips, |it| radix::pack_32(it.start().into(), it.end().into()));
        self.trees.push(Tree {
            file,
            root,
            call: file.macro_file(),
            nodes,
            kinds,
            attrs,
            off,
            replaced,
            skips,
            // clone: setup, once per tree, its place
            place: place.clone(),
        });
        for (at, expanded) in expansions {
            // clone: setup, once per tree, as above
            let mut place = place.clone();
            place.push(at);
            self.add(cx, expanded.file_id, expanded.value, place);
        }
    }

    /// The file of the out-of-line module `node` declares in a block (`fn f() { mod m; }`): it
    /// is in a block's def map, not the crate's, so only the walk coming to `node` finds it.
    fn block_module(&self, cx: &Cx<'_, 'db>, file: HirFileId, node: ast::Node<'db>) -> Option<InFile<ast::Node<'db>>> {
        let module = ast::Module::cast(node)?;
        if module.item_list().is_some() {
            return None;
        }
        let def = cx.vsema.to_def2(InFile::new(file, module))?;
        if self.modules.contains_key(&def) {
            return None;
        }
        let source = def.definition_source_view(cx.db);
        match source.value {
            // The root `cx.vsema` hands out, so what is found under it maps back to its file.
            ModuleSourceView::SourceFile(_) => Some(InFile::new(source.file_id, cx.root(source.file_id))),
            ModuleSourceView::Module(_) | ModuleSourceView::BlockExpr(_) => None,
        }
    }

    /// Whether `cfg` keeps `owner`: each of its `#[cfg]`s, those `cfg_attr` gives included,
    /// holds, as rustc evaluates them in order and stops at the first that does not.
    pub(crate) fn on(&self, owner: ast::AnyHasAttrs<'_>) -> bool {
        self.attrs(owner).all(|(meta, _)| match meta {
            ast::Meta::CfgMeta(meta) => !meta
                .cfg_predicate()
                .is_some_and(|it| self.cfg.check(&CfgExpr::parse_from_view(it)) == Some(false)),
            _ => true,
        })
    }

    /// The attributes of `owner` as rustc sees them once `cfg_attr` is expanded, outer then
    /// inner, in order: each with the attribute it is written in. Read off the tree as the
    /// caller iterates; nothing is gathered.
    pub(crate) fn attrs<'t>(
        &self,
        owner: ast::AnyHasAttrs<'t>,
    ) -> impl Iterator<Item = (ast::Meta<'t>, ast::Attr<'t>)> + use<'t, 'db> {
        hir_expand::attrs::cfg_attrs_view(ast::attrs_including_inner(&owner), self.cfg)
    }

    /// The tree of `file`, if the walk has it.
    pub(crate) fn tree(&self, file: HirFileId) -> Option<&Tree<'db>> {
        self.index.get(&file).map(|&it| &self.trees[it])
    }

    /// The trees of the crate's files, not of expansions.
    pub(crate) fn files(&self) -> impl Iterator<Item = &Tree<'db>> {
        self.trees.iter().filter(|it| it.in_file())
    }

    /// Where rustc's walk comes to `at` in `pass`: as it enters it, or for [`Pass::Post`] as it
    /// leaves it. A node of an expansion the walk does not have is placed at the call it came
    /// from.
    pub(crate) fn place(&self, cx: &Cx<'_, '_>, at: InFile<SyntaxNodePtr>, pass: Pass) -> Vec<(u32, u32)> {
        let (mut file, mut range) = (at.file_id, at.value.text_range());
        loop {
            if let Some(tree) = self.tree(file) {
                // clone: once per diagnostic placed (`lints`'s sort key)
                let mut place = tree.place.clone();
                place.push(match pass {
                    Pass::Post => (u32::from(range.end()), 0),
                    _ => entered(range),
                });
                return place;
            }
            if file.macro_file().is_none() {
                // alloc: empty, allocates nothing
                return Vec::new();
            }
            let node = cx.vsema.find_parent_file(file).expect("an expansion has its call");
            (file, range) = (node.file_id, node.value.text_range());
        }
    }
}

/// The place of entering a node of `range`: outer nodes before the nodes in them.
fn entered(range: TextRange) -> (u32, u32) {
    (u32::from(range.start()), u32::MAX - u32::from(range.end()))
}

/// How many nodes the walk of `root`'s tree may come to, the bound its columns are sized to:
/// a scan of the token flags of the file's tree and of each part already parsed. A part not
/// parsed yet is not parsed for this (what `cfg` removes stays unparsed); it is counted as one
/// node per three bytes of its text, about what written Rust has.
fn node_count(root: ast::Node<'_>) -> usize {
    fn nodes(tree: &FlatTree) -> usize {
        (0..tree.len()).filter(|&at| !tree.is_token(at)).count()
    }
    let file = root.file();
    let parts: usize = (file.lazy_nodes().iter())
        .map(|&at| match file.is_parsed(at) {
            // A part's root stands for its lazy node, counted in the file's tree.
            true => nodes(file.part(at)) - 1,
            false => file.text_len(at) as usize / 3,
        })
        .sum();
    nodes(file) + parts
}

/// Whether rustc's span of `node` is of a macro's making (`Span::from_expansion`): it is in
/// an expansion and not written, whole, in what the call was handed or in an attribute
/// macro's item.
pub(crate) fn from_expansion(cx: &Cx<'_, '_>, node: InFile<ast::Node<'_>>) -> bool {
    match node.file_id.macro_file() {
        None => false,
        Some(_) => !node
            .map(|it| it.text_range())
            .original_node_file_range_opt(cx.db)
            .is_some_and(|(_, ctxt)| ctxt.is_root()),
    }
}

/// Records on `diag` what a lint of the source at `at` read: the code of the body it is in,
/// else the signature of the item it is of.
pub(crate) fn source_fact(cx: &Cx<'_, '_>, diag: &mut Diag, at: InFile<SyntaxNodePtr>) {
    if crate::lints::body_item(cx, at).is_some() {
        crate::lints::code_fact(cx, diag, at);
        return;
    }
    if let Some((item, name)) = signature_item(cx, at) {
        // alloc: the diagnostic's fact
        diag.fact(Fact::Signature(cx.key(item)), format!("the signature of `{name}`"));
    }
}

/// The innermost item `at` is in or is, and its name, through a macro's expansion to where it
/// was called.
fn signature_item(cx: &Cx<'_, '_>, at: InFile<SyntaxNodePtr>) -> Option<(ItemId, String)> {
    // Every caller is building a diagnostic's fact (`source_fact`).
    fn name(it: Option<ast::Name<'_>>) -> Option<String> {
        // alloc: the item's name, for a diagnostic's fact
        it.map(|it| it.text().to_owned())
    }
    let mut node = cx.syntax_view(at);
    loop {
        for ancestor in node.value.ancestors() {
            if let Some(it) = ast::Fn::cast(ancestor) {
                let def = cx.vsema.to_def2(node.with_value(it))?;
                let id = hir_def::FunctionId::try_from(def).ok()?;
                return Some((ItemId::Function(id), name(it.name())?));
            }
            if let Some(it) = ast::Adt::cast(ancestor) {
                let id = match cx.vsema.to_def2(node.with_value(it))? {
                    hir::Adt::Struct(it) => AdtId::StructId(it.into()),
                    hir::Adt::Enum(it) => AdtId::EnumId(it.into()),
                    hir::Adt::Union(it) => AdtId::UnionId(it.into()),
                };
                return Some((ItemId::Adt(id), name(it.name())?));
            }
            if let Some(it) = ast::Trait::cast(ancestor) {
                let def = cx.vsema.to_def2(node.with_value(it))?;
                return Some((ItemId::Trait(def.into()), name(it.name())?));
            }
            if let Some(it) = ast::TypeAlias::cast(ancestor) {
                let def = cx.vsema.to_def2(node.with_value(it))?;
                return Some((ItemId::Alias(def.into()), name(it.name())?));
            }
            if let Some(it) = ast::Const::cast(ancestor) {
                let def = cx.vsema.to_def2(node.with_value(it))?;
                // alloc: the name of an unnamed const, as above
                return Some((ItemId::Const(def.into()), name(it.name()).unwrap_or_else(|| "_".to_owned())));
            }
            if let Some(it) = ast::Static::cast(ancestor) {
                let def = cx.vsema.to_def2(node.with_value(it))?;
                return Some((ItemId::Static(def.into()), name(it.name())?));
            }
            if ast::Impl::can_cast(ancestor.kind()) || ast::Module::can_cast(ancestor.kind()) {
                return None;
            }
        }
        node.file_id.macro_file()?;
        node = cx.vsema.find_parent_file(node.file_id)?;
    }
}

/// What rustc's early lint pass, its expansion and its attribute checks lint on the source as
/// written, in the order it emits them.
pub(crate) fn lints(cx: &Cx<'_, '_>, cmd: &CommandLine) -> Vec<Found> {
    let walk = cx.walk();
    let mut emitted = check_cfg::lints(cx, cmd, walk);
    emitted.extend(early::lints(cx, walk));
    emitted.extend(delims::lints(cx, walk));
    emitted.extend(attrs::lints(cx, walk));
    // Per emitted diagnostic, not per node: each key is made once, and what it costs scales with
    // the output.
    let mut keyed: Vec<_> = emitted
        .into_iter()
        .map(|Emitted { found, pass, stage, place }| {
            let group = match pass {
                Pass::PreExpansion => 0u8,
                Pass::Delayed => 2,
                _ => 1,
            };
            let module = match (found.phase, place) {
                (Phase::Items, Some(at)) => checked_in(cx, &walk.modules, at),
                _ => (0, false),
            };
            let place = place.map(|at| walk.place(cx, at, pass)).unwrap_or_default();
            let rank = found.diag.lint_name().map_or(EARLY_PASSES.len(), pass_rank);
            ((found.phase, group, module, place, pass, stage, rank), found)
        })
        // alloc: per diagnostic found, nothing when there is none
        .collect();
    keyed.sort_by(|a, b| a.0.cmp(&b.0));
    // alloc: as above
    keyed.into_iter().map(|(_, found)| found).collect()
}

/// Where rustc's `check_mod_attrs` comes to `at`: it checks each module in turn (`modules`,
/// rustc's `par_hir_for_each_module`), in it every item whose module it is, then for the
/// crate root the crate's own attributes. A module file's inner attributes are of its `mod`
/// item, checked in the module that has it.
fn checked_in(cx: &Cx<'_, '_>, modules: &HashMap<hir::Module, usize>, at: InFile<SyntaxNodePtr>) -> (usize, bool) {
    let node = cx.syntax_view(at);
    let rank = |module: hir::Module| modules.get(&module).copied().unwrap_or(usize::MAX);
    if let Some(file) = ast::SourceFile::cast(node.value) {
        let Some(module) = cx.vsema.to_def2(node.with_value(file)) else { return (usize::MAX, false) };
        return match module.parent(cx.db) {
            None => (rank(module), true),
            Some(parent) => (rank(parent), false),
        };
    }
    for ancestor in cx.vsema.ancestors_with_macros_file(node).skip(1) {
        if let Some(module) = ast::Module::cast(ancestor.value)
            && module.item_list().is_some()
        {
            return (cx.vsema.to_def2(ancestor.with_value(module)).map_or(usize::MAX, rank), false);
        }
        if let Some(file) = ast::SourceFile::cast(ancestor.value) {
            return (cx.vsema.to_def2(ancestor.with_value(file)).map_or(usize::MAX, rank), false);
        }
    }
    (usize::MAX, false)
}

/// The lints of the passes of rustc's `BuiltinCombinedEarlyLintPass`, in the order it runs
/// them on a node (`compiler/rustc_lint/src/lib.rs`).
const EARLY_PASSES: &[&[&str]] = &[
    &["unused_parens"],
    &["unused_braces"],
    &["unused_import_braces"],
    &["unsafe_code"],
    &["special_module_name"],
    &["anonymous_parameters"],
    &["ellipsis_inclusive_range_patterns"],
    &["non_camel_case_types"],
    &["while_true"],
    &["non_ascii_idents", "uncommon_codepoints", "confusable_idents", "mixed_script_confusables"],
    &["incomplete_features", "internal_features"],
    &["redundant_semicolons"],
    &["unused_doc_comments"],
    &["edition_2024_expr_fragment_specifier"],
    &["ambiguous_negative_literals"],
    &["double_negations"],
];

/// Where `lint`'s pass is among those rustc runs on a node: lints of one node and pass come in
/// that order; what no early pass gives comes after, as it was found.
fn pass_rank(lint: &str) -> usize {
    EARLY_PASSES.iter().position(|it| it.contains(&lint)).unwrap_or(EARLY_PASSES.len())
}

#[cfg(test)]
mod tests {
    use syntax::{
        TextSize,
        view::{AstNode as _, HasName as _},
    };

    use super::*;

    /// `check` given the walk of the first workspace crate of `fixture`.
    fn walked(fixture: &str, check: impl for<'db> FnOnce(&Cx<'_, 'db>, &Walk<'db>)) {
        let checked = crate::tests::checked(fixture);
        let db = &checked.db;
        hir::attach_db(db, || {
            let krate = hir::check::workspace_crates(db)[0];
            let frame = crate::tests::frame();
            let cx = Cx::new(db, krate, checked.files(), &frame);
            check(&cx, cx.walk());
        });
    }

    /// Each function the walk comes to, by name, with its tree and node.
    fn fns<'w, 'db>(walk: &'w Walk<'db>) -> Vec<(String, &'w Tree<'db>, ast::Node<'db>)> {
        walk.trees
            .iter()
            .flat_map(|tree| tree.nodes.iter().map(move |&node| (tree, node)))
            .filter_map(|(tree, node)| {
                let name = ast::Fn::cast(node)?.name()?.text().to_owned();
                Some((name, tree, node))
            })
            .collect()
    }

    fn place_of(cx: &Cx<'_, '_>, walk: &Walk<'_>, name: &str) -> Vec<(u32, u32)> {
        let fns = fns(walk);
        let (_, tree, node) = fns.iter().find(|(it, ..)| it == name).unwrap_or_else(|| panic!("no `{name}`"));
        walk.place(cx, tree.ptr(*node), Pass::Early)
    }

    #[test]
    fn what_cfg_removes_is_not_walked() {
        walked(
            r#"
//- /main.rs crate:main
#[cfg(FALSE)]
fn off() { let x = 1; }
fn on() {}
"#,
            |_, walk| {
                let names: Vec<String> = fns(walk).into_iter().map(|(it, ..)| it).collect();
                assert_eq!(names, ["on"]);
                let tree = walk.files().next().unwrap();
                assert_eq!(tree.off.len(), 1);
                assert!(tree.skipped(tree.off[0].text_range()));
            },
        );
    }

    #[test]
    fn a_module_file_is_walked_where_it_is_declared() {
        walked(
            r#"
//- /main.rs crate:main
fn before() {}
mod a;
fn after() {}
//- /a.rs
fn inside() {}
"#,
            |cx, walk| {
                assert_eq!(walk.files().count(), 2);
                let before = place_of(cx, walk, "before");
                let inside = place_of(cx, walk, "inside");
                let after = place_of(cx, walk, "after");
                assert!(before < inside && inside < after, "{before:?} {inside:?} {after:?}");
            },
        );
    }

    #[test]
    fn a_module_file_declared_in_a_block_is_walked_where_it_is_declared() {
        walked(
            r#"
//- /main.rs crate:main
fn before() {}
fn outer() { mod a; }
fn after() {}
//- /a.rs
fn inside() {}
"#,
            |cx, walk| {
                assert_eq!(walk.files().count(), 2);
                let before = place_of(cx, walk, "before");
                let inside = place_of(cx, walk, "inside");
                let after = place_of(cx, walk, "after");
                assert!(before < inside && inside < after, "{before:?} {inside:?} {after:?}");
            },
        );
    }

    /// The early lints and the stability check, which read only the walk's trees, both reach
    /// the file of a `#[path]` module declared in a block.
    #[test]
    fn the_early_lints_and_stability_reach_a_path_module_declared_in_a_block() {
        let checked = crate::tests::checked(
            r#"
//- /main.rs crate:main edition:2021
#[deprecated]
pub fn d() {}
fn main() {
    #[path = "m.rs"]
    mod m;
}
//- /m.rs
pub fn inner() {
    crate::d();
    while true {}
}
"#,
        );
        let mut messages: Vec<String> = checked
            .messages()
            .into_iter()
            .filter(|it| it.starts_with("while_true") || it.starts_with("deprecated") || it.starts_with("blaze can't"))
            .collect();
        messages.sort();
        assert_eq!(
            messages,
            ["deprecated: use of deprecated function `d`", "while_true: denote infinite loops with `loop { ... }`"]
        );
    }

    #[test]
    fn an_expansion_is_walked_where_it_is_called() {
        walked(
            r#"
//- /main.rs crate:main
macro_rules! m { ($e:expr) => { fn made() -> i32 { $e } }; }
fn before() {}
m!(1 + 2);
fn after() {}
"#,
            |cx, walk| {
                let fns = fns(walk);
                let (_, tree, made) = fns.iter().find(|(it, ..)| it == "made").unwrap();
                assert!(tree.call.is_some() && !tree.in_file());
                assert!(from_expansion(cx, InFile::new(tree.file, *made)));
                let sum = tree.nodes.iter().copied().find(|it| ast::BinExpr::can_cast(it.kind())).unwrap();
                assert!(!from_expansion(cx, InFile::new(tree.file, sum)));
                let (before, made, after) =
                    (place_of(cx, walk, "before"), place_of(cx, walk, "made"), place_of(cx, walk, "after"));
                assert!(before < made && made < after, "{before:?} {made:?} {after:?}");
            },
        );
    }

    #[test]
    fn a_node_is_entered_before_what_is_in_it_and_left_after() {
        let range = |lo: u32, hi: u32| TextRange::new(TextSize::from(lo), TextSize::from(hi));
        let (outer, inner, next) = (range(0, 10), range(2, 5), range(10, 12));
        assert!(entered(outer) < entered(inner));
        let left = (u32::from(outer.end()), 0);
        assert!(entered(inner) < left && left < entered(next));
    }

    /// Under `-A unused`, no lint of `delims` may fire: the pass reads no node and allocates
    /// nothing, at any size of crate.
    #[test]
    fn delims_allowed_everywhere_cost_nothing() {
        for fns in [4, 400] {
            crate::lints::with_cx(&crate::lints::quiet_crate(fns), &["-A", "unused"], |cx| {
                let walk = cx.walk();
                cx.lint_may_fire(crate::lints::registry::LintId::WARNINGS);
                let found = blaze_alloc_count::assert_at_most(0, "delims, all of its lints allowed", || {
                    delims::lints(cx, walk)
                });
                assert!(found.is_empty());
            });
        }
    }

    /// Each node's kind is in the kind column, in the order of the nodes.
    #[test]
    fn the_kind_column_is_the_nodes_kinds() {
        walked(&crate::lints::quiet_crate(3), |_, walk| {
            for tree in &walk.trees {
                assert_eq!(tree.kinds.len(), tree.nodes.len());
                assert!(tree.nodes.iter().zip(&tree.kinds).all(|(node, &kind)| node.kind() == kind));
                let fns: Vec<_> = tree.of_kind(|it| it == SyntaxKind::FN).collect();
                assert_eq!(fns.len(), 3);
            }
        });
    }

    /// Every node with an attribute, its own or inner, is marked in the attribute column, and a
    /// crate with none marks nothing.
    #[test]
    fn the_attrs_column_holds_every_owner() {
        const FIXTURE: &str = r#"
//- /lib.rs crate:main edition:2021
#![allow(unused)]
#[inline]
fn f(#[allow(unused)] x: u8) {
    #![allow(unused)]
    let y = { #![allow(unused)] 1 };
    match x {
        #![allow(unused)]
        #[allow(unused)]
        _ => {}
    }
}
mod m {
    #![allow(unused)]
}
struct S;
impl S {
    #![allow(unused)]
}
extern "C" {
    #![allow(unused)]
}
"#;
        walked(FIXTURE, |_, walk| {
            let mut owners = 0;
            for tree in &walk.trees {
                assert_eq!(tree.attrs.len(), tree.nodes.len());
                for (&node, &marked) in tree.nodes.iter().zip(&tree.attrs) {
                    let Some(owner) = ast::AnyHasAttrs::cast(node) else { continue };
                    if ast::attrs_including_inner(&owner).next().is_some() {
                        assert!(marked, "{node:?} has an attribute");
                        owners += 1;
                    }
                }
            }
            // `f`'s body is no owner of its own: its inner attribute is `f`'s, as rustc reads it.
            assert!(owners >= 9, "{owners}");
        });
        walked(&crate::lints::quiet_crate(3), |_, walk| {
            assert!(walk.trees.iter().all(|tree| tree.attrs.iter().all(|&it| !it)));
        });
    }

    /// The walk of a tree allocates its three columns and its stack, sized once from the flat
    /// tree, and nothing per node: the same count at 4 functions and at 400.
    #[test]
    fn walking_a_tree_allocates_only_its_columns() {
        for fns in [4, 400] {
            walked(&crate::lints::quiet_crate(fns), |cx, walk| {
                let tree = walk.files().next().unwrap();
                let mut again = Walk {
                    trees: Vec::with_capacity(1),
                    cfg: walk.cfg,
                    index: HashMap::with_capacity_and_hasher(1, Default::default()),
                    modules: HashMap::default(),
                };
                blaze_alloc_count::assert_at_most(4, "the walk of a tree", || {
                    again.add(cx, tree.file, tree.root, Vec::new());
                });
                assert_eq!(again.trees[0].nodes, tree.nodes);
                assert!(tree.nodes.len() > 10 * fns, "{}", tree.nodes.len());
            });
        }
    }

    /// The walk comes to every node of a file in pre-order but the subtrees `cfg` removes,
    /// `cfg_attr` included, and those are the ones rustc removes.
    #[test]
    fn walk_skips_what_cfg_removes() {
        const FIXTURE: &str = r#"
//- /main.rs crate:main
#![allow(unused)]
macro_rules! m { ($e:expr) => { $e } }
#[cfg(any())]
fn off() { let x = 1; }
#[cfg_attr(all(), cfg(any()))]
fn off_too() {}
#[cfg_attr(any(), cfg(any()))]
fn kept() {}
#[cfg_attr(all(), cfg_attr(all(), cfg(any())))]
mod nested_off {}
mod inner_off {
    #![cfg(any())]
    fn f() {}
}
struct S { #[cfg(any())] a: u8, b: u8 }
fn body() {
    #[cfg(any())]
    let y = 2;
    let z = match 1 { #[cfg(any())] 0 => 1, _ => 2 };
    m!(1 + 2);
}
"#;
        walked(FIXTURE, |_, walk| {
            let trees: Vec<_> = walk.files().collect();
            assert_eq!(trees.len(), 1, "the fixture is one file");
            let tree = trees[0];
            let mut nodes = Vec::new();
            let mut events = tree.root.preorder();
            while let Some(event) = events.next() {
                let WalkEvent::Enter(node) = event else { continue };
                if tree.off.contains(&node) || tree.replaced.contains(&node) {
                    events.skip_subtree();
                    continue;
                }
                nodes.push(node);
            }
            assert_eq!(tree.nodes, nodes);
            let text = tree.root.text();
            let off: Vec<_> = tree.off.iter().map(|it| &text[it.text_range()]).collect();
            assert_eq!(
                off,
                [
                    "#[cfg(any())]\nfn off() { let x = 1; }",
                    "#[cfg_attr(all(), cfg(any()))]\nfn off_too() {}",
                    "#[cfg_attr(all(), cfg_attr(all(), cfg(any())))]\nmod nested_off {}",
                    "mod inner_off {\n    #![cfg(any())]\n    fn f() {}\n}",
                    "#[cfg(any())] a: u8",
                    "#[cfg(any())]\n    let y = 2;",
                    "#[cfg(any())] 0 => 1,",
                ]
            );
            let kept = |it: &str| tree.nodes.iter().any(|node| &text[node.text_range()] == it);
            assert!(kept("#[cfg_attr(any(), cfg(any()))]\nfn kept() {}"));
            assert!(kept("b: u8"));
            assert!(kept("_ => 2"));
        });
    }

    #[test]
    fn lints_of_a_node_come_in_the_order_of_rustcs_passes() {
        assert!(pass_rank("unused_parens") < pass_rank("while_true"));
        assert!(pass_rank("non_camel_case_types") < pass_rank("while_true"));
        assert_eq!(pass_rank("unexpected_cfgs"), EARLY_PASSES.len());
    }
}
