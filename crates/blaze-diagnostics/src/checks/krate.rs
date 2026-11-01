//! The checks of the crate as a whole: a binary's entry point (rustc's `entry_fn`): no `main`
//! (E0601), two `#[rustc_main]` functions (E0137), a `main` in an `extern` block.
//!
//! Follows rustc's `compiler/rustc_passes/src/entry.rs` and its diagnostics (`NoMainErr`,
//! `ExternMain`, `MultipleRustcMain` in `compiler/rustc_passes/src/diagnostics.rs`),
//! `resolve_main` in `compiler/rustc_resolve/src/lib.rs`, `entry_point_type` in
//! `compiler/rustc_ast/src/entry.rs`, `def_span` in `compiler/rustc_middle/src/hir/map.rs`,
//! and where the crate's span ends: `new_parser_from_source_file` in
//! `compiler/rustc_parse/src/lib.rs`, `parse_mod` in `compiler/rustc_parse/src/parser/item.rs`
//! and the end-of-file token of `Parser::bump` in `compiler/rustc_parse/src/parser/mod.rs`
//! (MIT or Apache-2.0).

use std::ops::ControlFlow;

use base_db::{FileId, TargetKind};
use blaze_hash::{HashMap, HashSet};
use hir::{HirFileId, InFile, ModuleDef, Name};
use hir_def::{Lookup, ModuleDefId, expr_store::Body, item_scope::ImportOrGlob};
use syntax::{
    SyntaxKind, TextRange, TextSize,
    view::{self as ast, AstNode as _, HasName as _, NodeId},
};

use crate::{
    checks::{PASSES, has_crate_attr, modules},
    cx::{Cx, def_range_view as def_range, item_range_view as item_range},
    lower::Found,
    model::{Code, Diag, Level},
};

pub(crate) fn check(cx: &Cx<'_, '_>) -> Vec<Found> {
    // alloc: empty, allocates nothing, unless there is an error
    entry(cx).into_iter().map(|diag| Found::new(diag, PASSES)).collect()
}

/// rustc's `CrateType::Executable`: a binary, an example, a build script. A test or bench
/// target is built with the test harness, whose `main` is `#[rustc_main]`.
fn is_executable(kind: TargetKind) -> bool {
    matches!(kind, TargetKind::Bin | TargetKind::Example | TargetKind::BuildScript)
}

/// rustc's `entry_fn`: the errors of looking for the crate's `main`.
fn entry(cx: &Cx<'_, '_>) -> Vec<Diag> {
    if !is_executable(cx.krate.target_kind(cx.db)) || has_crate_attr(cx, "no_main") {
        // alloc: empty, allocates nothing
        return Vec::new();
    }
    // alloc: empty unless there is an error
    let mut diags = Vec::new();
    let items = free_items(cx);
    let mut rustc_main: Option<&FreeItem<'_>> = None;
    // alloc: empty unless a `main` is below the crate root
    let mut non_main_fns = Vec::new();
    for item in &items {
        if item.rustc_main {
            match rustc_main {
                None => rustc_main = Some(item),
                Some(first) => {
                    let span = cx.span(item.def_range());
                    let mut diag = Diag::error(
                        Some("E0137"),
                        "multiple functions with a `#[rustc_main]` attribute",
                        // clone: the error's span
                        span.clone(),
                    );
                    diag.span_label(cx.span(first.def_range()), "first `#[rustc_main]` function");
                    diag.span_label(span, "additional `#[rustc_main]` function");
                    diags.push(diag);
                }
            }
        } else if item.main && !item.at_root {
            non_main_fns.push(item);
        }
    }
    if rustc_main.is_some() {
        return diags;
    }
    let root = cx.krate.root_module(cx.db);
    let main = main_def(cx, root);
    if let Some(MainDef::Function(function)) = &main {
        let local = function.module(cx.db).krate(cx.db) == cx.krate;
        if local
            && function.extern_block(cx.db).is_some()
            && let Some(node) = cx.vsema.source(*function)
        {
            let span = cx.span(node.map(|it| def_range(it.syntax())));
            let mut diag = Diag::error(None, "the `main` function cannot be declared in an `extern` block", span);
            diag.fact(cx.resolves_fact(root, "main"), "what `main` is in the crate root");
            diags.push(diag);
        }
        return diags;
    }
    diags.push(no_main(cx, root, &non_main_fns, main));
    diags
}

