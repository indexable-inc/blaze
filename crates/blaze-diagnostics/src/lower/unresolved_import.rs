//! An import that does not resolve. hir says which use tree; why it fails is found again here
//! as rustc's resolver finds it, walking the import's path through hir's resolution of each
//! prefix: a segment before the last of the module path not found is E0433 on its own; the last
//! one not found, or the name not in the module found, is E0432 "unresolved import `a::b`",
//! one diagnostic for the failed imports of one `use` item, each with its label ("no `b` in
//! `a`", "use of unresolved module or unlinked crate `a`", "could not find `x` in `y`", ..)
//! and suggestion ("a similar path exists", "a similar name exists in the module", the
//! `extern crate` to add).
//!
//! Follows rustc's `compiler/rustc_resolve/src/imports.rs` (`finalize_imports`,
//! `finalize_import`, `import_path_to_string`), `diagnostics/impls.rs`
//! (`throw_unresolved_import_error`, `report_path_resolution_error`, `make_path_suggestion`,
//! `find_similarly_named_module_or_crate`, `into_struct_error` of `FailedToResolve`) and
//! `lib.rs` (`names_to_string`, `module_to_string`) (MIT or Apache-2.0).

use std::cell::OnceCell;

use blaze_items::Fact;
use hir::{HasVisibility as _, InFile, ItemInNs, ModPath, ModuleDef, Name, PathKind, StructKind};
use span::Edition;
use stdx::edit_distance::find_best_match_for_name;
use syntax::{
    NodeOrToken, SyntaxKind, TextRange, TextSize,
    view::{self as ast, AstNode as _},
};

use crate::{
    cx::Cx,
    lower::{Found, Phase},
    model::{Applicability, Diag, Span},
};

/// rustc's name of the crate root segment, `::` or implicit in a 2015 import.
pub(crate) const ROOT: &str = "{{root}}";

/// rustc's upper limit on the labels of one diagnostic of unresolved imports.
const MAX_LABEL_COUNT: usize = 10;

/// A segment of a path as rustc's resolver has it, with where it is written.
#[derive(Clone, Debug)]
pub(crate) struct Segment {
    pub(crate) name: String,
    pub(crate) range: TextRange,
}

/// An import as rustc's resolver has it: the module it is from and the name it takes.
#[derive(Clone, Debug)]
struct Import {
    /// rustc's `module_path`, a leading `{{root}}` included.
    module_path: Vec<Segment>,
    /// The name imported; none for a glob.
    source: Option<Segment>,
    /// rustc's `import.span`: the use tree, its rename included.
    range: TextRange,
}

/// What a module path resolved to: a module, an enum or a trait, or 2018's `::`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scope {
    Def(ModuleDef),
    ExternPrelude,
}

/// A suggestion of rustc's: its parts, or none for one written as a help.
struct Suggestion {
    parts: Vec<(Span, String)>,
    message: String,
}

/// Why an import fails, as rustc reports it.
enum Failure {
    /// E0433 of a segment before the last of the module path.
    Path { at: TextRange, error: PathError },
    /// The import's part of an E0432.
    Import(ImportError),
}

/// rustc's `UnresolvedImportError`.
struct ImportError {
    span: TextRange,
    label_at: TextRange,
    path: String,
    label: Option<String>,
    note: Option<String>,
    suggestion: Option<Suggestion>,
    fact: Option<(Fact, String)>,
}

pub(crate) fn lower<'db>(cx: &Cx<'_, 'db>, d: &hir::UnresolvedImport) -> Vec<Found> {
    let tree = cx.node_view(d.decl);
    let file = tree.file_id;
    let r = Resolver::new(cx, file, tree.value.syntax());
    let mine = r.failure(&r.import(tree.value));
    let error = match mine {
        // alloc: the error's parts, for an error hir found
        Some(Failure::Path { at, error }) => return vec![Found::new(path_diag(&r, at, error), Phase::Resolve)],
        Some(Failure::Import(error)) => error,
        None => {
            // hir finds no way to it where this walk finds one: still the error, as rustc's
            // message says it, with nothing it cannot say.
            let import = r.import(tree.value);
            let path = import_path(&import, None);
            // alloc: the error's text, for an error hir found
            let diag = Diag::error(Some("E0432"), format!("unresolved import `{path}`"), r.span(import.range));
            // alloc: the error's parts, for an error hir found
            return vec![Found::new(diag, Phase::Resolve)];
        }
    };
    // rustc gives the failed imports of one `use` item in one diagnostic, at the first: the
    // others give nothing of their own.
    let leaves = tree.value.syntax().ancestors().find_map(ast::Use::cast).map_or_else(Vec::new, leaves);
    let at = leaves.iter().position(|it| it.syntax() == tree.value.syntax()).unwrap_or(leaves.len());
    let import_error = |leaf: &ast::UseTree<'db>| {
        let r = Resolver::new(cx, file, leaf.syntax());
        match r.failure(&r.import(*leaf)) {
            Some(Failure::Import(error)) => Some(error),
            _ => None,
        }
    };
    if leaves[..at].iter().any(|it| import_error(it).is_some()) {
        // alloc: empty, allocates nothing
        return Vec::new();
    }
    // alloc: the error's parts, for an error hir found
    let mut group = vec![error];
    group.extend(leaves.iter().skip(at + 1).filter_map(import_error));
    // alloc: the error's parts, for an error hir found
    vec![Found::new(unresolved_imports(&r, group), Phase::Resolve)]
}

