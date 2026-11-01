//! What rustc's `check_unused` lints once the crate is resolved: `#[macro_use]` imports
//! (`macro_use_extern_crate`, `unused_imports`), the `use` trees nothing is found through
//! (`unused_imports`, one diagnostic per `use` item, with the spans rustfix removes),
//! `extern crate` items (`unused_extern_crates`), and single imports of what is in scope
//! without them (`redundant_imports`); then, after type checking, the trait imports only a
//! method lookup could have used and none did (`unused_imports`).
//!
//! An `extern crate` item of a crate that may provide a global allocator, a panic handler, a
//! panic runtime, the compiler builtins or an externally implementable item is never
//! unused (rustc keeps those); where that can't be told of the crate, it is not linted. A
//! redundant import is looked for in the `use` item's module, its globs and the blocks around
//! it, the extern prelude and the builtin types; one rustc finds redundant through the
//! standard library's prelude, or in the macro namespace, is not linted.
//!
//! Follows rustc's `compiler/rustc_resolve/src/check_unused.rs` (`check_unused`,
//! `UnusedImportCheckVisitor`, `calc_unused_spans`, `report_unused_extern_crate_items`),
//! `compiler/rustc_resolve/src/imports.rs` (`check_for_redundant_imports`),
//! `compiler/rustc_resolve/src/lib.rs` (`module_to_string`),
//! `compiler/rustc_span/src/source_map.rs` (`guess_head_span`) and
//! `compiler/rustc_hir_analysis/src/check_unused.rs` (`check_unused_traits`) (MIT or
//! Apache-2.0).

use std::sync::LazyLock;

use base_db::{CrateOrigin, LangCrateOrigin};
use blaze_hash::{FixedState, HashMap, HashSet};
use blaze_mem::Frame;
use cfg::CfgAtom;
use hir::InFile;
use hir_def::{ModuleDefId, ModuleId};
use hir_expand::name::Name;
use intern::sym;
use memchr::memmem;
use smallvec::SmallVec;
use syntax::{
    NodeOrToken, TextRange,
    view::{self as ast, AstNode as _, HasName, Node},
};

use super::used::{
    Binding, Entry, ExternCrate, Ns, SmallSet, Tree, TreeKind, UseItem, Used, Uses, entry,
};
use crate::{
    cx::{Cx, item_range_view},
    lints::registry::LintId,
    lower::{Found, Phase},
    model::{Applicability, Diag, Span, SuggestionStyle},
};
use stdx::list::Join;

/// What `check_unused` finds.
pub(super) struct Unused {
    /// Its lints, in the order rustc buffers them.
    pub(super) resolve: Vec<Found>,
    /// What `check_unused_traits` lints after type checking.
    pub(super) bodies: Vec<Found>,
    /// The `use` trees found unused.
    pub(super) unused: HashSet<Binding>,
    /// The single imports found redundant.
    pub(super) redundant: HashSet<Binding>,
}