/// What `crate::main` names in the value namespace (rustc's `MainDefinition`).
enum MainDef {
    Function(hir::Function),
    /// Something else, with rustc's span of its binding: the item, or the `use` that brings it.
    Other(InFile<TextRange>),
}

/// rustc's `resolve_main`: `main` in the crate root's value namespace, imports and globs
/// included.
fn main_def(cx: &Cx<'_, '_>, root: hir::Module) -> Option<MainDef> {
    let root_id = hir_def::ModuleId::from(root);
    let scope = root_id.def_map(cx.db).module(cx.db, root_id).scope(cx.db);
    let item = scope.get(&Name::new_root("main")).values?;
    if let ModuleDefId::FunctionId(_) = item.def
        && let ModuleDef::Function(function) = ModuleDef::from(item.def)
    {
        return Some(MainDef::Function(function));
    }
    let use_tree = |use_: hir_def::UseId, idx| {
        let ast_id = use_.lookup(cx.db).id;
        let tree = hir_def::src::use_tree_to_view(cx.db, ast_id, idx);
        InFile::new(ast_id.file_id, tree.syntax().text_range())
    };
    let binding = match item.import {
        Some(ImportOrGlob::Import(import)) => Some(use_tree(import.use_, import.idx)),
        Some(ImportOrGlob::Glob(glob)) => Some(use_tree(glob.use_, glob.idx)),
        None => source(cx, ModuleDef::from(item.def)).map(|node| node.map(item_range)),
    };
    binding.map(MainDef::Other)
}

/// rustc's `no_main_err`.
fn no_main(cx: &Cx<'_, '_>, root: hir::Module, non_main_fns: &[&FreeItem<'_>], main: Option<MainDef>) -> Diag {
    // alloc: the error's message
    let message = format!("`main` function not found in crate `{}`", crate_name(cx, root));
    let file = root.as_source_file_id(cx.db).map(|it| it.file_id(cx.db));
    let filename = file.map(|it| cx.file_name(it));
    let at = file.and_then(|file| Some(cx.span_in_file(file, TextRange::empty(crate_end(cx, file)?))));
    let mut diag = match &at {
        // clone: the error's span
        Some(at) => Diag::error(Some("E0601"), message, at.clone()),
        // alloc: the error's code
        None => Diag::new(Level::Error, Some(Code::Error("E0601".to_owned())), message),
    };
    let note = if non_main_fns.is_empty() {
        match &filename {
            // alloc: the error's label
            Some(filename) => format!("consider adding a `main` function to `{filename}`"),
            // alloc: the error's label
            None => "consider adding a `main` function at the crate level".to_owned(),
        }
    } else {
        for item in non_main_fns {
            diag.span_note(cx.span(item.def_range()), "here is a function named `main`");
        }
        diag.note("you have one or more functions named `main` not defined at the crate level");
        diag.help("consider moving the `main` function definitions");
        match &filename {
            // alloc: the error's label
            Some(filename) => format!("the main function must be defined at the crate level (in `{filename}`)"),
            // alloc: the error's label
            None => "the main function must be defined at the crate level".to_owned(),
        }
    };
    match at {
        Some(at) => {
            diag.span_label(at, note);
        }
        None => {
            diag.note(note);
        }
    }
    if let Some(MainDef::Other(binding)) = main {
        diag.span_label(cx.span(binding), "non-function item at `crate::main` is found");
    }
    diag.fact(cx.resolves_fact(root, "main"), "what `main` is in the crate root");
    for item in non_main_fns {
        diag.fact(cx.resolves_fact(item.module, "main"), "the items named `main` below the crate root");
    }
    diag
}

/// The crate's name: `--crate-name`, else rustc's from the root file's name.
fn crate_name(cx: &Cx<'_, '_>, root: hir::Module) -> String {
    if let Some(name) = cx.krate.display_name(cx.db) {
        // alloc: the error's message
        return name.crate_name().to_string();
    }
    let file = root.as_source_file_id(cx.db).map(|it| cx.file_name(it.file_id(cx.db))).unwrap_or_default();
    let stem = file.rsplit('/').next().unwrap_or(&file);
    stem.strip_suffix(".rs").unwrap_or(stem).replace('-', "_")
}

/// Where the crate root's span ends (rustc's `inner_span.hi()`): after its last token, or at
/// the end of a file with none; `None` for an empty file, which rustc cannot point into.
fn crate_end(cx: &Cx<'_, '_>, file: FileId) -> Option<TextSize> {
    let text = cx.file_text(file);
    if text.is_empty() {
        return None;
    }
    let root = cx.krate.root_module(cx.db).as_source_file_id(cx.db)?;
    let tree = cx.vsema.parse(root);
    let mut token = tree.syntax().last_token();
    while let Some(it) = token {
        if !is_rustc_trivia(it) {
            return Some(it.text_range().end());
        }
        token = it.prev_token();
    }
    Some(TextSize::of(text))
}

/// What rustc's lexer gives no token for: whitespace, comments that are not doc comments
/// (a doc comment is an `INNER_DOC_COMMENT` or `OUTER_DOC_COMMENT` token), the shebang and the
/// frontmatter.
fn is_rustc_trivia(token: ast::Token<'_>) -> bool {
    matches!(
        token.kind(),
        SyntaxKind::WHITESPACE | SyntaxKind::COMMENT | SyntaxKind::SHEBANG | SyntaxKind::FRONTMATTER
    )
}

/// An item rustc's `hir_free_items` gives: a module's, a block's, not an associated or a
/// foreign item. Only those `entry` looks at are kept: a `#[rustc_main]` function, an item
/// named `main` below the crate root.
struct FreeItem<'db> {
    /// Named `main`.
    main: bool,
    /// Directly in the crate root.
    at_root: bool,
    /// The module it is declared in (a block's for an item in a body).
    module: hir::Module,
    node: InFile<ast::Node<'db>>,
    rustc_main: bool,
    /// Where it is in the crate's source: rustc's order of items.
    place: Vec<u32>,
}