/// rustc's `throw_unresolved_import_error` of the errors of one `use` item.
fn unresolved_imports(r: &Resolver<'_, '_, '_>, errors: Vec<ImportError>) -> Diag {
    // alloc: the error's text, for an error hir found
    let paths: Vec<String> = errors.iter().map(|it| format!("`{}`", it.path)).collect();
    let plural = if paths.len() == 1 { "" } else { "s" };
    // alloc: the error's text, for an error hir found
    let message = format!("unresolved import{plural} {}", paths.join(", "));
    let mut spans = errors.iter().map(|it| r.span(it.span));
    let mut diag = Diag::error(Some("E0432"), message, spans.next().expect("one error at least"));
    spans.for_each(|it| {
        diag.primary(it);
    });
    // clone: on the error path, for an error hir found
    if let Some(note) = errors.last().and_then(|it| it.note.clone()) {
        diag.note(note);
    }
    for error in errors.into_iter().take(MAX_LABEL_COUNT) {
        if let Some(label) = error.label {
            diag.span_label(r.span(error.label_at), label);
        }
        if let Some((fact, what)) = error.fact {
            diag.fact(fact, what);
        }
        if let Some(suggestion) = error.suggestion {
            suggest(&mut diag, suggestion);
        }
    }
    diag
}

/// E0433 of a path's segment at `at`, as rustc's `FailedToResolve` gives it.
fn path_diag(r: &Resolver<'_, '_, '_>, at: TextRange, error: PathError) -> Diag {
    let at = r.span(at);
    // clone: on the error path, for an error hir found
    let mut diag = Diag::error(Some("E0433"), error.message, at.clone());
    diag.span_label(at, error.label);
    if let Some(suggestion) = error.suggestion {
        suggest(&mut diag, suggestion);
    }
    if let Some((fact, what)) = error.fact {
        diag.fact(fact, what);
    }
    diag
}

/// rustc's error of a macro path of more than one segment that does not resolve
/// (`finalize_macro_resolutions`): E0433 at the segment not found, or at the whole path when it
/// goes into an enum or a trait. `expected` is the macro's kind as rustc names it, with its
/// article (`("a", "macro")`); `bang` whether it is called with `!`.
pub(super) fn macro_path_error<'db>(
    cx: &Cx<'_, 'db>,
    file: hir::HirFileId,
    node: ast::Node<'db>,
    path: &[Segment],
    range: TextRange,
    expected: (&str, &str),
    bang: bool,
) -> Diag {
    let r = Resolver::new(cx, file, node);
    let (last, module_path) = path.split_last().expect("a path of one segment at least");
    let mut module = None;
    for (at, segment) in module_path.iter().enumerate() {
        match r.module(&module_path[..=at]) {
            Some(Scope::Def(def @ (ModuleDef::Adt(_) | ModuleDef::Trait(_)))) => {
                let (found_an, found_descr) = match def {
                    ModuleDef::Trait(_) => ("a", "trait"),
                    _ => ("an", "enum"),
                };
                let unresolved = path.len() - at - 1;
                let scope = match path[..unresolved].last() {
                    // alloc: the error's text, for an error hir found
                    Some(prev) => format!("{found_descr} `{}`", prev.name),
                    // alloc: the error's text, for an error hir found
                    None => found_descr.to_owned(),
                };
                let (expected_an, expected_descr) = expected;
                // alloc: the error's text, for an error hir found
                let message = format!("cannot find {expected_descr} `{}` in {scope}", path[unresolved].name);
                let span = r.span(range);
                // clone: on the error path, for an error hir found
                let mut diag = Diag::error(Some("E0433"), message, span.clone());
                // alloc: the error's text, for an error hir found
                diag.span_label(span, format!("{expected_an} {expected_descr} can't exist within {found_an} {found_descr}"));
                return diag;
            }
            Some(found) => module = Some(found),
            None => return path_diag(&r, segment.range, r.path_error(path, at, module, true)),
        }
    }
    let mut diag = path_diag(&r, last.range, r.path_error(path, path.len() - 1, module, true));
    if bang && let Some(def) = r.value(Some(module_path), &last.name) {
        let bang_at = TextRange::at(last.range.end(), TextSize::of('!'));
        let names = names_to_string(path.iter().map(|it| it.name.as_str()));
        diag.multipart_suggestion(
            // alloc: the error's text, for an error hir found
            format!("{names} is not a macro, but a {}, try to remove `!`", value_descr(cx, def)),
            // alloc: the error's text, for an error hir found
            vec![(r.span(bang_at), String::new())],
            Applicability::MaybeIncorrect,
        );
    }
    diag
}

/// The segments rustc resolves for the path of a `pub(in path)` or `pub(super)`
/// (`try_resolve_visibility`): `{{root}}` first in Rust 2015 when it starts with a name.
pub(super) fn visibility_segments(cx: &Cx<'_, '_>, path: ast::Path<'_>) -> Vec<Segment> {
    let mut segments = path_segments(path);
    if cx.edition == Edition::Edition2015
        && let Some(first) = segments.first()
        && !is_path_segment_keyword(&first.name)
    {
        let range = TextRange::empty(first.range.start());
        // alloc: the error's text, for an error hir found
        segments.insert(0, Segment { name: ROOT.to_owned(), range });
    }
    segments
}

