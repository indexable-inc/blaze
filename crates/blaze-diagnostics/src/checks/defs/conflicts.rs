//! A name defined twice in a namespace of a module, block, enum or trait: E0428 for two items,
//! E0252 for two imports, E0255 for an import and an item, E0254, E0259 and E0260 when one is
//! an `extern crate`, each with the suggestion to rename or remove the import.
//!
//! One [`Scope`] is reused for every scope of the crate: its bindings, their names (ranges of
//! one string), the table of names planted and the import walk's stack keep their capacity, so
//! a crate without a conflict allocates nothing per scope once they have grown.
//!
//! Follows rustc's `compiler/rustc_resolve/src/build_reduced_graph.rs`
//! (`plant_decl_into_local_module`, `build_reduced_graph_for_use_tree`), `imports.rs`
//! (`try_plant_decl_into_local_module`), `diagnostics/impls.rs` (`report_conflict`,
//! `add_suggestion_for_rename_of_use`, `add_suggestion_for_duplicate_nested_use`,
//! `find_span_of_binding_until_next_binding`, `extend_span_to_previous_binding`) and
//! `diagnostics/mod.rs` (`NameDefinedMultipleTime`) (MIT or Apache-2.0).

use std::hash::BuildHasher as _;

use blaze_hash::{FixedState, HashMap};
use blaze_ids::radix;
use blaze_items::ItemId;
use either::Either;
use hashbrown::HashTable;
use hir::{InFile, PathResolution};
use syntax::{
    TextRange, TextSize,
    view::{self as ast, AstNode, HasAttrs, HasName as _},
};

use super::{head_range_view as head_range, is_block, item_id};
use crate::{
    cx::{Cx, item_range_view as item_range},
    lower::{Found, Phase},
    model::{Applicability, Diag, Span, SuggestionStyle},
};

/// rustc's `Namespace`, in the order `per_ns` visits them.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Ns {
    Type,
    Value,
    Macro,
}

impl Ns {
    fn descr(self) -> &'static str {
        match self {
            Ns::Type => "type",
            Ns::Value => "value",
            Ns::Macro => "macro",
        }
    }
}

/// A name of a binding: a range of [`Scope::names`].
#[derive(Clone, Copy)]
struct Name {
    start: u32,
    end: u32,
}

/// What put a name in a scope: rustc's `DeclKind` with the import's kind.
#[derive(Clone, Copy)]
enum Origin {
    Def,
    ExternCrate(ExternCrate),
    Import(Import),
}

#[derive(Clone, Copy)]
struct ExternCrate {
    /// The crate's name as written, before any `as`.
    source: Name,
    /// The whole item with its attributes: rustc's `use_span_with_attributes`.
    with_attrs: TextRange,
    has_attrs: bool,
}

#[derive(Clone, Copy)]
struct Import {
    /// Where the last segment of its path ends: rustc's `source.span.hi()`.
    source_end: TextSize,
    /// Where the `use` item ends: rustc's `use_span.hi()`.
    use_end: TextSize,
    with_attrs: TextRange,
    has_attrs: bool,
    nested: bool,
}

/// A name put in a namespace of a scope: rustc's `Decl` as `report_conflict` reads it.
#[derive(Clone, Copy)]
struct Binding {
    name: Name,
    ns: Ns,
    origin: Origin,
    /// What it resolves to, to tell two imports of one item.
    res: Option<hir::ModuleDef>,
    /// rustc's `old_kind` of it, said when it is the first of two.
    kind: &'static str,
    /// Its span: the item, the `extern crate` item, or the use tree.
    range: InFile<TextRange>,
    /// rustc's `guess_head_span` of its span.
    head: InFile<TextRange>,
    item: Option<ItemId>,
}

impl Binding {
    fn is_extern_crate(&self) -> bool {
        matches!(self.origin, Origin::ExternCrate(_))
    }

    /// rustc's `is_import_user_facing`, which an `extern crate` is.
    fn is_import(&self) -> bool {
        !matches!(self.origin, Origin::Def)
    }

    fn has_attrs(&self) -> bool {
        match &self.origin {
            Origin::Def => false,
            Origin::ExternCrate(it) => it.has_attrs,
            Origin::Import(it) => it.has_attrs,
        }
    }