impl FreeItem<'_> {
    /// rustc's `def_span`.
    fn def_range(&self) -> InFile<TextRange> {
        self.node.map(free_item_def_range)
    }
}

/// rustc's `def_span` of a free item: a const's or a static's ends with its type, a macro's
/// with its name, the rest as [`def_range`].
fn free_item_def_range(node: ast::Node<'_>) -> TextRange {
    let start = item_range(node).start();
    let end = if let Some(it) = ast::Const::cast(node) {
        it.ty().map(|ty| ty.syntax().text_range().end())
    } else if let Some(it) = ast::Static::cast(node) {
        it.ty().map(|ty| ty.syntax().text_range().end())
    } else if let Some(it) = ast::MacroRules::cast(node) {
        it.name().map(|name| name.syntax().text_range().end())
    } else if let Some(it) = ast::MacroDef::cast(node) {
        it.name().map(|name| name.syntax().text_range().end())
    } else {
        None
    };
    match end {
        Some(end) if end >= start => TextRange::new(start, end),
        _ => def_range(node),
    }
}

/// The crate's free items, in rustc's order.
fn free_items<'db>(cx: &Cx<'_, 'db>) -> Vec<FreeItem<'db>> {
    let root = cx.krate.root_module(cx.db);
    // alloc: setup, once per crate; `items` is empty unless there is a `#[rustc_main]` or a `main` below the root
    let mut walk = Walk { cx, places: None, root, seen: HashSet::default(), items: Vec::new() };
    walk.module(root);
    let mut items = walk.items;
    items.sort_by(|a, b| a.place.cmp(&b.place));
    items
}

struct Walk<'a, 'b, 'db> {
    cx: &'a Cx<'b, 'db>,
    /// Made for the first item kept.
    places: Option<Places>,
    root: hir::Module,
    seen: HashSet<(HirFileId, NodeId)>,
    items: Vec<FreeItem<'db>>,
}