/// rustc's E0433 of a `pub(in path)` or `pub(super)` that names nothing
/// (`try_resolve_visibility`'s `FailedToResolve`): at the first segment that names no module.
pub(super) fn visibility_path_error<'db>(cx: &Cx<'_, 'db>, file: hir::HirFileId, path: ast::Path<'db>) -> Diag {
    let r = Resolver::new(cx, file, path.syntax());
    let segments = visibility_segments(cx, path);
    let mut module = None;
    for (at, segment) in segments.iter().enumerate() {
        match r.module(&segments[..=at]) {
            Some(found) => module = Some(found),
            None => return path_diag(&r, segment.range, r.path_error(&segments, at, module, false)),
        }
    }
    // hir finds no module where this walk finds one: still the error, with what it can say.
    let names = names_to_string(segments.iter().map(|it| it.name.as_str()));
    // alloc: the error's text, for an error hir found
    Diag::error(Some("E0433"), format!("failed to resolve: `{names}`"), r.span(path.syntax().text_range()))
}

fn suggest(diag: &mut Diag, suggestion: Suggestion) {
    match suggestion.parts.is_empty() {
        true => diag.help(suggestion.message),
        false => diag.multipart_suggestion(suggestion.message, suggestion.parts, Applicability::MaybeIncorrect),
    };
}

/// The use trees of `item` that import one thing each, in order.
fn leaves(item: ast::Use<'_>) -> Vec<ast::UseTree<'_>> {
    fn walk<'t>(tree: ast::UseTree<'t>, out: &mut Vec<ast::UseTree<'t>>) {
        match tree.use_tree_list() {
            Some(list) => list.use_trees().for_each(|it| walk(it, out)),
            None => out.push(tree),
        }
    }
    // alloc: the error's parts, for an error hir found
    let mut out = Vec::new();
    if let Some(tree) = item.use_tree() {
        walk(tree, &mut out);
    }
    out
}

/// rustc's `import_path_to_string`: the module path up to the segment that failed, or with the
/// name imported.
fn import_path(import: &Import, failed: Option<usize>) -> String {
    let global = import.module_path.first().is_some_and(|it| it.name == ROOT);
    let names = &import.module_path[usize::from(global)..];
    // alloc: the error's parts, for an error hir found
    let names: Vec<&str> = names.iter().map(|it| it.name.as_str()).collect();
    if let Some(at) = failed {
        let at = at - usize::from(global);
        return names[..=at].join("::");
    }
    let source = import.source.as_ref().map_or("*", |it| it.name.as_str());
    match names.is_empty() {
        // alloc: the error's text, for an error hir found
        true => source.to_owned(),
        // alloc: the error's text, for an error hir found
        false => format!("{}::{source}", names.join("::")),
    }
}

/// rustc's `names_to_string`: the names joined by `::`, `{{root}}` written as nothing.
pub(super) fn names_to_string<'a>(names: impl IntoIterator<Item = &'a str>) -> String {
    // alloc: the error's text, for an error hir found
    let mut out = String::new();
    for (i, name) in names.into_iter().enumerate().filter(|(_, it)| *it != ROOT) {
        if i > 0 {
            out.push_str("::");
        }
        out.push_str(name);
    }
    out
}

fn is_path_segment_keyword(name: &str) -> bool {
    matches!(name, ROOT | "crate" | "self" | "super" | "Self" | "$crate")
}

/// Resolution of the paths of the import at `node` through hir's resolver.
struct Resolver<'c, 'a, 'db> {
    cx: &'c Cx<'a, 'db>,
    file: hir::HirFileId,
    node: ast::Node<'db>,
    /// [`Resolver::current`], found once: each path the import resolves asks for it.
    current: OnceCell<Option<hir::Module>>,
}

impl<'c, 'a, 'db> Resolver<'c, 'a, 'db> {
    fn new(cx: &'c Cx<'a, 'db>, file: hir::HirFileId, node: ast::Node<'db>) -> Self {
        Resolver { cx, file, node, current: OnceCell::new() }
    }

    fn span(&self, range: TextRange) -> Span {
        self.cx.span(InFile::new(self.file, range))
    }

    fn edition(&self) -> Edition {
        self.cx.edition
    }

    /// The module the import is in: `self` of its paths.
    fn current(&self) -> Option<hir::Module> {
        *self.current.get_or_init(|| {
            Some(self.cx.vsema.scope(self.node)?.module().nearest_non_block_module(self.cx.db))
        })
    }

    /// rustc's import of the leaf use tree `tree`: the paths of the trees it is nested in and
    /// its own, `{{root}}` before a 2015 path that does not start with a keyword.
    fn import(&self, tree: ast::UseTree<'_>) -> Import {
        // alloc: the error's parts, for an error hir found
        let mut segments: Vec<Segment> = Vec::new();
        let nested = nested_segments(tree, &mut segments) > 1;
        if self.edition() == Edition::Edition2015
            && let Some(first) = segments.first()
            && !is_path_segment_keyword(&first.name)
        {
            let range = TextRange::empty(first.range.start());
            // alloc: the error's text, for an error hir found
            segments.insert(0, Segment { name: ROOT.to_owned(), range });
        }
        let range = tree.syntax().text_range();
        if tree.star_token().is_some() {
            return Import { module_path: segments, source: None, range };
        }
        // `a::{self}` imports `a`.
        if segments.last().is_some_and(|it| it.name == "self") && nested {
            segments.pop();
        }
        let source = segments.pop();
        Import { module_path: segments, source, range }
    }