    /// Whether it is written after `other` (rustc compares where spans start); bindings of
    /// different files keep the order they were planted in.
    fn is_written_after(&self, other: &Binding) -> bool {
        self.range.file_id == other.range.file_id
            && self.range.value.start() > other.range.value.start()
    }
}

/// The scope bindings are planted in, and how rustc's note names it, with the buffers its
/// filling and planting reuse.
struct Scope {
    module: hir::Module,
    container: &'static str,
    bindings: Vec<Binding>,
    /// The text of every [`Name`] of `bindings`.
    names: String,
    /// The scratch of the sort of a module's items.
    scratch: Vec<Binding>,
    /// The first binding of each name and namespace planted (an index of `bindings`).
    planted: HashTable<u32>,
}

/// The buffers the walk of an import reuses, for every import of the check.
#[derive(Default)]
struct UseTrees<'db> {
    /// The use trees the walk of an import has left: each with whether it is nested and the
    /// index in `prefixes` of the path it is under.
    trees: Vec<(ast::UseTree<'db>, bool, u32)>,
    prefixes: Vec<Option<ast::Path<'db>>>,
}

impl Scope {
    fn new(module: hir::Module) -> Scope {
        Scope {
            module,
            container: "module",
            // alloc: setup, once per check; reused for every scope
            bindings: Vec::new(),
            // alloc: as above
            names: String::new(),
            // alloc: as above
            scratch: Vec::new(),
            planted: HashTable::new(),
        }
    }

    fn reset(&mut self, module: hir::Module, container: &'static str) {
        self.module = module;
        self.container = container;
        self.bindings.clear();
        self.names.clear();
    }

    fn intern(&mut self, text: &str) -> Name {
        let start = self.names.len() as u32;
        self.names.push_str(text);
        Name { start, end: self.names.len() as u32 }
    }

    fn name(&self, name: Name) -> &str {
        &self.names[name.start as usize..name.end as usize]
    }
}

pub(super) fn check(cx: &Cx<'_, '_>, scopes: &[hir::Module]) -> Vec<Found> {
    // alloc: empty, allocates nothing until a conflict
    let mut found = Vec::new();
    let mut seen: HashMap<String, InFile<TextRange>> = HashMap::default();
    let Some(&first) = scopes.first() else { return found };
    let root = cx.krate.root_module(cx.db);
    let extern_prelude = ExternPrelude::of(cx, root);
    let mut scope = Scope::new(first);
    // alloc: setup, once per check; reused for every import
    let mut uses = UseTrees::default();
    for &module in scopes {
        module_scope(cx, &mut scope, &mut uses, module);
        plant(cx, &mut scope, &extern_prelude, &mut seen, &mut found);
        for def in module.declarations_iter(cx.db) {
            match def {
                hir::ModuleDef::Adt(hir::Adt::Enum(it)) => enum_scope(cx, &mut scope, module, it),
                hir::ModuleDef::Trait(it) => trait_scope(cx, &mut scope, module, it),
                _ => continue,
            }
            plant(cx, &mut scope, &extern_prelude, &mut seen, &mut found);
        }
    }
    found
}

/// The names of the crate's extern prelude, with whether an `extern crate` item put each there.
struct ExternPrelude {
    names: HashMap<String, bool>,
}

impl ExternPrelude {
    fn of(cx: &Cx<'_, '_>, root: hir::Module) -> ExternPrelude {
        let mut names: HashMap<String, bool> = cx
            .krate
            .dependencies_iter(cx.db)
            // alloc: setup, once per check: the crate's dependencies
            .map(|dep| (dep.name.as_str().to_owned(), false))
            // alloc: as above
            .collect();
        for decl in root.extern_crate_decls(cx.db) {
            if let Some(name) = decl.alias_or_name(cx.db) {
                // alloc: as above, the crate's `extern crate` items
                names.insert(name.as_str().to_owned(), true);
            }
        }
        ExternPrelude { names }
    }

    /// rustc's `extern_prelude.get(ident).is_none_or(|it| it.introduced_by_item())`.
    fn from_item(&self, name: &str) -> bool {
        self.names.get(name).is_none_or(|&by_item| by_item)
    }
}