impl<'db> Walk<'_, '_, 'db> {
    fn module(&mut self, module: hir::Module) {
        let db = self.cx.db;
        for def in module.declarations_iter(db) {
            let foreign = match def {
                ModuleDef::Function(it) => it.extern_block(db).is_some(),
                ModuleDef::Static(it) => it.extern_block(db).is_some(),
                _ => false,
            };
            if foreign {
                continue;
            }
            if let Some(node) = source(self.cx, def) {
                self.push(module, def.name(db), node);
            }
            bodies(self.cx, def, |body| self.blocks(body));
        }
        for macro_ in module.legacy_macros_iter(db) {
            if macro_.module(db) != module {
                continue;
            }
            if let Some(node) = self.cx.vsema.source(macro_) {
                let node = node.map(|it| it.either(|it| it.syntax(), |it| it.syntax()));
                self.push(module, Some(macro_.name(db)), node);
            }
        }
        let module_id = hir_def::ModuleId::from(module);
        let scope = module_id.def_map(db).module(db, module_id).scope(db);
        for id in scope.extern_crate_decls() {
            let decl = hir::ExternCrateDecl::from(id);
            if let Some(node) = self.cx.vsema.source(decl) {
                self.push(module, decl.alias_or_name(db), node.map(|it| it.syntax()));
            }
        }
        for impl_ in module.impl_defs_iter(db) {
            for item in impl_.items_iter(db) {
                if let Some(body) = assoc_body(item) {
                    self.blocks(body);
                }
            }
        }
        for child in module.children(db) {
            self.module(child);
        }
    }

    fn push(&mut self, module: hir::Module, name: Option<Name>, node: InFile<ast::Node<'db>>) {
        if !self.seen.insert((node.file_id, node.value.id())) {
            return;
        }
        let at_root = module == self.root;
        let main = name.is_some_and(|it| it.as_str() == "main");
        let rustc_main = has_attr(self.cx, node, "rustc_main");
        if !rustc_main && (!main || at_root) {
            return;
        }
        let cx = self.cx;
        let place = self.places.get_or_insert_with(|| Places::new(cx)).of(cx, node);
        self.items.push(FreeItem { main, at_root, module, rustc_main, place, node });
    }

    /// The items of the blocks of `body`.
    fn blocks(&mut self, body: hir::DefWithBody) {
        let Ok(id) = hir_def::DefWithBodyId::try_from(body) else { return };
        let db = self.cx.db;
        for (_, def_map) in Body::of(db, id).blocks(db) {
            self.module(hir::Module::from(def_map.root_module_id()));
        }
    }
}

/// Each body of `def`: its own, its variants' discriminants, a trait's default bodies.
fn bodies(cx: &Cx<'_, '_>, def: ModuleDef, f: impl FnMut(hir::DefWithBody)) {
    match def {
        ModuleDef::Adt(hir::Adt::Enum(it)) => it.variants_iter(cx.db).map(hir::DefWithBody::from).for_each(f),
        ModuleDef::Trait(it) => it.items_iter(cx.db).filter_map(assoc_body).for_each(f),
        _ => def.as_def_with_body().into_iter().for_each(f),
    }
}

fn assoc_body(item: hir::AssocItem) -> Option<hir::DefWithBody> {
    match item {
        hir::AssocItem::Function(it) => Some(it.into()),
        hir::AssocItem::Const(it) => Some(it.into()),
        hir::AssocItem::TypeAlias(_) => None,
    }
}

/// The node of a module item as written: the item, a module's declaration.
fn source<'db>(cx: &Cx<'_, 'db>, def: ModuleDef) -> Option<InFile<ast::Node<'db>>> {
    fn syntax<'db, N: ast::AstNode<'db>>(it: InFile<N>) -> InFile<ast::Node<'db>> {
        it.map(|it| it.syntax())
    }
    match def {
        ModuleDef::Module(it) => it.declaration_source_view(cx.db).map(syntax),
        ModuleDef::Function(it) => cx.vsema.source(it).map(syntax),
        ModuleDef::Adt(it) => cx.vsema.source(it).map(syntax),
        ModuleDef::Const(it) => cx.vsema.source(it).map(syntax),
        ModuleDef::Static(it) => cx.vsema.source(it).map(syntax),
        ModuleDef::Trait(it) => cx.vsema.source(it).map(syntax),
        ModuleDef::TypeAlias(it) => cx.vsema.source(it).map(syntax),
        ModuleDef::Macro(it) => {
            cx.vsema.source(it).map(|it| it.map(|it| it.either(|it| it.syntax(), |it| it.syntax())))
        }
        ModuleDef::EnumVariant(_) | ModuleDef::BuiltinType(_) => None,
    }
}