    /// What `path` (rustc's segments) names as a module, if it names one.
    fn module(&self, path: &[Segment]) -> Option<Scope> {
        let db = self.cx.db;
        let (kind, rest) = self.kind_of(path);
        if rest.is_empty() {
            let current = self.current()?;
            return match kind {
                PathKind::Abs if self.edition() > Edition::Edition2015 => Some(Scope::ExternPrelude),
                PathKind::Abs | PathKind::Crate => Some(Scope::Def(ModuleDef::Module(current.crate_root(db)))),
                PathKind::Super(n) => {
                    let mut module = current;
                    for _ in 0..n {
                        module = module.parent(db)?;
                    }
                    Some(Scope::Def(ModuleDef::Module(module)))
                }
                _ => None,
            };
        }
        let path = ModPath::from_segments(kind, rest.iter().map(|it| Name::new_root(&it.name)));
        let mut items = self.cx.vsema.resolve_mod_path(self.node, &path)?;
        items.find_map(|it| match it {
            ItemInNs::Types(def @ (ModuleDef::Module(_) | ModuleDef::Trait(_))) => Some(Scope::Def(def)),
            ItemInNs::Types(def @ ModuleDef::Adt(hir::Adt::Enum(_))) => Some(Scope::Def(def)),
            _ => None,
        })
    }

    /// What `name` names as a value in `scope`, or in the import's scope when there is none.
    fn value(&self, scope: Option<&[Segment]>, name: &str) -> Option<ModuleDef> {
        let mut path: Vec<Segment> = scope.map_or_else(Vec::new, <[Segment]>::to_vec);
        // alloc: the error's text, for an error hir found
        path.push(Segment { name: name.to_owned(), range: TextRange::default() });
        let (kind, rest) = self.kind_of(&path);
        let path = ModPath::from_segments(kind, rest.iter().map(|it| Name::new_root(&it.name)));
        let mut items = self.cx.vsema.resolve_mod_path(self.node, &path)?;
        items.find_map(|it| match it {
            ItemInNs::Values(def) => Some(def),
            _ => None,
        })
    }

    /// Whether the import's name is found in the module `scope`, in any namespace.
    fn found_in(&self, import: &Import, source: &Segment) -> bool {
        // clone: on the error path, for an error hir found
        let mut path = import.module_path.clone();
        // clone: on the error path, for an error hir found
        path.push(source.clone());
        let (kind, rest) = self.kind_of(&path);
        if rest.is_empty() {
            return true;
        }
        let path = ModPath::from_segments(kind, rest.iter().map(|it| Name::new_root(&it.name)));
        self.cx.vsema.resolve_mod_path(self.node, &path).is_some_and(|mut it| it.next().is_some())
    }