fn hash_of(scope: &Scope, binding: &Binding) -> u64 {
    FixedState.hash_one((scope.name(binding.name), binding.ns))
}

/// Each binding of `scope` in the order rustc plants them, each conflict reported as rustc
/// reports it.
fn plant(
    cx: &Cx<'_, '_>,
    scope: &mut Scope,
    extern_prelude: &ExternPrelude,
    seen: &mut HashMap<String, InFile<TextRange>>,
    found: &mut Vec<Found>,
) {
    let mut planted = std::mem::take(&mut scope.planted);
    planted.clear();
    {
        let scope = &*scope;
        for (index, binding) in scope.bindings.iter().enumerate() {
            let name = scope.name(binding.name);
            let hash = hash_of(scope, binding);
            let same = |&at: &u32| {
                let it = &scope.bindings[at as usize];
                it.ns == binding.ns && scope.name(it.name) == name
            };
            let Some(&old) = planted.find(hash, same) else {
                planted.insert_unique(hash, index as u32, |&at| hash_of(scope, &scope.bindings[at as usize]));
                continue;
            };
            let old = &scope.bindings[old as usize];
            let (old, new) = match old.is_written_after(binding) {
                true => (binding, old),
                false => (old, binding),
            };
            let new_name = scope.name(new.name);
            if seen.get(new_name) == Some(&new.head) {
                continue;
            }
            found.push(Found::new(conflict(cx, scope, old, new, extern_prelude), Phase::Resolve));
            // alloc: the conflict's name, once per error
            seen.insert(new_name.to_owned(), new.head);
        }
    }
    scope.planted = planted;
}

/// rustc's `report_conflict` of `new`, written after `old` in the same namespace.
fn conflict(
    cx: &Cx<'_, '_>,
    scope: &Scope,
    old: &Binding,
    new: &Binding,
    extern_prelude: &ExternPrelude,
) -> Diag {
    let name = scope.name(new.name);
    let code = match (old.is_extern_crate(), new.is_extern_crate()) {
        (true, true) => "E0259",
        (true, _) | (_, true) => match old.is_import() && new.is_import() {
            true => "E0254",
            false => "E0260",
        },
        _ => match (old.is_import(), new.is_import()) {
            (false, false) => "E0428",
            (true, true) => "E0252",
            _ => "E0255",
        },
    };
    let span = cx.span(new.head);
    // alloc: the error's message
    let message = format!("the name `{name}` is defined multiple times");
    // clone: a span, on the error path
    let mut diag = Diag::error(Some(code), message, span.clone());
    let label = match new.is_import() {
        // alloc: the error's label
        true => format!("`{name}` reimported here"),
        // alloc: as above
        false => format!("`{name}` redefined here"),
    };
    diag.span_label(span, label);
    if old.head != new.head {
        let what = match old.is_import() {
            true => "import",
            false => "definition",
        };
        // alloc: the error's label
        diag.span_label(cx.span(old.head), format!("previous {what} of the {} `{name}` here", old.kind));
    }
    // alloc: the error's note
    diag.note(format!(
        "`{name}` must be defined only once in the {} namespace of this {}",
        new.ns.descr(),
        scope.container
    ));

    let import = match (&new.origin, &old.origin) {
        (Origin::Def, Origin::Def) => None,
        _ if new.is_import() && old.is_import() && (new.has_attrs() || old.has_attrs()) => {
            match old.has_attrs() {
                true => Some((new, true)),
                false => Some((old, true)),
            }
        }
        _ if new.is_import() => Some((new, old.is_import())),
        _ => Some((old, new.is_import())),
    };
    let duplicate = new.res.is_some() && new.res == old.res;
    let should_remove = duplicate
        && (new.is_extern_crate() || old.is_extern_crate() || extern_prelude.from_item(name));
    match import {
        Some((binding, true)) if should_remove => remove(cx, &mut diag, binding),
        Some((binding, _)) => rename(cx, &mut diag, scope, binding),
        None => {}
    }

    // alloc: the error's fact
    diag.fact(cx.resolves_fact(scope.module, name), format!("what `{name}` names in its scope"));
    for binding in [old, new] {
        if let Some(item) = binding.item {
            // alloc: as above
            diag.fact(blaze_items::Fact::Name(cx.key(item)), format!("the definition of `{name}`"));
        }
    }
    diag
}