impl Unused {
    pub(super) fn of(cx: &Cx<'_, '_>, uses: &Uses<'_, '_>) -> Unused {
        let mut out =
            // alloc: both hold only diagnostics that fire; empty, they allocate nothing
            Unused { resolve: Vec::new(), bodies: Vec::new(), unused: HashSet::default(), redundant: HashSet::default() };
        let extern_crates = cx.lint_may_fire(LintId::named("unused_extern_crates"));
        let mut maybe_unused_crates = HashSet::default();
        for krate in &uses.crates {
            out.macro_use(cx, uses, krate);
            let binding = Binding::ExternCrate(krate.id);
            if extern_crates
                && !krate.public
                && !uses.used.contains_key(&binding)
                && krate.krate.is_some_and(|it| may_be_unused(cx, it))
            {
                maybe_unused_crates.insert(krate.id);
            }
        }

        let mut candidates = Candidates::of(cx.frame(), &uses.candidates);
        // alloc: holds only the `use` items with an unused tree, each linted by `unused_imports`
        let mut unused_trees: Vec<(usize, Trees)> = Vec::new();
        for (i, item) in uses.items.iter().enumerate() {
            let mut unused = Trees::default();
            for (j, tree) in item.trees.iter().enumerate() {
                if let Some(binding) = tree.binding
                    && uses.exported.contains(&binding)
                {
                    if tree.kind == TreeKind::Simple
                        && tree.underscore
                        && !matches!(entry(&tree.imported, Ns::Types), Some(Entry { def: ModuleDefId::TraitId(_), .. }))
                    {
                        unused.insert_growing(j);
                    }
                    candidates.swap_remove(binding);
                    continue;
                }
                match tree.kind {
                    TreeKind::List => {
                        if tree.node.use_tree_list().is_none_or(|it| it.use_trees().next().is_none()) {
                            unused.insert_growing(j);
                        }
                    }
                    TreeKind::Simple | TreeKind::Glob => {
                        let Some(binding) = tree.binding else { continue };
                        if is_used(uses, binding, tree) {
                            candidates.swap_remove(binding);
                            unused.remove(j);
                        } else if !candidates.contains(binding) {
                            unused.insert_growing(j);
                        }
                    }
                }
            }
            if !unused.is_empty() {
                for j in unused.iter() {
                    if let Some(binding) = item.trees[j].binding {
                        out.unused.insert(binding);
                    }
                }
                unused_trees.push((i, unused));
            }
        }

        if extern_crates {
            for krate in &uses.crates {
                out.extern_crate(cx, uses, krate, &maybe_unused_crates);
            }
        }
        for (i, unused) in &unused_trees {
            out.unused_imports(cx, &uses.items[*i], unused);
        }

        // What is redundant is read by `unused_qualifications` too.
        let check_redundant = cx.lint_may_fire(LintId::named("redundant_imports"))
            || cx.lint_may_fire(LintId::named("unused_qualifications"));
        let mut seen: hashbrown::HashSet<ModuleId, FixedState, _> = hashbrown::HashSet::with_capacity_and_hasher_in(
            if check_redundant { uses.by_module.len() } else { 0 },
            FixedState,
            cx.frame(),
        );
        for module in uses.items.iter().map(|it| it.module).filter(|_| check_redundant) {
            if !seen.insert(module) {
                continue;
            }
            for item in uses.items_of(module) {
                for tree in &item.trees {
                    let Some(binding) = tree.binding else { continue };
                    if tree.kind != TreeKind::Simple || out.unused.contains(&binding) {
                        continue;
                    }
                    if let Some(found) = redundant(cx, uses, item, tree, binding) {
                        out.redundant.insert(binding);
                        out.resolve.push(found);
                    }
                }
            }
        }

        for binding in candidates.order {
            let Some((item, tree)) = uses.tree(binding) else { continue };
            if item.public || uses.typeck.contains(&binding) {
                continue;
            }
            let Some(path) = tree.node.path() else { continue };
            let span = cx.span(InFile::new(item.node.file_id, path.syntax().text_range()));
            let message = match cx.snippet(&span) {
                // alloc: the message of a lint that fires
                Some(snippet) => format!("unused import: `{snippet}`"),
                // alloc: the message of a lint that fires
                None => "unused import".to_owned(),
            };
            let mut diag = Diag::lint("unused_imports", message, span);
            if let Some(name) = &tree.name {
                // alloc: the fact of a lint that fires
                diag.fact(cx.resolves_fact(item.module.into(), name.as_str()), format!("what uses `{}`", name.as_str()));
            }
            let root = item.trees.first().is_some_and(|it| it.node == tree.node);
            let at = if root { item.node.value.syntax() } else { tree.node.syntax() };
            out.bodies.push(Found::lint(diag, Phase::Bodies, InFile::new(item.node.file_id, ast::ptr(at))));
        }
        out
    }

    /// The lints of the `#[macro_use]` imports of `krate`: rustc's `MACRO_USE_EXTERN_CRATE`
    /// for a used one, `UNUSED_IMPORTS` for one nothing is found through.
    fn macro_use(&mut self, cx: &Cx<'_, '_>, uses: &Uses<'_, '_>, krate: &ExternCrate<'_>) {
        let Some(loaded) = krate.krate else { return };
        let at = krate.node.map(|it| ast::ptr(it.syntax()));
        for (range, name) in &krate.macro_use {
            if let Some(name) = name
                && !exports_macro(cx, loaded, name)
            {
                continue;
            }
            let used = uses.macro_uses.contains(&(krate.id, *name));
            let lint = match used {
                true => LintId::named("macro_use_extern_crate"),
                false => LintId::named("unused_imports"),
            };
            if !cx.lint_may_fire(lint) {
                continue;
            }
            let span = cx.span(InFile::new(krate.node.file_id, *range));
            let mut diag = if used {
                let mut diag = Diag::lint(
                    "macro_use_extern_crate",
                    "applying the `#[macro_use]` attribute to an `extern crate` item is deprecated",
                    span,
                );
                diag.help("remove it and import macros at use sites with a `use` item instead");
                diag
            } else {
                Diag::lint("unused_imports", "unused `#[macro_use]` import", span)
            };
            let module = hir::Module::from(krate.module);
            let name = krate.name.as_str();
            // alloc: the fact of a lint that fires
            diag.fact(cx.resolves_fact(module, name), format!("which macros of `{name}` are used"));
            self.resolve.push(Found::lint(diag, Phase::Resolve, at));
        }
    }