    /// The leading keywords of `path` as hir's path kind, and the names after them.
    fn kind_of<'p>(&self, path: &'p [Segment]) -> (PathKind, &'p [Segment]) {
        let mut kind = PathKind::Plain;
        let mut rest = path;
        while let [first, tail @ ..] = rest {
            kind = match (first.name.as_str(), kind) {
                (ROOT, PathKind::Plain) if rest.len() == path.len() => PathKind::Abs,
                ("crate", PathKind::Plain) if rest.len() == path.len() => PathKind::Crate,
                ("self", PathKind::Plain) if rest.len() == path.len() => PathKind::Super(0),
                ("super", PathKind::Plain) if rest.len() == path.len() => PathKind::Super(1),
                ("super", PathKind::Super(n)) => PathKind::Super(n + 1),
                _ => break,
            };
            rest = tail;
        }
        (kind, rest)
    }

    /// Why `import` fails as rustc's `finalize_import` finds it, or none when it resolves.
    fn failure(&self, import: &Import) -> Option<Failure> {
        let path = &import.module_path;
        let mut module = None;
        for (at, segment) in path.iter().enumerate() {
            match self.module(&path[..=at]) {
                Some(found) => module = Some(found),
                None => {
                    let failed = self.path_error(path, at, module, false);
                    if at + 1 < path.len() {
                        return Some(Failure::Path { at: segment.range, error: failed });
                    }
                    let (label, note, suggestion) = match self.similar_path(path) {
                        Some((suggested, note)) => {
                            let suggested = names_to_string(suggested.iter().map(|it| it.name.as_str()));
                            // alloc: the error's parts, for an error hir found
                            let parts = vec![(self.span(segment.range), suggested)];
                            // alloc: the error's text, for an error hir found
                            (None, note, Some(Suggestion { parts, message: "a similar path exists".to_owned() }))
                        }
                        None => (Some(failed.label), None, failed.suggestion),
                    };
                    return Some(Failure::Import(ImportError {
                        span: segment.range,
                        label_at: segment.range,
                        path: import_path(import, Some(at)),
                        label,
                        note,
                        suggestion,
                        fact: failed.fact,
                    }));
                }
            }
        }
        let source = import.source.as_ref()?;
        if self.found_in(import, source) {
            return None;
        }
        let scope = match module {
            Some(scope) => scope,
            None if path.is_empty() => Scope::ExternPrelude,
            None => return None,
        };
        let label = match scope {
            Scope::Def(def) => match module_to_string(self.cx, def) {
                // alloc: the error's text, for an error hir found
                Some(module) => format!("no `{}` in `{module}`", source.name),
                // alloc: the error's text, for an error hir found
                None => format!("no `{}` in the root", source.name),
            },
            Scope::ExternPrelude if !is_path_segment_keyword(&source.name) => {
                // alloc: the error's text, for an error hir found
                format!("no external crate `{}`", source.name)
            }
            // alloc: the error's text, for an error hir found
            Scope::ExternPrelude => format!("no `{}` in the root", source.name),
        };
        let names = self.names_in(scope, &source.name);
        // alloc: the error's parts, for an error hir found
        let names: Vec<&str> = names.iter().map(Name::as_str).collect();
        let suggestion = find_best_match_for_name(&names, &source.name, None).copied().map(|it| Suggestion {
            // alloc: the error's text, for an error hir found
            parts: vec![(self.span(source.range), it.to_owned())],
            // alloc: the error's text, for an error hir found
            message: "a similar name exists in the module".to_owned(),
        });
        let fact = match scope {
            Scope::Def(ModuleDef::Module(m)) => Some((
                self.cx.resolves_fact(m, &source.name),
                // alloc: the error's text, for an error hir found
                format!("what `{}` names in `{}`", source.name, module_to_string(self.cx, ModuleDef::Module(m)).unwrap_or_else(|| "crate".to_owned())),
            )),
            _ => None,
        };
        Some(Failure::Import(ImportError {
            span: import.range,
            label_at: source.range,
            path: import_path(import, None),
            label: Some(label),
            note: None,
            suggestion,
            fact,
        }))
    }

    /// The names of `scope` rustc may suggest for `name`: not `name` itself, nor `_`.
    /// Interned names, so the sort and dedup compare symbols, not copied strings.
    fn names_in(&self, scope: Scope, name: &str) -> Vec<Name> {
        let db = self.cx.db;
        let mut names: Vec<Name> = match scope {
            Scope::Def(ModuleDef::Module(m)) => m
                .scope(db, None)
                .into_iter()
                .filter(|(_, def)| !matches!(def, hir::ScopeDef::Unknown))
                .map(|(name, _)| name)
                // alloc: the error's parts, for an error hir found
                .collect(),
            // alloc: the error's parts, for an error hir found
            Scope::Def(ModuleDef::Adt(hir::Adt::Enum(e))) => e.variants_iter(db).map(|it| it.name(db)).collect(),
            // alloc: the error's parts, for an error hir found
            Scope::Def(ModuleDef::Trait(t)) => t.items_iter(db).filter_map(|it| it.name(db)).collect(),
            // alloc: empty, allocates nothing
            _ => Vec::new(),
        };
        names.retain(|it| it.as_str() != name && it.as_str() != "_");
        names.sort_unstable_by(|a, b| a.symbol().cmp_order(b.symbol()));
        names.dedup();
        names
    }

    /// rustc's `make_path_suggestion`: the path with `self`, `crate`, `super` or a crate's name
    /// first, when that names a module, and the note it gives with it.
    fn similar_path(&self, path: &[Segment]) -> Option<(Vec<Segment>, Option<String>)> {
        // alloc: the error's parts, for an error hir found
        let mut path = path.to_vec();
        match path.as_slice() {
            [first, second, ..] if first.name == ROOT && !is_path_segment_keyword(&second.name) => {}
            [first, ..] if self.edition() > Edition::Edition2015 && !is_path_segment_keyword(&first.name) => {
                // alloc: the error's text, for an error hir found
                path.insert(0, Segment { name: String::new(), range: TextRange::default() });
            }
            _ => return None,
        }
        let with = |first: &str| {
            // clone: on the error path, for an error hir found
            let mut path = path.clone();
            // alloc: the error's text, for an error hir found
            path[0].name = first.to_owned();
            path
        };
        for first in ["self", "crate", "super"] {
            let candidate = with(first);
            if self.module(&candidate).is_some() {
                let note = (first == "crate").then(|| {
                    // alloc: the error's note, for an error hir found
                    "`use` statements changed in Rust 2018; read more at <https://doc.rust-lang.org/edition-guide/rust-2018/module-system/path-clarity.html>".to_owned()
                });
                return Some((candidate, note));
            }
        }
        if self.edition() == Edition::Edition2015 {
            return None;
        }
        let mut crates = self.extern_prelude();
        crates.sort_unstable_by(|a, b| b.symbol().cmp_order(a.symbol()));
        crates.into_iter().map(|it| with(it.as_str())).find(|it| self.module(it).is_some()).map(|it| (it, None))
    }

    /// The names of the crate's extern prelude: its dependencies.
    fn extern_prelude(&self) -> Vec<Name> {
        // alloc: the error's parts, for an error hir found
        self.cx.krate.dependencies_iter(self.cx.db).map(|it| it.name).collect()
    }

    /// rustc's `report_path_resolution_error` of the segment `at` of `path`, `module` what the
    /// segments before it named; `macro_last` when the last segment is looked for as a macro
    /// (an import's path is all of modules).
    fn path_error(&self, path: &[Segment], at: usize, module: Option<Scope>, macro_last: bool) -> PathError {
        let cx = self.cx;
        let db = cx.db;
        let ident = &path[at];
        let name = ident.name.as_str();
        let scope = match path[..at].last() {
            // alloc: the error's text, for an error hir found
            Some(prev) if prev.name == ROOT => "the crate root".to_owned(),
            // alloc: the error's text, for an error hir found
            Some(prev) => format!("`{}`", prev.name),
            // alloc: the error's text, for an error hir found
            None => "this scope".to_owned(),
        };
        // alloc: the error's text, for an error hir found
        let message = format!("cannot find `{name}` in {scope}");
        let in_module = match module {
            Some(Scope::Def(ModuleDef::Module(m))) => Some(m),
            _ => self.current(),
        };
        let fact = in_module.map(|m| {
            // alloc: the error's text, for an error hir found
            let of = module_to_string(cx, ModuleDef::Module(m)).map_or_else(|| "the crate root".to_owned(), |it| format!("`{it}`"));
            // alloc: the error's text, for an error hir found
            (cx.resolves_fact(m, name), format!("what `{name}` names in {of}"))
        });
        let error = |message: String, label: String, suggestion: Option<Suggestion>| PathError {
            message,
            label,
            suggestion,
            // clone: on the error path, for an error hir found
            fact: fact.clone(),
        };
        if name == "super" {
            return error(
                // alloc: the error's text, for an error hir found
                "too many leading `super` keywords".to_owned(),
                // alloc: the error's text, for an error hir found
                "there are too many leading `super` keywords".to_owned(),
                None,
            );
        }
        let crate_root = cx.krate.root_module(db);
        if module == Some(Scope::Def(ModuleDef::Module(crate_root))) {
            if let Some(candidate) = self.module_candidate(path, at) {
                // alloc: the error's parts, for an error hir found
                let parts = vec![(self.span(ident.range), candidate)];
                return error(
                    message,
                    // alloc: the error's text, for an error hir found
                    "unresolved import".to_owned(),
                    // alloc: the error's text, for an error hir found
                    Some(Suggestion { parts, message: "a similar path exists".to_owned() }),
                );
            }
            if name == "core" {
                // alloc: the error's text, for an error hir found
                let parts = vec![(self.span(ident.range), "std".to_owned())];
                return error(
                    message,
                    // alloc: the error's text, for an error hir found
                    "you might be missing crate `core`".to_owned(),
                    // alloc: the error's text, for an error hir found
                    Some(Suggestion { parts, message: "try using `std` instead of `core`".to_owned() }),
                );
            }
            if name == "_" {
                return error(
                    // alloc: the error's text, for an error hir found
                    "invalid crate or module name `_`".to_owned(),
                    // alloc: the error's text, for an error hir found
                    "`_` is not a valid crate or module name".to_owned(),
                    None,
                );
            }
            if self.edition() == Edition::Edition2015 {
                // alloc: the error's text, for an error hir found
                let parts = vec![(self.crate_insert_span(), format!("extern crate {name};\n"))];
                return error(
                    // alloc: the error's text, for an error hir found
                    format!("cannot find module or crate `{name}` in {scope}"),
                    // alloc: the error's text, for an error hir found
                    format!("use of unresolved module or unlinked crate `{name}`"),
                    Some(Suggestion { parts, message: missing_crate_2015(cx, name) }),
                );
            }
            // alloc: the error's text, for an error hir found
            return error(message, format!("could not find `{name}` in the crate root"), None);
        }
        if at > 0 {
            let parent = match path[at - 1].name.as_str() {
                // alloc: the error's text, for an error hir found
                ROOT if self.edition() > Edition::Edition2015 => "the list of imported crates".to_owned(),
                // alloc: the error's text, for an error hir found
                ROOT | "crate" => "the crate root".to_owned(),
                // alloc: the error's text, for an error hir found
                parent => format!("`{parent}`"),
            };
            let in_types = !(macro_last && at + 1 == path.len());
            let label = match self.value(Some(&path[..at]), name).filter(|_| in_types) {
                // alloc: the error's text, for an error hir found
                Some(def) => format!("expected type, found {} `{name}` in {parent}", value_descr(cx, def)),
                // alloc: the error's text, for an error hir found
                None => format!("could not find `{name}` in {parent}"),
            };
            return error(message, label, None);
        }
        if name == "Self" {
            // alloc: the error's text, for an error hir found
            return error(message, "`Self` cannot be used in imports".to_owned(), None);
        }
        if name.chars().next().is_some_and(|c| c.is_ascii_uppercase()) {
            return error(
                // alloc: the error's text, for an error hir found
                format!("cannot find type `{name}` in {scope}"),
                // alloc: the error's text, for an error hir found
                format!("use of undeclared type `{name}`"),
                None,
            );
        }
        let mut suggestion = (name == "alloc").then(|| Suggestion {
            // alloc: the error's parts, for an error hir found
            parts: Vec::new(),
            // alloc: the error's text, for an error hir found
            message: "add `extern crate alloc` to use the `alloc` crate".to_owned(),
        });
        if suggestion.is_none() {
            suggestion = self.similar_module_or_crate(name).map(|it| Suggestion {
                // alloc: the error's parts, for an error hir found
                parts: vec![(self.span(ident.range), it)],
                // alloc: the error's text, for an error hir found
                message: "there is a crate or module with a similar name".to_owned(),
            });
        }
        // alloc: the error's text, for an error hir found
        let message = format!("cannot find module or crate `{name}` in {scope}");
        if let Some(def) = self.value(None, name) {
            // alloc: the error's text, for an error hir found
            let label = format!("{} `{name}` is not a crate or module", value_descr(cx, def));
            return error(message, label, suggestion);
        }
        let suggestion =
            // alloc: the error's parts, for an error hir found
            suggestion.or_else(|| Some(Suggestion { parts: Vec::new(), message: missing_crate(cx, name) }));
        // alloc: the error's text, for an error hir found
        error(message, format!("use of unresolved module or unlinked crate `{name}`"), suggestion)
    }

    /// rustc's `lookup_import_candidates` of a module named as the segment `at` of `path`, as
    /// the path that names it with what it shares with `path` left out.
    fn module_candidate(&self, path: &[Segment], at: usize) -> Option<String> {
        let db = self.cx.db;
        let name = path[at].name.as_str();
        let current = self.current()?;
        let mut candidates: Vec<Vec<String>> = self
            .cx
            .paths()
            .decls(self.cx)
            .named(name)
            .iter()
            .filter_map(|it| match it.def {
                hir::ModuleDef::Module(module) if it.local => Some(module),
                _ => None,
            })
            .filter(|m| m.is_visible_from(db, current))
            .map(|m| {
                // alloc: the error's parts, for an error hir found
                let mut segments: Vec<String> = Vec::new();
                if self.edition() > Edition::Edition2015 {
                    // alloc: the error's text, for an error hir found
                    segments.push("crate".to_owned());
                }
                // alloc: the error's text, for an error hir found
                segments.extend(m.path_segments(db).map(|it| it.as_str().to_owned()));
                segments
            })
            // alloc: the error's parts, for an error hir found
            .collect();
        candidates.sort_by_cached_key(|it| (it.len(), it.join("::")));
        let candidate = candidates.into_iter().next()?;
        let len = candidate.len();
        let start = (0..=at.min(len - 1)).find(|&i| path[i].name != candidate[i]).unwrap_or_default();
        Some(candidate[start..].join("::"))
    }

    /// rustc's `find_similarly_named_module_or_crate`: a crate of the extern prelude or a
    /// module below the import's module whose name is close to `name`.
    fn similar_module_or_crate(&self, name: &str) -> Option<String> {
        let db = self.cx.db;
        let current = self.current()?;
        let mut candidates = self.extern_prelude();
        for module in self.cx.krate.modules_iter(db) {
            let below = std::iter::successors(module.parent(db), |it| it.parent(db)).any(|it| it == current);
            if below && let Some(it) = module.name(db) {
                candidates.push(it);
            }
        }
        candidates.retain(|it| !it.as_str().is_empty());
        candidates.sort_unstable_by(|a, b| a.symbol().cmp_order(b.symbol()));
        candidates.dedup();
        // alloc: the error's parts, for an error hir found
        let candidates: Vec<&str> = candidates.iter().map(Name::as_str).collect();
        find_best_match_for_name(&candidates, name, None).copied().filter(|it| *it != name).map(str::to_owned)
    }

    /// rustc's `current_crate_outer_attr_insert_span`: before the first token after the crate
    /// root's inner attributes.
    fn crate_insert_span(&self) -> Span {
        let cx = self.cx;
        let file = cx.krate.root_module(cx.db).definition_source_file_id(cx.db);
        let source = cx.root(file);
        // A doc comment is a `DOC_COMMENT` node: an inner one is skipped with the inner
        // attributes, an outer one's token counts.
        let is_token = |token: &ast::Token<'_>| {
            !matches!(token.kind(), SyntaxKind::WHITESPACE | SyntaxKind::SHEBANG | SyntaxKind::COMMENT)
        };
        let at = source
            .children_with_tokens()
            .find_map(|it| match it {
                NodeOrToken::Node(node) => {
                    if ast::AnyAttr::cast(node).is_some_and(|it| it.kind() == ast::AttrKind::Inner) {
                        return None;
                    }
                    node.descendants_with_tokens().filter_map(NodeOrToken::into_token).find(is_token)
                }
                NodeOrToken::Token(token) => is_token(&token).then_some(token),
            })
            .map_or(source.text_range().end(), |it| it.text_range().start());
        cx.span(InFile::new(file, TextRange::empty(at)))
    }
}