/// rustc's `add_suggestion_for_duplicate_nested_use` and `ToolOnlyRemoveUnnecessaryImport`:
/// removing the import, which names what the other binding names.
fn remove(cx: &Cx<'_, '_>, diag: &mut Diag, binding: &Binding) {
    let file = binding.range.file_id;
    let (with_attrs, nested) = match &binding.origin {
        Origin::Import(it) => (it.with_attrs, it.nested.then_some(it.use_end)),
        Origin::ExternCrate(it) => (it.with_attrs, None),
        Origin::Def => return,
    };
    let whole = cx.span(InFile::new(file, with_attrs));
    let Some(use_end) = nested else {
        hidden_removal(diag, whole);
        return;
    };
    let text = cx.root(file).text();
    let range = binding.range.value;
    let (closing_brace, until_next) = until_next_binding(text, range, use_end);
    if !closing_brace {
        diag.suggestion(cx.span(InFile::new(file, until_next)), "remove unnecessary import", "", Applicability::MaybeIncorrect);
        return;
    }
    match to_previous_binding(text, until_next) {
        Some(it) => hidden_removal(diag, cx.span(InFile::new(file, it))),
        None => {
            diag.suggestion(whole, "remove unnecessary import", "", Applicability::MaybeIncorrect);
        }
    }
}

fn hidden_removal(diag: &mut Diag, span: Span) {
    diag.suggestion_with_style(
        // alloc: the error's suggestion
        vec![(span, String::new())],
        "remove unnecessary import",
        Applicability::MaybeIncorrect,
        SuggestionStyle::CompletelyHidden,
    );
}

/// rustc's `find_span_of_binding_until_next_binding`: the binding with the commas and spaces
/// after it, and whether a `}` came next.
fn until_next_binding(text: &str, binding: TextRange, use_end: TextSize) -> (bool, TextRange) {
    let after = text.get(usize::from(binding.end())..usize::from(use_end)).unwrap_or("");
    let mut closing_brace = false;
    let taken: usize = after
        .chars()
        .take_while(|&ch| {
            if ch == '}' {
                closing_brace = true;
            }
            ch == ' ' || ch == ','
        })
        .map(char::len_utf8)
        .sum();
    let end = binding.end() + TextSize::from(taken as u32);
    (closing_brace, TextRange::new(binding.start(), end))
}

/// rustc's `extend_span_to_previous_binding`: back to the comma before it, unless the list's
/// `{` is nearer (it is the only binding).
fn to_previous_binding(text: &str, range: TextRange) -> Option<TextRange> {
    let before = text.get(..usize::from(range.start()))?;
    let comma = before.rfind(',')?;
    let brace = before.rfind('{')?;
    let after_comma = before.len() - comma - 1;
    let after_brace = before.len() - brace - 1;
    if after_comma > after_brace {
        return None;
    }
    let start = range.start() - TextSize::from(after_comma as u32 + 1);
    Some(TextRange::new(start, range.end()))
}

/// rustc's `add_suggestion_for_rename_of_use`: `as Other{name}` or `as other_{name}`.
fn rename(cx: &Cx<'_, '_>, diag: &mut Diag, scope: &Scope, binding: &Binding) {
    let name = scope.name(binding.name);
    let suggested = match name.chars().next().is_some_and(char::is_uppercase) {
        // alloc: the error's suggestion
        true => format!("Other{name}"),
        // alloc: as above
        false => format!("other_{name}"),
    };
    let message = "you can use `as` to change the binding name of the import";
    let file = binding.range.file_id;
    let range = binding.range.value;
    let (at, code) = match &binding.origin {
        Origin::Import(import) => {
            let snippet = cx.root(file).text();
            let snippet = snippet.get(usize::from(range.start())..usize::from(range.end())).unwrap_or("");
            let pos = import.source_end.checked_sub(range.start());
            match pos {
                Some(pos) if usize::from(pos) <= snippet.len() => {
                    let end = range.end() - TextSize::from(u32::from(snippet.ends_with(';')));
                    // alloc: the error's suggestion
                    (TextRange::new(range.start() + pos, end), format!(" as {suggested}"))
                }
                _ => {
                    diag.span_label(cx.span(binding.range), message);
                    return;
                }
            }
        }
        // alloc: the error's suggestion
        Origin::ExternCrate(it) => (range, format!("extern crate {} as {suggested};", scope.name(it.source))),
        Origin::Def => return,
    };
    diag.suggestion(cx.span(InFile::new(file, at)), message, code, Applicability::MaybeIncorrect);
}