/// Whether `node` has the attribute `#[name]`, `cfg_attr` expanded.
fn has_attr(cx: &Cx<'_, '_>, node: InFile<ast::Node<'_>>, name: &str) -> bool {
    let Some(owner) = ast::AnyHasAttrs::cast(node.value) else { return false };
    let attrs = ast::attrs_including_inner(&owner);
    hir_expand::attrs::expand_cfg_attr_view(attrs, || cx.krate.cfg(cx.db), |meta, _| {
        let named = meta.path().and_then(|it| it.as_single_name_ref()).is_some_and(|it| it.text() == name);
        if named { ControlFlow::Break(()) } else { ControlFlow::Continue(()) }
    })
    .is_some()
}

/// Where each file of the crate is in its source as rustc reads it: a module's file where
/// its `mod` item is.
struct Places {
    files: HashMap<FileId, Vec<u32>>,
}

impl Places {
    fn new(cx: &Cx<'_, '_>) -> Places {
        let mut places = Places { files: HashMap::default() };
        for module in modules(cx) {
            let Some(file) = module.as_source_file_id(cx.db) else { continue };
            let file = file.file_id(cx.db);
            let place = match module.declaration_source_view(cx.db) {
                Some(declaration) => places.of(cx, declaration.map(|it| it.syntax())),
                // alloc: empty, allocates nothing
                None => Vec::new(),
            };
            places.files.insert(file, place);
        }
        places
    }

    /// Where `node` is: its file's place, then where in the file it is written (for a macro's
    /// output, where the macro is called).
    fn of(&self, cx: &Cx<'_, '_>, node: InFile<ast::Node<'_>>) -> Vec<u32> {
        let range = node.original_file_range_rooted_view(cx.db).into_file_id(cx.db);
        let mut place = self.files.get(&range.file_id).cloned().unwrap_or_default();
        place.push(range.range.start().into());
        place
    }
}

#[cfg(test)]
mod tests {
    use syntax::{Edition, NodeOrToken, SourceFile};

    use super::*;

    /// Walking back from the last token comes to every token of the file, last first; a comment
    /// is trivia unless it is a doc comment, of any kind, and the shebang and whitespace are.
    #[test]
    fn trivia_is_what_rustcs_lexer_skips() {
        let comment = |kind: SyntaxKind| {
            matches!(
                kind,
                SyntaxKind::COMMENT | SyntaxKind::INNER_DOC_COMMENT | SyntaxKind::OUTER_DOC_COMMENT | SyntaxKind::SHEBANG
            )
        };
        let mut docs = 0;
        for (text, expected) in [
            (
                "#!/usr/bin/env rust\n// c\nfn main() {}\n/* b */\n",
                // The shebang token is cargo's frontmatter parse of it, which holds its newline.
                &[("/* b */", true), ("// c", true), ("#!/usr/bin/env rust\n", true)][..],
            ),
            (
                "//! inner\n/// outer\nfn f() {}\n/** block outer */\nstruct S;\n// trailing\n",
                &[("// trailing", true), ("/** block outer */", false), ("/// outer", false), ("//! inner", false)],
            ),
            ("/*! inner block */\nfn g() {}\n/// last\n", &[("/// last", false), ("/*! inner block */", false)]),
            ("// only a comment\n", &[("// only a comment", true)]),
        ] {
            let parse = SourceFile::parse(text, Edition::CURRENT);
            let mut forward: Vec<_> =
                parse.root().descendants_with_tokens().filter_map(NodeOrToken::into_token).collect();
            forward.reverse();
            let mut back = Vec::new();
            let mut at = parse.root().last_token();
            while let Some(token) = at {
                back.push(token);
                at = token.prev_token();
            }
            assert_eq!(back, forward, "the walk back comes to every token in {text:?}");
            let mut comments = Vec::new();
            for token in back {
                match token.kind() {
                    kind if comment(kind) => comments.push((token.text(), is_rustc_trivia(token))),
                    SyntaxKind::WHITESPACE => assert!(is_rustc_trivia(token)),
                    kind => assert!(!is_rustc_trivia(token), "{kind:?} in {text:?}"),
                }
                if matches!(token.kind(), SyntaxKind::INNER_DOC_COMMENT | SyntaxKind::OUTER_DOC_COMMENT) {
                    docs += 1;
                }
            }
            assert_eq!(comments, expected, "in {text:?}");
        }
        assert_eq!(docs, 5, "each doc comment is a token of its own kind");
    }
}