/// What rustc's `report_path_resolution_error` gives.
/// rustc's help for a first segment that names no crate (`report_path_resolution_error`), with
/// `cargo add` when cargo runs the compile.
pub(crate) fn missing_crate(cx: &Cx<'_, '_>, name: &str) -> String {
    if cx.cargo() {
        // alloc: the error's text, for an error hir found
        format!("if you wanted to use a crate named `{name}`, use `cargo add {name}` to add it to your `Cargo.toml`")
    } else {
        // alloc: the error's text, for an error hir found
        format!("you might be missing a crate named `{name}`")
    }
}

/// [`missing_crate`] on Rust 2015, where it comes with an `extern crate` to insert.
pub(crate) fn missing_crate_2015(cx: &Cx<'_, '_>, name: &str) -> String {
    if cx.cargo() {
        // alloc: the error's text, for an error hir found
        format!(
            "if you wanted to use a crate named `{name}`, use `cargo add {name}` to add it to your `Cargo.toml` and import it in your code"
        )
    } else {
        // alloc: the error's text, for an error hir found
        format!("you might be missing a crate named `{name}`, add it to your project and import it in your code")
    }
}

struct PathError {
    message: String,
    label: String,
    suggestion: Option<Suggestion>,
    fact: Option<(Fact, String)>,
}