/// The bindings of a module or block, in the order rustc plants them: items and `extern crate`s
/// as written (those a macro wrote after), then single imports.
fn module_scope<'db>(cx: &Cx<'_, 'db>, scope: &mut Scope, uses: &mut UseTrees<'db>, module: hir::Module) {
    let db = cx.db;
    let container = if is_block(cx, module) { "block" } else { "module" };
    scope.reset(module, container);
    for def in module.declarations_iter(db) {
        item_bindings(cx, scope, def);
    }
    for decl in module.extern_crate_decls(db) {
        extern_crate_binding(cx, scope, decl);
    }
    let home = module.definition_source_file_id(db);
    let key = |it: &Binding| match it.range.file_id == home {
        true => radix::pack_32(0, u32::from(it.range.value.start())),
        false => radix::pack_32(1, 0),
    };
    radix::sort_copy_by_key_in(&mut scope.bindings, key, &mut scope.scratch);
    for decl in module.use_decls(db) {
        let source = decl.source_view(db);
        // Records the file's tree with `cx.vsema`, which resolves the import's paths.
        cx.root(source.file_id);
        import_bindings(cx, scope, uses, source);
    }
}

/// The variants of an enum, each in both namespaces, read from its syntax (the front end keeps
/// one variant per name).
fn enum_scope(cx: &Cx<'_, '_>, scope: &mut Scope, module: hir::Module, enum_: hir::Enum) {
    scope.reset(module, "enum");
    let Some(source) = cx.vsema.source(enum_) else { return };
    let Some(list) = source.value.variant_list() else { return };
    let item = Some(ItemId::Adt(hir::Adt::from(enum_).into()));
    for variant in list.variants().filter(|&it| cx.cfg_enabled(it.syntax())) {
        let Some(name) = variant.name() else { continue };
        let name = scope.intern(name.text());
        let node = variant.syntax();
        let (range, head) = (source.with_value(item_range(node)), source.with_value(head_range(node)));
        for (ns, kind) in [(Ns::Type, "type"), (Ns::Value, "value")] {
            scope.bindings.push(Binding { name, ns, origin: Origin::Def, res: None, kind, range, head, item });
        }
    }
}

/// The associated items of a trait, functions and consts as values, types as types.
fn trait_scope(cx: &Cx<'_, '_>, scope: &mut Scope, module: hir::Module, trait_: hir::Trait) {
    let db = cx.db;
    scope.reset(module, "trait");
    let item = Some(ItemId::Trait(trait_.into()));
    for assoc in trait_.items_iter(db) {
        let (name, ns, at) = match assoc {
            hir::AssocItem::Function(it) => (it.name(db), Ns::Value, cx.vsema.source(it).map(ranges)),
            hir::AssocItem::Const(it) => {
                let Some(name) = it.name(db) else { continue };
                (name, Ns::Value, cx.vsema.source(it).map(ranges))
            }
            hir::AssocItem::TypeAlias(it) => (it.name(db), Ns::Type, cx.vsema.source(it).map(ranges)),
        };
        let Some((range, head)) = at else { continue };
        if name.as_str() == "_" {
            continue;
        }
        let name = scope.intern(name.as_str());
        let kind = ns.descr();
        scope.bindings.push(Binding { name, ns, origin: Origin::Def, res: None, kind, range, head, item });
    }
}

/// The span of an item's syntax and rustc's `guess_head_span` of it.
fn ranges<'t, N: AstNode<'t>>(node: InFile<N>) -> (InFile<TextRange>, InFile<TextRange>) {
    node_ranges(node.map(|it| it.syntax()))
}

fn node_ranges(node: InFile<ast::Node<'_>>) -> (InFile<TextRange>, InFile<TextRange>) {
    (node.with_value(item_range(node.value)), node.with_value(head_range(node.value)))
}