    /// `unused_extern_crates` of `krate`: unused, or one a `use` could replace.
    fn extern_crate(&mut self, cx: &Cx<'_, '_>, uses: &Uses<'_, '_>, krate: &ExternCrate<'_>, maybe_unused: &HashSet<hir_def::ExternCrateId>) {
        let node = krate.node;
        let at = node.map(|it| ast::ptr(it.syntax()));
        let name = krate.name.as_str();
        let module = hir::Module::from(krate.module);
        if !name.starts_with('_') && maybe_unused.contains(&krate.id) {
            let span = cx.span(node.map(|it| item_range_view(it.syntax())));
            // clone: the lint fires; its span is both the primary span and the label's
            let mut diag = Diag::lint("unused_extern_crates", "unused extern crate", span.clone());
            diag.span_label(span, "unused");
            let removal = cx.span(node.map(|it| with_attrs(it.syntax())));
            diag.suggestion_verbose(removal, "remove the unused `extern crate`", "", Applicability::MachineApplicable);
            // alloc: the fact of a lint that fires
            diag.fact(cx.resolves_fact(module, name), format!("what uses `{name}`"));
            self.resolve.push(Found::lint(diag, Phase::Resolve, at));
            return;
        }
        if !cx.edition.at_least_2018()
            || krate.renames
            || !uses.extern_prelude.contains(&krate.name)
            || ast::HasAttrs::attrs(&node.value).next().is_some()
            || no_implicit_prelude(cx, krate.module)
        {
            return;
        }
        let span = cx.span(node.map(|it| item_range_view(it.syntax())));
        let Some(ident) = node.value.name_ref() else { return };
        let item = item_range_view(node.value.syntax());
        let vis = match ast::HasVisibility::visibility(&node.value) {
            Some(vis) => vis.syntax().text_range(),
            None => TextRange::empty(item.start()),
        };
        let between = TextRange::new(vis.end(), ident.syntax().text_range().start());
        let code = if vis.is_empty() { "use " } else { " use " };
        let mut diag = Diag::lint("unused_extern_crates", "`extern crate` is not idiomatic in the new edition", span);
        diag.suggestion_verbose(
            cx.span(InFile::new(node.file_id, between)),
            "convert it to a `use`",
            code,
            Applicability::MachineApplicable,
        );
        // alloc: the fact of a lint that fires
        diag.fact(cx.resolves_fact(module, name), format!("what `{name}` is in the extern prelude"));
        self.resolve.push(Found::lint(diag, Phase::Resolve, at));
    }