/// rustc's `module_to_string`: the path of a module, enum or trait from the crate root, or
/// none for a crate root.
fn module_to_string(cx: &Cx<'_, '_>, def: ModuleDef) -> Option<String> {
    let db = cx.db;
    let (module, own) = match def {
        ModuleDef::Module(m) => (m, None),
        def => (def.module(db)?, def.name(db)),
    };
    let names: Vec<String> =
        // alloc: the error's text, for an error hir found
        module.path_segments(db).chain(own).map(|it| it.as_str().to_owned()).collect();
    (!names.is_empty()).then(|| names_to_string(names.iter().map(String::as_str)))
}

/// rustc's `Res::descr` of what a name means as a value.
pub(crate) fn value_descr(cx: &Cx<'_, '_>, def: ModuleDef) -> &'static str {
    let db = cx.db;
    match def {
        ModuleDef::Function(_) => "function",
        ModuleDef::Const(_) => "constant",
        ModuleDef::Static(_) => "static",
        ModuleDef::Adt(hir::Adt::Struct(it)) => match it.kind(db) {
            StructKind::Tuple => "tuple struct",
            StructKind::Unit => "unit struct",
            StructKind::Record => "struct",
        },
        ModuleDef::EnumVariant(it) => match it.kind(db) {
            StructKind::Tuple => "tuple variant",
            StructKind::Unit => "unit variant",
            StructKind::Record => "struct variant",
        },
        ModuleDef::Macro(_) => "macro",
        _ => "value",
    }
}