/// The names an item puts in its module: types, modules and traits in the type namespace,
/// functions, consts, statics and the constructors of tuple and unit structs in the value
/// namespace, macros in the macro namespace.
fn item_bindings(cx: &Cx<'_, '_>, scope: &mut Scope, def: hir::ModuleDef) {
    let db = cx.db;
    let Some(name) = def.name(db) else { return };
    if name.as_str() == "_" {
        return;
    }
    let Some((range, head)) = def_ranges(cx, def) else { return };
    let namespaces: &[Ns] = match def {
        hir::ModuleDef::Module(_) | hir::ModuleDef::Trait(_) | hir::ModuleDef::TypeAlias(_) => {
            &[Ns::Type]
        }
        hir::ModuleDef::Adt(hir::Adt::Struct(it)) => match it.kind(db) {
            hir::StructKind::Record => &[Ns::Type],
            hir::StructKind::Tuple | hir::StructKind::Unit => &[Ns::Type, Ns::Value],
        },
        hir::ModuleDef::Adt(_) => &[Ns::Type],
        hir::ModuleDef::Function(_) | hir::ModuleDef::Const(_) | hir::ModuleDef::Static(_) => {
            &[Ns::Value]
        }
        hir::ModuleDef::Macro(_) => &[Ns::Macro],
        hir::ModuleDef::EnumVariant(_) | hir::ModuleDef::BuiltinType(_) => return,
    };
    let item = item_id(db, def);
    let name = scope.intern(name.as_str());
    for &ns in namespaces {
        let kind = kind_of(ns, def);
        scope.bindings.push(Binding { name, ns, origin: Origin::Def, res: Some(def), kind, range, head, item });
    }
}

/// rustc's `old_kind`: what a binding is called when it is the first of two.
fn kind_of(ns: Ns, def: hir::ModuleDef) -> &'static str {
    match (ns, def) {
        (Ns::Value, _) => "value",
        (Ns::Macro, _) => "macro",
        (Ns::Type, hir::ModuleDef::Module(_)) => "module",
        (Ns::Type, hir::ModuleDef::Trait(_)) => "trait",
        (Ns::Type, _) => "type",
    }
}

/// The span and head of a definition's syntax: a module's `mod` item, else the item.
fn def_ranges(cx: &Cx<'_, '_>, def: hir::ModuleDef) -> Option<(InFile<TextRange>, InFile<TextRange>)> {
    let db = cx.db;
    Some(match def {
        hir::ModuleDef::Module(it) => ranges(it.declaration_source_view(db)?),
        hir::ModuleDef::Function(it) => ranges(cx.vsema.source(it)?),
        hir::ModuleDef::Adt(it) => ranges(cx.vsema.source(it)?),
        hir::ModuleDef::EnumVariant(it) => ranges(cx.vsema.source(it)?),
        hir::ModuleDef::Const(it) => ranges(cx.vsema.source(it)?),
        hir::ModuleDef::Static(it) => ranges(cx.vsema.source(it)?),
        hir::ModuleDef::Trait(it) => ranges(cx.vsema.source(it)?),
        hir::ModuleDef::TypeAlias(it) => ranges(cx.vsema.source(it)?),
        hir::ModuleDef::Macro(it) => {
            let source = cx.vsema.source(it)?;
            node_ranges(source.map(|it| match it {
                Either::Left(it) => it.syntax(),
                Either::Right(it) => it.syntax(),
            }))
        }
        hir::ModuleDef::BuiltinType(_) => return None,
    })
}

/// An `extern crate`'s name in the type namespace.
fn extern_crate_binding(cx: &Cx<'_, '_>, scope: &mut Scope, decl: hir::ExternCrateDecl) {
    let db = cx.db;
    let Some(name) = decl.alias_or_name(db) else { return };
    if name.as_str() == "_" {
        return;
    }
    let Some(source) = cx.vsema.source(decl) else { return };
    let node = source.value.syntax();
    let res = decl.resolved_crate(db).map(|it| hir::ModuleDef::Module(it.root_module(db)));
    let name = scope.intern(name.as_str());
    let crate_name = scope.intern(decl.name(db).as_str());
    scope.bindings.push(Binding {
        name,
        ns: Ns::Type,
        origin: Origin::ExternCrate(ExternCrate {
            source: crate_name,
            with_attrs: node.text_range(),
            has_attrs: source.value.attrs().next().is_some(),
        }),
        res,
        kind: "extern crate",
        range: source.with_value(item_range(node)),
        head: source.with_value(item_range(node)),
        item: None,
    });
}