    /// The `unused_imports` diagnostic of `item`, whose trees `unused` are unused: what it
    /// builds is the lint's, which fires unless every tree turns out used.
    fn unused_imports(&mut self, cx: &Cx<'_, '_>, item: &UseItem<'_>, unused: &Trees) {
        let file = item.node.file_id;
        let children = children(item);
        let item_span = with_attrs(item.node.value.syntax());
        let (spans, remove) = match calc_unused_spans(item, &children, unused, 0, item_span) {
            Calc::Used => return,
            // alloc: the removal of a lint that fires
            Calc::Unused { spans, remove } => (spans, vec![remove]),
            Calc::Partial { spans, remove } => (spans, remove),
        };
        // alloc: the spans of a lint that fires
        let spans: Vec<Span> = spans.into_iter().map(|it| cx.span(InFile::new(file, it))).collect();
        // alloc: the snippets of the message of a lint that fires
        let mut snippets: Vec<String> = spans.iter().filter_map(|it| cx.snippet(it)).map(|it| format!("`{it}`")).collect();
        snippets.sort();
        let list = stdx::list::list(snippets.iter(), Join::And, false);
        let message =
            // alloc: the message of a lint that fires
            if snippets.len() == 1 { format!("unused import: {list}") } else { format!("unused imports: {list}") };
        let Some((first, rest)) = spans.split_first() else { return };
        // clone: the lint fires; `spans` stays for the count of the suggestion's message
        let mut diag = Diag::lint("unused_imports", message, first.clone());
        for span in rest {
            // clone: the lint fires; `spans` stays for the count of the suggestion's message
            diag.primary(span.clone());
        }
        let remove_whole_use = remove.len() == 1 && remove[0] == item_span;
        // alloc: the removals of a lint that fires
        let remove: Vec<Span> = remove.into_iter().map(|it| cx.span(InFile::new(file, it))).collect();
        if remove.iter().all(|it| it.expansion.is_none()) {
            if remove_whole_use {
                diag.suggestion_with_style(
                    // clone: alloc: the suggestion of a lint that fires
                    vec![(remove[0].clone(), String::new())],
                    "remove the whole `use` item",
                    Applicability::MachineApplicable,
                    SuggestionStyle::CompletelyHidden,
                );
            } else {
                let message = if spans.len() == 1 { "remove the unused import" } else { "remove the unused imports" };
                diag.suggestion_with_style(
                    // alloc: the suggestion of a lint that fires
                    remove.into_iter().map(|it| (it, String::new())).collect(),
                    message,
                    Applicability::MachineApplicable,
                    SuggestionStyle::CompletelyHidden,
                );
            }
        }
        if !cx.krate.cfg(cx.db).check_atom(&CfgAtom::Flag(sym::test))
            && let Some(span) = test_module(cx, item.module)
        {
            diag.span_help(span, "if this is a test module, consider adding a `#[cfg(test)]` to the containing module");
        }
        let module = hir::Module::from(item.module);
        for j in unused.iter() {
            if let Some(name) = &item.trees[j].name {
                // alloc: the fact of a lint that fires
                diag.fact(cx.resolves_fact(module, name.as_str()), format!("what uses `{}`", name.as_str()));
            }
        }
        let at = InFile::new(file, ast::ptr(item.node.value.syntax()));
        self.resolve.push(Found::lint(diag, Phase::Resolve, at));
    }
}

/// rustc's `maybe_unused_trait_imports`, an `IndexSet`: the order, and each binding's place in
/// it. `contains` and `swap_remove` are one hash lookup each.
struct Candidates<'f> {
    order: Vec<Binding, &'f Frame>,
    at: hashbrown::HashMap<Binding, u32, FixedState, &'f Frame>,
}