/// The segments of a path as written: `::` first as `{{root}}`, keywords as themselves.
pub(super) fn path_segments(path: ast::Path<'_>) -> Vec<Segment> {
    // alloc: the error's parts, for an error hir found
    let mut out = Vec::new();
    push_path_segments(path, &mut out);
    out
}

fn push_path_segments(path: ast::Path<'_>, out: &mut Vec<Segment>) {
    for (i, segment) in path.segments().enumerate() {
        if i == 0
            && let Some(token) = segment.coloncolon_token()
        {
            // alloc: the error's text, for an error hir found
            out.push(Segment { name: ROOT.to_owned(), range: token.text_range() });
        }
        if let Some(name) = segment.name_ref() {
            // alloc: the error's text, for an error hir found
            out.push(Segment { name: name.text().to_owned(), range: name.syntax().text_range() });
        }
    }
}

/// Pushes the segments of the paths of `tree` and of the use trees it is nested in, outermost
/// first, as rustc's import has them: a `::` with no path before it as `{{root}}`. How many
/// trees those are.
fn nested_segments(tree: ast::UseTree<'_>, segments: &mut Vec<Segment>) -> usize {
    let depth = tree.parent_use_tree_list().map_or(0, |it| nested_segments(it.parent_use_tree(), segments));
    match tree.path() {
        Some(path) => push_path_segments(path, segments),
        None => {
            if let Some(token) = tree.coloncolon_token()
                && segments.is_empty()
            {
                // alloc: the error's text, for an error hir found
                segments.push(Segment { name: ROOT.to_owned(), range: token.text_range() });
            }
        }
    }
    depth + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segment(name: &str) -> Segment {
        Segment { name: name.to_owned(), range: TextRange::default() }
    }

    #[test]
    fn the_path_of_an_import_is_as_rustc_writes_it() {
        let import = Import {
            module_path: vec![segment(ROOT), segment("zed")],
            source: Some(segment("baz")),
            range: TextRange::default(),
        };
        assert_eq!(import_path(&import, None), "zed::baz");
        assert_eq!(import_path(&import, Some(1)), "zed");
        let glob = Import { module_path: vec![segment("a"), segment("b")], source: None, range: TextRange::default() };
        assert_eq!(import_path(&glob, None), "a::b::*");
        let single = Import { module_path: vec![segment(ROOT)], source: Some(segment("foo")), range: TextRange::default() };
        assert_eq!(import_path(&single, None), "foo");
    }

    /// Each use tree's segments, from the outermost tree it is in down to it, and how many trees
    /// that is: a leading `::` is the root, where it is written, and only at the start.
    #[test]
    fn a_use_trees_segments_are_those_of_the_trees_around_it() {
        use syntax::view::AstNode as _;
        let text = "use ::a::{b, c::{self, d::e}, *};\nuse crate::{f::{g as h}, ::i};\n\
                    use {j, k::l};\nuse super::super::m;\nuse ::{n, o::{}};\n";
        let parse = syntax::SourceFile::parse(text, Edition::CURRENT);
        let trees: Vec<_> = parse
            .root()
            .descendants()
            .filter_map(ast::UseTree::cast)
            .map(|tree| {
                let mut segments = Vec::new();
                let depth = nested_segments(tree, &mut segments);
                for it in &segments {
                    let written = if it.name == ROOT { "::" } else { it.name.as_str() };
                    assert_eq!(&text[it.range], written, "in {}", &text[tree.syntax().text_range()]);
                }
                let names: Vec<_> = segments.into_iter().map(|it| it.name).collect();
                (&text[tree.syntax().text_range()], names.join(" "), depth)
            })
            .collect();
        let expected = [
            ("::a::{b, c::{self, d::e}, *}", "{{root}} a", 1),
            ("b", "{{root}} a b", 2),
            ("c::{self, d::e}", "{{root}} a c", 2),
            ("self", "{{root}} a c self", 3),
            ("d::e", "{{root}} a c d e", 3),
            ("*", "{{root}} a", 2),
            ("crate::{f::{g as h}, ::i}", "crate", 1),
            ("f::{g as h}", "crate f", 2),
            ("g as h", "crate f g", 3),
            ("::i", "crate {{root}} i", 2),
            ("{j, k::l}", "", 1),
            ("j", "j", 2),
            ("k::l", "k l", 2),
            ("super::super::m", "super super m", 1),
            ("::{n, o::{}}", "{{root}}", 1),
            ("n", "{{root}} n", 2),
            ("o::{}", "{{root}} o", 2),
        ];
        let expected: Vec<_> = expected.iter().map(|&(tree, names, depth)| (tree, names.to_owned(), depth)).collect();
        assert_eq!(trees, expected);
    }

    #[test]
    fn names_are_joined_with_the_root_as_nothing() {
        assert_eq!(names_to_string(["self", "foo"]), "self::foo");
        assert_eq!(names_to_string([ROOT, "foo"]), "::foo");
    }
}