/// The single imports of a `use` item, each in every namespace its path resolves in (globs
/// and `_` bind no name that can conflict).
fn import_bindings<'db>(cx: &Cx<'_, 'db>, scope: &mut Scope, uses: &mut UseTrees<'db>, item: InFile<ast::Use<'db>>) {
    let Some(tree) = item.value.use_tree() else { return };
    let site = Import {
        source_end: TextSize::from(0),
        use_end: item.value.syntax().text_range().end(),
        with_attrs: item.value.syntax().text_range(),
        has_attrs: item.value.attrs().next().is_some(),
        nested: false,
    };
    let UseTrees { trees, prefixes } = uses;
    trees.clear();
    prefixes.clear();
    prefixes.push(None);
    trees.push((tree, false, 0));
    while let Some((tree, nested, parent)) = trees.pop() {
        if let Some(list) = tree.use_tree_list() {
            let prefix = match tree.path() {
                Some(path) => {
                    prefixes.push(Some(path));
                    (prefixes.len() - 1) as u32
                }
                None => parent,
            };
            let at = trees.len();
            trees.extend(list.use_trees().map(|it| (it, true, prefix)));
            trees[at..].reverse();
            continue;
        }
        if tree.star_token().is_some() {
            continue;
        }
        let Some(path) = tree.path() else { continue };
        let Some(segment) = path.segment() else { continue };
        let is_self = segment.self_token().is_some() && path.qualifier().is_none();
        let source = match is_self {
            true => prefixes[parent as usize].and_then(|it| it.segment()),
            false => Some(segment),
        };
        let Some(source) = source else { continue };
        let source_name = source.name_ref();
        let rename = tree.rename();
        let name = match &rename {
            Some(rename) if rename.underscore_token().is_some() => continue,
            Some(rename) => match rename.name() {
                Some(it) => scope.intern(it.text()),
                None => continue,
            },
            None => match &source_name {
                Some(it) => scope.intern(it.text()),
                None => continue,
            },
        };
        let Some(resolved) = cx.vsema.resolve_path_per_ns(path) else { continue };
        let range = item.with_value(tree.syntax().text_range());
        let import = Import { source_end: source.syntax().text_range().end(), nested, ..site };
        let namespaces = [
            (Ns::Type, resolved.type_ns),
            (Ns::Value, if is_self { None } else { resolved.value_ns }),
            (Ns::Macro, if is_self { None } else { resolved.macro_ns }),
        ];
        for (ns, res) in namespaces {
            let Some(PathResolution::Def(def)) = res else { continue };
            scope.bindings.push(Binding {
                name,
                ns,
                origin: Origin::Import(import),
                res: Some(def),
                kind: kind_of(ns, def),
                range,
                head: range,
                item: item_id(cx.db, def),
            });
        }
    }
    prefixes.clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(text: &str, needle: &str) -> TextRange {
        let at = text.find(needle).unwrap();
        TextRange::at(TextSize::from(at as u32), TextSize::of(needle))
    }

    #[test]
    fn a_nested_duplicate_is_removed_with_the_comma_after_it() {
        let text = "use issue_52891::{d, a, e};";
        let (brace, until) = until_next_binding(text, range(text, "a"), TextSize::of(text));
        assert!(!brace);
        assert_eq!(&text[until], "a, ");
    }

    #[test]
    fn the_last_nested_duplicate_is_removed_with_the_comma_before_it() {
        let text = "use issue_52891::{d, e, a};";
        let a = TextRange::at(range(text, "a}").start(), TextSize::of("a"));
        let (brace, until) = until_next_binding(text, a, TextSize::of(text));
        assert!(brace);
        assert_eq!(&text[until], "a");
        assert_eq!(&text[to_previous_binding(text, until).unwrap()], ", a");
        let text = "use issue_52891::{self};";
        let self_ = range(text, "self");
        assert_eq!(to_previous_binding(text, self_), None);
    }
}