impl<'f> Candidates<'f> {
    fn of(frame: &'f Frame, candidates: &[Binding]) -> Candidates<'f> {
        let mut order = Vec::with_capacity_in(candidates.len(), frame);
        order.extend_from_slice(candidates);
        let mut at = hashbrown::HashMap::with_capacity_and_hasher_in(candidates.len(), FixedState, frame);
        at.extend(candidates.iter().enumerate().map(|(i, it)| (*it, i as u32)));
        Candidates { order, at }
    }

    fn contains(&self, binding: Binding) -> bool {
        self.at.contains_key(&binding)
    }

    /// Removes `binding` as `IndexSet::swap_remove` does: the last takes its place.
    fn swap_remove(&mut self, binding: Binding) {
        let Some(at) = self.at.remove(&binding) else { return };
        self.order.swap_remove(at as usize);
        if let Some(&moved) = self.order.get(at as usize) {
            self.at.insert(moved, at);
        }
    }
}

/// A set of an item's trees by index.
type Trees = SmallSet;

/// Whether name resolution used `binding`, or it did not resolve (rustc's dummy binding of
/// an unresolved import counts as used).
fn is_used(uses: &Uses<'_, '_>, binding: Binding, tree: &Tree<'_>) -> bool {
    if uses.used.contains_key(&binding) {
        return true;
    }
    match tree.kind {
        TreeKind::Glob => tree.source.is_none(),
        _ => Ns::ALL.iter().all(|ns| entry(&tree.imported, *ns).is_none()),
    }
}

/// The trees directly in each tree of `item`, by index.
fn children(item: &UseItem<'_>) -> HashMap<usize, Vec<usize>> {
    let mut children: HashMap<usize, Vec<usize>> = HashMap::default();
    for (j, tree) in item.trees.iter().enumerate() {
        if let Some(parent) = tree.parent {
            children.entry(parent as usize).or_default().push(j);
        }
    }
    children
}

/// What rustfix removes of a `use` tree: rustc's `UnusedSpanResult`.
enum Calc {
    Used,
    Unused { spans: Vec<TextRange>, remove: TextRange },
    Partial { spans: Vec<TextRange>, remove: Vec<TextRange> },
}

/// rustc's `calc_unused_spans` of the tree `j` of `item`, for the `unused_imports` lint of an
/// item with an unused tree.
fn calc_unused_spans(
    item: &UseItem<'_>,
    children: &HashMap<usize, Vec<usize>>,
    unused: &Trees,
    j: usize,
    item_span: TextRange,
) -> Calc {
    let tree = &item.trees[j];
    let span = tree.node.syntax().text_range();
    let full_span = if j == 0 { item_span } else { span };
    match tree.kind {
        TreeKind::Simple | TreeKind::Glob => {
            // alloc: the spans of a lint that fires
            if unused.contains(j) { Calc::Unused { spans: vec![span], remove: full_span } } else { Calc::Used }
        }
        TreeKind::List => {
            let nested = children.get(&j).map(Vec::as_slice).unwrap_or(&[]);
            if nested.is_empty() {
                // alloc: the spans of a lint that fires
                return Calc::Unused { spans: vec![span], remove: full_span };
            }
            // alloc: the spans of a lint that fires; empty, it allocates nothing
            let mut unused_spans = Vec::new();
            // alloc: the removals of a lint that fires; empty, it allocates nothing
            let mut to_remove: Vec<TextRange> = Vec::new();
            let mut used_children = 0;
            let mut contains_self = false;
            let mut previous_unused = false;
            for (pos, &child) in nested.iter().enumerate() {
                let remove = match calc_unused_spans(item, children, unused, child, item_span) {
                    Calc::Used => {
                        used_children += 1;
                        None
                    }
                    Calc::Unused { mut spans, remove } => {
                        unused_spans.append(&mut spans);
                        Some(remove)
                    }
                    Calc::Partial { mut spans, remove: mut extra } => {
                        used_children += 1;
                        unused_spans.append(&mut spans);
                        to_remove.append(&mut extra);
                        None
                    }
                };
                if let Some(remove) = remove {
                    let child_span = item.trees[child].node.syntax().text_range();
                    let remove_span = if nested.len() == 1 {
                        remove
                    } else if pos == nested.len() - 1 || used_children > 0 {
                        let previous = item.trees[nested[pos - 1]].node.syntax().text_range();
                        TextRange::new(previous.end(), child_span.end())
                    } else {
                        let next = item.trees[nested[pos + 1]].node.syntax().text_range();
                        TextRange::new(child_span.start(), next.start())
                    };
                    match to_remove.pop() {
                        Some(previous) if previous_unused => to_remove.push(previous.cover(remove_span)),
                        Some(previous) => {
                            to_remove.push(previous);
                            to_remove.push(remove_span);
                        }
                        None => to_remove.push(remove_span),
                    }
                }
                let child_tree = &item.trees[child];
                contains_self |= child_tree.kind == TreeKind::Simple
                    && child_tree.node.path().is_some_and(|it| it.syntax().text() == "self")
                    && !unused.contains(child);
                previous_unused = remove.is_some();
            }
            if unused_spans.is_empty() {
                Calc::Used
            } else if used_children == 0 {
                Calc::Unused { spans: unused_spans, remove: full_span }
            } else {
                if used_children == 1
                    && !contains_self
                    && let Some(list) = tree.node.use_tree_list()
                {
                    let list = list.syntax().text_range();
                    let first = item.trees[nested[0]].node.syntax().text_range();
                    let last = item.trees[nested[nested.len() - 1]].node.syntax().text_range();
                    to_remove.push(TextRange::new(list.start(), first.start()));
                    to_remove.push(TextRange::new(last.end(), list.end()));
                }
                Calc::Partial { spans: unused_spans, remove: to_remove }
            }
        }
    }
}

/// rustc's `redundant_imports` of the single import `tree`: each namespace it imports in
/// resolves to the same without it.
fn redundant(cx: &Cx<'_, '_>, uses: &Uses<'_, '_>, item: &UseItem<'_>, tree: &Tree<'_>, binding: Binding) -> Option<Found> {
    if tree.self_import
        || tree.underscore
        || item.node.file_id.is_macro()
        || uses.used.get(&binding) == Some(&Used::Other)
        || uses.exported.contains(&binding)
    {
        return None;
    }
    let name = tree.name.as_ref()?;
    let source = tree.node.path()?.segment()?.name_ref()?;
    if let Some(rename) = tree.node.rename()
        && rename.name().is_none_or(|it| it.text() != source.text())
    {
        return None;
    }
    let mut ranges: SmallVec<[(Option<InFile<TextRange>>, bool); 3]> = SmallVec::new();
    for ns in Ns::ALL {
        let Some(mine) = entry(&tree.imported, ns) else { continue };
        if ns == Ns::Macros {
            return None;
        }
        let (def, bound) = without(cx, uses, item, binding, name, ns)?;
        if def != mine.def {
            return None;
        }
        let (range, import) = match bound {
            Bound::Glob(range) => (Some(range), true),
            Bound::Entry(found) => {
                let (range, import) = binding_range(cx, uses, found)?;
                (Some(range), import)
            }
            Bound::ExternCrate(krate) => (Some(krate.node.map(|it| item_range_view(it.syntax()))), true),
            Bound::Prelude => (None, false),
        };
        ranges.push((range, import));
    }
    if ranges.is_empty() {
        return None;
    }
    let mut found: SmallVec<[(Option<Span>, bool); 3]> =
        // alloc: the labels of a lint that fires, inline for the three namespaces
        ranges.into_iter().map(|(range, import)| (range.map(|it| cx.span(it)), import)).collect();
    found.sort_by_key(|(span, import)| (span.as_ref().map(|it| (it.lo, it.hi)), *import));
    found.dedup();
    // alloc: the lint fires; the name goes into its message, labels and fact
    let text = source.text().to_string();
    let span = cx.span(InFile::new(item.node.file_id, tree.node.syntax().text_range()));
    // clone: alloc: the message of a lint that fires, whose span its labels may share
    let mut diag = Diag::lint("redundant_imports", format!("the item `{text}` is imported redundantly"), span.clone());
    for (other, import) in found {
        let label = match (other.is_some(), import) {
            // alloc: a label of a lint that fires
            (true, true) => format!("the item `{text}` is already imported here"),
            // alloc: a label of a lint that fires
            (true, false) => format!("the item `{text}` is already defined here"),
            // alloc: a label of a lint that fires
            (false, true) => format!("the item `{text}` is already imported by the extern prelude"),
            // alloc: a label of a lint that fires
            (false, false) => format!("the item `{text}` is already defined by the extern prelude"),
        };
        // clone: a label of a lint that fires, at the import when nothing else is placed
        diag.span_label(other.unwrap_or_else(|| span.clone()), label);
    }
    // alloc: the fact of a lint that fires
    diag.fact(cx.resolves_fact(item.module.into(), &text), format!("what `{text}` is in scope as"));
    let at = InFile::new(item.node.file_id, ast::ptr(tree.node.syntax()));
    Some(Found::lint(diag, Phase::Resolve, at))
}

/// Where the binding [`without`] finds is: placed only once its definition is known to be
/// the import's, since placing an item reads its source.
enum Bound<'u, 'db> {
    /// A glob import's, at its tree.
    Glob(InFile<TextRange>),
    /// One of a module around the item; [`binding_range`] places it.
    Entry(Entry),
    /// The crate root's `extern crate` item.
    ExternCrate(&'u ExternCrate<'db>),
    /// The extern prelude's or the builtin types'.
    Prelude,
}

/// What `name` resolves to in `ns` where `item` is, were `binding` not there: rustc's
/// `resolve_ident_in_scope_set` ignoring the import. The definition and where the binding
/// found is; `None` when it is not found, or not where this can tell.
fn without<'u, 'db>(
    cx: &Cx<'_, '_>,
    uses: &'u Uses<'_, 'db>,
    item: &UseItem<'_>,
    binding: Binding,
    name: &Name,
    ns: Ns,
) -> Option<(ModuleDefId, Bound<'u, 'db>)> {
    let db = cx.db;
    let in_glob = |from: ModuleDefId| match from {
        ModuleDefId::ModuleId(from) => entry(&from.def_map(db).module(db, from).name_in_scope(db, name), ns),
        _ => None,
    };
    let mut at = item.module;
    loop {
        if at == item.module {
            let mut globs = uses
                .globs_of(at)
                .filter(|(_, tree)| tree.binding != Some(binding))
                .filter_map(|(it, tree)| {
                    let found = in_glob(tree.source?)?;
                    found.vis.is_visible_from(db, at).then_some((it, tree, found))
                });
            if let Some((glob_item, glob, found)) = globs.next() {
                if globs.any(|(_, _, it)| it.def != found.def) {
                    return None;
                }
                return Some((found.def, Bound::Glob(InFile::new(glob_item.node.file_id, glob.node.syntax().text_range()))));
            }
        } else if let Some(found) = entry(&at.def_map(db).module(db, at).name_in_scope(db, name), ns) {
            if let Some(Binding::Glob(_)) = found.import
                && uses
                    .globs_of(at)
                    .filter_map(|(_, tree)| in_glob(tree.source?))
                    .any(|it| it.vis.is_visible_from(db, at) && it.def != found.def)
            {
                return None;
            }
            return Some((found.def, Bound::Entry(found)));
        }
        if !at.is_block_module(db) {
            break;
        }
        at = at.def_map(db).containing_module(db, at)?;
    }
    if ns != Ns::Types {
        return None;
    }
    let root: ModuleId = cx.krate.root_module(db).into();
    if let Some(krate) = uses.crates.iter().find(|it| it.module == root && it.name == *name) {
        let loaded = krate.krate?;
        return Some((ModuleDefId::ModuleId(loaded.root_module(db).into()), Bound::ExternCrate(krate)));
    }
    if uses.extern_prelude.contains(name) {
        let dep = cx.krate.dependencies_iter(db).find(|it| it.name == *name)?;
        return Some((ModuleDefId::ModuleId(dep.krate.root_module(db).into()), Bound::Prelude));
    }
    let builtin = hir_def::builtin_type::BuiltinType::by_name(name)?;
    Some((ModuleDefId::BuiltinType(builtin), Bound::Prelude))
}

/// Where the binding `found` is, and whether it is an import's.
fn binding_range(cx: &Cx<'_, '_>, uses: &Uses<'_, '_>, found: Entry) -> Option<(InFile<TextRange>, bool)> {
    match found.import {
        Some(binding @ (Binding::Import(_) | Binding::Glob(_))) => {
            let (item, tree) = uses.tree(binding)?;
            Some((InFile::new(item.node.file_id, tree.node.syntax().text_range()), true))
        }
        Some(Binding::ExternCrate(id)) => {
            let krate = uses.extern_crate_of(id)?;
            Some((krate.node.map(|it| item_range_view(it.syntax())), true))
        }
        None => {
            let node = def_item(cx, found.def)?;
            Some((node.map(item_range_view), false))
        }
    }
}

/// The item defining `def`, when it is one a block can have.
fn def_item<'db>(cx: &Cx<'_, 'db>, def: ModuleDefId) -> Option<InFile<Node<'db>>> {
    use hir::view::HasSourceView as _;
    fn node<'db, N: ast::AstNode<'db>>(it: Option<InFile<N>>) -> Option<InFile<Node<'db>>> {
        it.map(|it| it.map(|it| it.syntax()))
    }
    let db = cx.db;
    match hir::ModuleDef::from(def) {
        hir::ModuleDef::Module(it) => node(it.declaration_source_view(db)),
        hir::ModuleDef::Function(it) => node(it.source_view(db)),
        hir::ModuleDef::Adt(hir::Adt::Struct(it)) => node(it.source_view(db)),
        hir::ModuleDef::Adt(hir::Adt::Union(it)) => node(it.source_view(db)),
        hir::ModuleDef::Adt(hir::Adt::Enum(it)) => node(it.source_view(db)),
        hir::ModuleDef::Const(it) => node(it.source_view(db)),
        hir::ModuleDef::Static(it) => node(it.source_view(db)),
        hir::ModuleDef::Trait(it) => node(it.source_view(db)),
        hir::ModuleDef::TypeAlias(it) => node(it.source_view(db)),
        hir::ModuleDef::Macro(it) => node(it.source_view(db)),
        hir::ModuleDef::EnumVariant(_) | hir::ModuleDef::BuiltinType(_) => None,
    }
}

/// rustc's `span_with_attributes` of an item: from its first attribute or doc comment.
fn with_attrs(node: Node<'_>) -> TextRange {
    let start = node
        .children_with_tokens()
        .find(|it| match it {
            NodeOrToken::Node(_) => true,
            NodeOrToken::Token(token) => !token.kind().is_trivia(),
        })
        .map_or(node.text_range().start(), |it| it.text_range().start());
    TextRange::new(start, node.text_range().end())
}

/// Where the help on a test module points, when the `use` item's nearest module that is not a
/// block is named as a test module's: rustc's `guess_head_span` of the module.
fn test_module(cx: &Cx<'_, '_>, module: ModuleId) -> Option<Span> {
    let db = cx.db;
    let mut module = module;
    while module.is_block_module(db) {
        module = module.def_map(db).containing_module(db, module)?;
    }
    let path = module_path(cx, module)?;
    let test = path == "test"
        || path == "tests"
        || path.starts_with("test_")
        || path.starts_with("tests_")
        || path.ends_with("_test")
        || path.ends_with("_tests");
    if !test {
        return None;
    }
    let declared = hir::Module::from(module).declaration_source_view(db)?;
    let span = cx.span(declared.map(|it| item_range_view(it.syntax())));
    let snippet = cx.snippet(&span)?;
    let head = snippet.split('{').next().unwrap_or("").trim_end();
    if head.is_empty() || head.contains('\n') {
        return Some(span);
    }
    Some(span.with_hi(span.lo + head.len() as u32))
}

/// rustc's `module_to_string`: the names from the root to `module`, `None` for the root. Only
/// the `unused_imports` lint that fires asks, for its help.
fn module_path(cx: &Cx<'_, '_>, module: ModuleId) -> Option<String> {
    let db = cx.db;
    let mut names: SmallVec<[Name; 8]> = SmallVec::new();
    let mut at = Some(module);
    while let Some(it) = at {
        if let Some(name) = it.name(db) {
            names.push(name);
        }
        at = it.def_map(db).module(db, it).parent;
    }
    if names.is_empty() {
        return None;
    }
    // alloc: the help of a lint that fires
    let mut path = String::with_capacity(32);
    for (i, name) in names.iter().rev().enumerate() {
        if i > 0 {
            path.push_str("::");
        }
        path.push_str(name.as_str());
    }
    Some(path)
}

/// Whether `module`, or a module it is in, is `#![no_implicit_prelude]`.
fn no_implicit_prelude(cx: &Cx<'_, '_>, module: ModuleId) -> bool {
    let db = cx.db;
    let mut at = Some(module);
    while let Some(id) = at {
        let module = hir::Module::from(id);
        let source = module.definition_source_view(db).value;
        let inner = match source {
            hir::view::ModuleSourceView::SourceFile(it) => ast::attrs_including_inner(&it).any(is_no_implicit_prelude),
            hir::view::ModuleSourceView::Module(it) => ast::attrs_including_inner(&it).any(is_no_implicit_prelude),
            hir::view::ModuleSourceView::BlockExpr(_) => false,
        };
        if inner
            || module
                .declaration_source_view(db)
                .is_some_and(|declared| ast::HasAttrs::attrs(&declared.value).any(is_no_implicit_prelude))
        {
            return true;
        }
        at = id.def_map(db).containing_module(db, id);
    }
    false
}

fn is_no_implicit_prelude(attr: ast::Attr<'_>) -> bool {
    attr.path().and_then(|it| it.as_single_name_ref()).is_some_and(|it| it.text() == "no_implicit_prelude")
}

/// Whether the crate `krate` exports a macro named `name` at its root.
fn exports_macro(cx: &Cx<'_, '_>, krate: hir::Crate, name: &Name) -> bool {
    let db = cx.db;
    let root: ModuleId = krate.root_module(db).into();
    root.def_map(db).module(db, root).scope(db).macros().any(|(it, _)| it == name)
}

/// Whether rustc may find an `extern crate` of `krate` unused: it provides none of what
/// rustc keeps such an item for (the compiler builtins, a panic runtime, a global allocator, a
/// panic handler, an externally implementable item). `false` where that can't be told.
fn may_be_unused(cx: &Cx<'_, '_>, krate: hir::Crate) -> bool {
    let db = cx.db;
    match &krate.base().data(db).origin {
        CrateOrigin::Lang(LangCrateOrigin::Core | LangCrateOrigin::Alloc | LangCrateOrigin::ProcMacro | LangCrateOrigin::Test) => {
            return true;
        }
        CrateOrigin::Lang(_) => return false,
        _ => {}
    }
    for module in krate.modules_iter(db) {
        let Some(file) = module.as_source_file_id(db) else { continue };
        if mentions_kept(cx.file_text(file.file_id(db)).as_bytes()) {
            return false;
        }
    }
    true
}

const KEPT: [&str; 5] = ["compiler_builtins", "panic_runtime", "global_allocator", "panic_handler", "eii"];

/// Whether `text` has one of [`KEPT`] as a word, or a word starting `eii_`. One SIMD substring
/// search per name (memchr's `memmem`, chosen for the machine at run time) over the source as
/// it is in the database; no copy, no allocation.
fn mentions_kept(text: &[u8]) -> bool {
    static FINDERS: LazyLock<[memmem::Finder<'static>; 5]> = LazyLock::new(|| KEPT.map(memmem::Finder::new));
    let ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    FINDERS.iter().zip(KEPT).any(|(finder, name)| {
        finder.find_iter(text).any(|at| {
            let end = at + name.len();
            let starts = at == 0 || !ident(text[at - 1]);
            let ends = text.get(end).is_none_or(|&b| !ident(b) || (name == "eii" && b == b'_'));
            starts && ends
        })
    })
}
