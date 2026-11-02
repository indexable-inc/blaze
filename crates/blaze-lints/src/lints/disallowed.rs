//! `disallowed_types`, `disallowed_methods`, `disallowed_macros`: a use of an item a
//! `disallowed-*` entry of the package's `clippy.toml` names, found by resolution: a written
//! path to it (in a `use`, a type, an expression, a pattern, a macro call's path), and for a
//! method, a method call resolved to it. An entry with a `replacement` path has a
//! machine-applicable fix, which `blaze check --fix` makes: a `use` is retargeted whole, a qualified
//! path is rewritten, and a bare name follows the `use` that brings it. Where the replacement's
//! API differs and the fix knows how (std's locks to parking_lot's), the calls are rewritten
//! too.
//!
//! Follows clippy's `clippy_lints/src/disallowed_types.rs`, `disallowed_methods.rs` and
//! `disallowed_macros.rs`, and `clippy_config`'s `DisallowedPath` (MIT or Apache-2.0), at
//! rust-lang/rust's nightly of 2026-10: messages, the suggestion's message (the reason, else
//! "use"), and a path that resolves to nothing being an error unless `allow-invalid`. blaze's
//! own additions are `allow-in` (modules where the item is allowed) and `lint` (report under a
//! blaze lint instead).

use blaze_diagnostics::model::Applicability;
use blaze_hash::HashMap;
use blaze_intent::retarget::{Names, Retarget};
use hir::{ModuleDef, PathResolution};
use syntax::{
    SyntaxKind, TextRange, TextSize,
    view::{self as ast, AstNode as _, HasArgList as _, HasName as _},
};

use super::Implemented;
use crate::{
    blaze,
    check::Cx,
    conf::{Conf, Disallowed, Kind},
};

/// The configuration a crate's entries are resolved under.
#[blaze_query::interned(debug, unsafe(no_lifetime))]
pub(crate) struct ConfKey {
    #[returns(ref)]
    pub(crate) conf: Conf,
}

/// An entry, as the crate resolves it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Entry {
    pub(crate) kind: Kind,
    pub(crate) entry: Disallowed,
    /// The blaze lint it reports under, for `lint = "blaze::.."`.
    pub(crate) blaze: Option<blaze::Lint>,
    /// What `replacement` resolves to, when it is a path that does.
    pub(crate) replacement: Option<ModuleDef>,
}

/// A crate's entries, resolved.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Resolved {
    pub(crate) entries: Box<[Entry]>,
    /// Each item an entry names, with the entry's index.
    pub(crate) by_def: HashMap<ModuleDef, u32>,
    /// What of the configuration is wrong for the crate, in the order of its entries: a path
    /// that resolves to nothing, or to an item of another kind than its list's.
    pub(crate) errors: Box<[String]>,
}

impl Resolved {
    pub(crate) fn of(&self, def: ModuleDef) -> Option<&Entry> {
        self.by_def.get(&def).map(|&at| &self.entries[at as usize])
    }
}

/// What `blaze::wall_clock` reports, after the configured entries: clippy refuses a `lint` key
/// in `clippy.toml`, so the lint knows its paths, and an entry configured for one wins.
const WALL_CLOCK: [&str; 3] = ["std::time::Instant::now", "std::time::SystemTime::now", "std::time::Instant::elapsed"];
const WALL_CLOCK_REASON: &str =
    "a core reads time from its injected clock, `advance_clock(now)` (AGENTS.md rule 11); drivers read the wall clock";

/// The `disallowed-*` entries of `key`'s configuration, resolved as `krate` names their paths:
/// once per crate and configuration.
#[blaze_query::tracked(returns(ref))]
pub(crate) fn disallowed_defs(db: &base_db::Db, krate: base_db::Crate, key: ConfKey) -> Resolved {
    let krate = hir::Crate::from(krate);
    let conf = key.conf(db);
    let mut resolved = Resolved::default();
    let mut entries = Vec::new();
    let mut errors = Vec::new();
    for kind in Kind::ALL {
        let list = match conf.disallowed(kind) {
            Ok(list) => list,
            Err(error) => {
                errors.push(error);
                continue;
            }
        };
        for entry in list {
            let defs: Vec<ModuleDef> =
                resolve(db, krate, &entry.path).into_iter().filter(|it| of_kind(*it, kind)).collect();
            if defs.is_empty() && !entry.allow_invalid {
                let found = resolve(db, krate, &entry.path);
                errors.push(match found.first() {
                    None => format!("`{}` of `{}` does not refer to an item `{}` can name", entry.path, kind.key(), crate_name(db, krate)),
                    Some(_) => format!("`{}` of `{}` is not a {}", entry.path, kind.key(), kind_name(kind)),
                });
            }
            let blaze = match &entry.lint {
                None => None,
                Some(lint) => {
                    let name = lint.strip_prefix("blaze::").unwrap_or(lint);
                    let found = blaze::Lint::ALL.iter().copied().find(|it| it.name() == name);
                    if found.is_none() {
                        errors.push(format!("`lint` of `{}` is `{lint}`, which is no lint of blaze's", entry.path));
                    }
                    found
                }
            };
            let replacement = entry
                .replacement
                .as_deref()
                .filter(|it| is_path(it))
                .and_then(|it| resolve(db, krate, it).into_iter().find(|it| of_kind(*it, kind)));
            let at = entries.len() as u32;
            for def in defs {
                resolved.by_def.entry(def).or_insert(at);
            }
            entries.push(Entry { kind, entry, blaze, replacement });
        }
    }
    for path in WALL_CLOCK {
        let defs: Vec<ModuleDef> =
            resolve(db, krate, path).into_iter().filter(|it| of_kind(*it, Kind::Methods)).collect();
        if defs.is_empty() {
            continue;
        }
        let at = entries.len() as u32;
        for def in defs {
            resolved.by_def.entry(def).or_insert(at);
        }
        let entry = Disallowed {
            path: (*path).to_owned(),
            reason: Some(WALL_CLOCK_REASON.to_owned()),
            lint: Some(format!("blaze::{}", blaze::Lint::WallClock.name())),
            ..Disallowed::default()
        };
        entries.push(Entry { kind: Kind::Methods, entry, blaze: Some(blaze::Lint::WallClock), replacement: None });
    }
    resolved.entries = entries.into_boxed_slice();
    resolved.errors = errors.into_boxed_slice();
    resolved
}

fn crate_name(db: &base_db::Db, krate: hir::Crate) -> String {
    krate.display_name(db).map_or_else(|| "the crate".to_owned(), |it| it.to_string())
}

const fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::Types => "type",
        Kind::Methods => "function or method",
        Kind::Macros => "macro",
    }
}

/// Whether `def` is an item a list of `kind` may name: clippy's `DefKind` check of each entry.
fn of_kind(def: ModuleDef, kind: Kind) -> bool {
    match kind {
        Kind::Types => matches!(
            def,
            ModuleDef::Adt(_) | ModuleDef::TypeAlias(_) | ModuleDef::Trait(_) | ModuleDef::BuiltinType(_)
        ),
        Kind::Methods => matches!(def, ModuleDef::Function(_)),
        Kind::Macros => matches!(def, ModuleDef::Macro(_)),
    }
}

/// Whether `text` is a path, which a fix can write: `parking_lot::Mutex`, not
/// `std::hash::BuildHasherDefault<FxHasher>`.
fn is_path(text: &str) -> bool {
    !text.is_empty() && text.split("::").all(|it| !it.is_empty() && it.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'))
}

/// What `path` names from `krate`: its first segment `crate`, a dependency's name or the crate's
/// own, then modules, and past a type or trait its associated items (an inherent impl's, a
/// trait's), as clippy's `def_path_res` walks. Every namespace's item is kept.
fn resolve(db: &base_db::Db, krate: hir::Crate, path: &str) -> Vec<ModuleDef> {
    let mut segments = path.split("::");
    let Some(first) = segments.next() else { return Vec::new() };
    let root = if first == "crate" || krate.display_name(db).is_some_and(|it| it.crate_name().as_str() == first) {
        Some(krate)
    } else {
        krate.dependencies_iter(db).find(|it| it.name.as_str() == first).map(|it| it.krate)
    };
    let Some(root) = root else { return Vec::new() };
    let mut current = vec![ModuleDef::Module(root.root_module(db))];
    for segment in segments {
        let mut next = Vec::new();
        for def in current {
            match def {
                ModuleDef::Module(module) => {
                    for (name, def) in module.scope(db, None) {
                        if name.as_str() == segment
                            && let hir::ScopeDef::ModuleDef(def) = def
                            && !next.contains(&def)
                        {
                            next.push(def);
                        }
                    }
                }
                ModuleDef::Adt(adt) => {
                    for impl_ in hir::Impl::all_for_type(db, adt.ty(db)) {
                        if impl_.trait_(db).is_some() {
                            continue;
                        }
                        next.extend(assoc_named(db, impl_.items(db), segment));
                    }
                }
                ModuleDef::Trait(trait_) => next.extend(assoc_named(db, trait_.items(db), segment)),
                _ => {}
            }
        }
        current = next;
    }
    current
}

fn assoc_named(db: &base_db::Db, items: Vec<hir::AssocItem>, name: &str) -> impl Iterator<Item = ModuleDef> {
    items.into_iter().filter(move |it| it.name(db).is_some_and(|it| it.as_str() == name)).map(|it| match it {
        hir::AssocItem::Function(it) => ModuleDef::Function(it),
        hir::AssocItem::Const(it) => ModuleDef::Const(it),
        hir::AssocItem::TypeAlias(it) => ModuleDef::TypeAlias(it),
    })
}

/// The lint each list reports under.
const fn lint_of(kind: Kind) -> Implemented {
    match kind {
        Kind::Types => Implemented::DisallowedTypes,
        Kind::Methods => Implemented::DisallowedMethods,
        Kind::Macros => Implemented::DisallowedMacros,
    }
}

fn on(cx: &Cx<'_, '_>, entry: &Entry) -> bool {
    match entry.blaze {
        Some(lint) => cx.on_blaze(lint),
        None => cx.on(lint_of(entry.kind)),
    }
}

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if !cx.on(Implemented::DisallowedTypes)
        && !cx.on(Implemented::DisallowedMethods)
        && !cx.on(Implemented::DisallowedMacros)
        && !cx.on_any(!0 << super::BLAZE_BITS)
    {
        return;
    }
    if Kind::ALL.iter().all(|it| cx.conf().value(it.key()).is_none()) && !cx.on_blaze(blaze::Lint::WallClock) {
        return;
    }
    let resolved = cx.disallowed();
    if !resolved.entries.iter().any(|it| on(cx, it)) {
        return;
    }
    if node == cx.root() && cx.in_file() && cx.analysis().is_none() && is_crate_root(cx, node) {
        for error in &resolved.errors {
            let span = cx.span(TextRange::empty(TextSize::new(0)));
            let lint = Implemented::DisallowedTypes;
            if cx.on(lint) {
                cx.lint(lint, node, span, format!("error reading Clippy's configuration file: {error}"));
            }
        }
    }
    if let Some(path) = ast::Path::cast(node) {
        check_path(cx, resolved, path);
    } else if let Some(call) = ast::MethodCallExpr::cast(node) {
        check_method_call(cx, resolved, call);
    }
}

fn is_crate_root<'t>(cx: &Cx<'_, 't>, node: ast::Node<'t>) -> bool {
    module_of(cx, node).is_some_and(|it| it.is_crate_root(cx.db()))
}

/// The module `node` is written in: a body's own, or the item scope's for the items of a file.
fn module_of<'t>(cx: &Cx<'_, 't>, node: ast::Node<'t>) -> Option<hir::Module> {
    use hir_def::{DefWithBodyId, HasModule as _};
    if let Some(analysis) = cx.analysis() {
        let def = DefWithBodyId::try_from(analysis.def()).ok()?;
        return Some(hir::Module::from(def.module(cx.db())));
    }
    match cx.resolve_name(node, "self")? {
        PathResolution::Def(ModuleDef::Module(it)) => Some(it),
        _ => None,
    }
}

/// Whether `entry` allows `node` by its module: `crate::driver` allows `crate::driver` and the
/// modules inside it. A node whose module is not known is not allowed.
fn allowed<'t>(cx: &Cx<'_, 't>, entry: &Entry, node: ast::Node<'t>) -> bool {
    if entry.entry.allow_in.is_empty() {
        return false;
    }
    let db = cx.db();
    let Some(module) = module_of(cx, node) else { return false };
    let mut names: Vec<String> = module.path_to_root(db).iter().filter_map(|it| it.name(db)).map(|it| it.as_str().to_owned()).collect();
    names.reverse();
    let mut path = String::from("crate");
    let mut within = vec![path.clone()];
    for name in names {
        path.push_str("::");
        path.push_str(&name);
        within.push(path.clone());
    }
    entry.entry.allow_in.iter().any(|it| within.contains(it))
}

/// What `path` names in the type namespace, for [`Retarget`].
fn names(cx: &Cx<'_, '_>, path: ast::Path<'_>) -> Names {
    match cx.path(path) {
        Some(PathResolution::Def(it)) => [Some(it), None],
        _ => [None, None],
    }
}

/// Why `entry`'s fix can't be made in `cx`'s crate, if it can't.
fn unfixable(cx: &Cx<'_, '_>, entry: &Entry) -> Option<String> {
    let replacement = entry.entry.replacement.as_deref()?;
    if !is_path(replacement) {
        return Some(format!("`{replacement}` is not a path, so the fix is by hand"));
    }
    if entry.replacement.is_none() {
        let first = replacement.split("::").next().unwrap_or(replacement);
        let krate = crate_name(cx.db(), cx.krate());
        return Some(format!(
            "`{replacement}` does not refer to an item `{krate}` can name: add `{first}` to its `[dependencies]`, and `blaze check --fix` makes this"
        ));
    }
    None
}

/// The finding of `entry` at `at`, with its fix or the reason there is none.
fn report<'t>(cx: &mut Cx<'_, 't>, entry: &Entry, at: ast::Node<'t>, message: String, fix: Option<Result<Vec<(TextRange, String)>, String>>) {
    let Some(range) = cx.range(at) else { return };
    let span = cx.span(range);
    let fix = fix.map(|it| it.map(|parts| parts.into_iter().map(|(range, text)| (cx.span(range), text)).collect::<Vec<_>>()));
    let reason = entry.entry.reason.clone();
    let diag = match entry.blaze {
        Some(lint) => cx.blaze(lint, at, span, message),
        None => cx.lint(lint_of(entry.kind), at, span, message),
    };
    match fix {
        Some(Ok(parts)) => {
            diag.multipart_suggestion(reason.unwrap_or_else(|| "use".to_owned()), parts, Applicability::MachineApplicable);
        }
        Some(Err(why)) => {
            if let Some(reason) = reason {
                diag.note(reason);
            }
            diag.note(why);
        }
        None => {
            if let Some(reason) = reason {
                diag.note(reason);
            }
        }
    }
}

fn message(entry: &Entry) -> String {
    let what = match entry.kind {
        Kind::Types => "type",
        Kind::Methods => "method",
        Kind::Macros => "macro",
    };
    format!("use of a disallowed {what} `{}`", entry.entry.path)
}

fn check_path<'t>(cx: &mut Cx<'_, 't>, resolved: &Resolved, path: ast::Path<'t>) {
    let Some(PathResolution::Def(def)) = cx.path(path) else { return };
    if let ModuleDef::Module(module) = def {
        check_glob(cx, resolved, path, module);
        return;
    }
    let Some(entry) = resolved.of(def) else { return };
    if !on(cx, entry) || allowed(cx, entry, path.syntax()) {
        return;
    }
    let fix = match (&entry.entry.replacement, unfixable(cx, entry)) {
        (None, _) => None,
        (Some(_), Some(why)) => Some(Err(why)),
        (Some(replacement), None) => Some(path_fix(cx, resolved, path, replacement)),
    };
    report(cx, entry, path.syntax(), message(entry), fix);
}

/// A glob `use` of `module`, `path`, through which the scope of the `use` names a disallowed
/// item: no path of the `use` names the item, so the finding of its rewrite is the glob's, under
/// the first such item's entry. A body's walk sees only the body, never the file's `use`.
fn check_glob<'t>(cx: &mut Cx<'_, 't>, resolved: &Resolved, path: ast::Path<'t>, module: hir::Module) {
    let Some(tree) = path.syntax().parent().and_then(ast::UseTree::cast) else { return };
    if tree.star_token().is_none() || cx.analysis().is_some() {
        return;
    }
    let Some(item) = tree.syntax().ancestors().find_map(ast::Use::cast) else { return };
    let scope = item.syntax().parent().unwrap_or(cx.root());
    let resolve = |path: ast::Path<'t>| names(cx, path);
    let to = |_: ModuleDef| None;
    let retarget = Retarget { db: cx.db(), resolve: &resolve, to: &to };
    let named = module.scope(cx.db(), None).into_iter().find_map(|(name, def)| {
        let hir::ScopeDef::ModuleDef(def) = def else { return None };
        let entry = resolved.of(def)?;
        (on(cx, entry) && !allowed(cx, entry, path.syntax()) && retarget.names(scope, name.as_str(), def)).then_some(entry)
    });
    let Some(entry) = named else { return };
    let fix = match (&entry.entry.replacement, unfixable(cx, entry)) {
        (None, _) => None,
        (Some(_), Some(why)) => Some(Err(why)),
        (Some(_), None) => use_fix(cx, resolved, item).transpose(),
    };
    report(cx, entry, tree.syntax(), message(entry), fix);
}

/// The fix of a path naming a disallowed item whose replacement is `replacement`: the `use` it
/// is in, retargeted whole; a qualified path, rewritten; a bare name, the `use` of the file
/// that brings it, retargeted.
fn path_fix<'t>(cx: &Cx<'_, 't>, resolved: &Resolved, path: ast::Path<'t>, replacement: &str) -> Result<Vec<(TextRange, String)>, String> {
    if let Some(item) = path.syntax().ancestors().find_map(ast::Use::cast) {
        return use_fix(cx, resolved, item)?.ok_or_else(|| "the `use` resolves to nothing the fix moves".to_owned());
    }
    let name = path.segment().and_then(|it| it.name_ref()).ok_or("the path names no item")?;
    if path.qualifier().is_some() {
        let range = TextRange::new(path.syntax().text_range().start(), name.syntax().text_range().end());
        return written(cx, path.syntax(), range)
            .map(|range| vec![(range, replacement.to_owned())])
            .ok_or_else(|| "the path is made by a macro: write it outside the macro".to_owned());
    }
    for item in uses_in_scope(path.syntax(), name.text()) {
        if let Some(fix) = use_fix(cx, resolved, item)? {
            return Ok(fix);
        }
    }
    Err(format!("`{}` is brought in by an import outside this item: the fix is made at that import", name.text()))
}

/// The `use` items of the scopes around `node`, innermost first, that may bring `name`: one
/// importing it by its name or an alias, or a glob.
fn uses_in_scope<'t>(node: ast::Node<'t>, name: &str) -> Vec<ast::Use<'t>> {
    let mut found = Vec::new();
    for scope in node.ancestors() {
        if !matches!(scope.kind(), SyntaxKind::SOURCE_FILE | SyntaxKind::ITEM_LIST | SyntaxKind::STMT_LIST) {
            continue;
        }
        for item in scope.children().filter_map(ast::Use::cast) {
            let brings = item.syntax().descendants().filter_map(ast::UseTree::cast).any(|tree| {
                tree.star_token().is_some()
                    || match tree.rename().and_then(|it| it.name()) {
                        Some(alias) => alias.text() == name,
                        None => tree.use_tree_list().is_none()
                            && tree.path().and_then(|it| it.segment()).and_then(|it| it.name_ref()).is_some_and(|it| it.text() == name),
                    }
            });
            if brings {
                found.push(item);
            }
        }
    }
    found
}

/// Where `range`, inside `node`, is in the file: `None` unless the file has `node` as written,
/// so a fix never rewrites tokens a macro made.
fn written<'t>(cx: &Cx<'_, 't>, node: ast::Node<'t>, range: TextRange) -> Option<TextRange> {
    let at = cx.range(node)?;
    if *cx.source_of(node) != *node.text() {
        return None;
    }
    let shift = |it: TextSize| at.start() + (it - node.text_range().start());
    Some(TextRange::new(shift(range.start()), shift(range.end())))
}

/// The rewrite of `item` moving every item it imports whose entry has a fix; `None` when it
/// imports none.
fn use_fix<'t>(cx: &Cx<'_, 't>, resolved: &Resolved, item: ast::Use<'t>) -> Result<Option<Vec<(TextRange, String)>>, String> {
    if !cx.in_file() {
        return Err("the `use` is made by a macro: write it outside the macro".to_owned());
    }
    let resolve = |path: ast::Path<'t>| names(cx, path);
    let to = |def: ModuleDef| {
        let entry = resolved.of(def)?;
        if !on(cx, entry) || unfixable(cx, entry).is_some() {
            return None;
        }
        entry.entry.replacement.clone()
    };
    let retarget = Retarget { db: cx.db(), resolve: &resolve, to: &to };
    Ok(retarget.use_item(item, cx.root())?.map(|it| vec![(it.range, it.text)]))
}

/// The replacements whose API the fix also ports: the calls of std's locks that hand back
/// what parking_lot's don't.
const LOCKS: [(&str, &str); 3] = [
    ("std::sync::Mutex", "parking_lot::Mutex"),
    ("std::sync::RwLock", "parking_lot::RwLock"),
    ("std::sync::Condvar", "parking_lot::Condvar"),
];

/// The methods of std's `Mutex` and `RwLock` whose `LockResult` parking_lot's have none of.
const POISONING: [&str; 5] = ["lock", "read", "write", "into_inner", "get_mut"];
/// Those whose `TryLockResult` is parking_lot's `Option`: `.unwrap()` and `.expect(..)` read
/// the same.
const TRYING: [&str; 3] = ["try_lock", "try_read", "try_write"];

fn check_method_call<'t>(cx: &mut Cx<'_, 't>, resolved: &Resolved, call: ast::MethodCallExpr<'t>) {
    let Some(f) = cx.method(call) else { return };
    if let Some(entry) = resolved.of(ModuleDef::Function(f)) {
        if on(cx, entry) && !allowed(cx, entry, call.syntax()) {
            let fix = entry.entry.replacement.as_ref().map(|it| {
                Err(format!("a method call: write it as a call of `{it}`"))
            });
            report(cx, entry, call.syntax(), message(entry), fix);
        }
        return;
    }
    let db = cx.db();
    let Some(hir::AssocItemContainer::Impl(impl_)) = hir::AsAssocItem::as_assoc_item(f, db).map(|it| it.container(db)) else {
        return;
    };
    let Some(adt) = impl_.self_ty(db).as_adt() else { return };
    let Some(entry) = resolved.of(ModuleDef::Adt(adt)) else { return };
    let ported = LOCKS.iter().any(|&(from, to)| entry.entry.path == from && entry.entry.replacement.as_deref() == Some(to));
    if !ported || !on(cx, entry) || allowed(cx, entry, call.syntax()) {
        return;
    }
    lock_call(cx, entry, call);
}

/// A call of std's lock or `Condvar` that hands back what the replacement does not: the fix
/// drops the `.unwrap()` of a poisoning result and lends the guard to a wait.
fn lock_call<'t>(cx: &mut Cx<'_, 't>, entry: &Entry, call: ast::MethodCallExpr<'t>) {
    let Some(name) = call.name_ref() else { return };
    let name = name.text();
    let condvar = matches!(name, "wait" | "wait_while" | "wait_timeout" | "wait_timeout_while");
    let poisoning = POISONING.contains(&name) || TRYING.contains(&name) || matches!(name, "is_poisoned" | "clear_poison");
    if !condvar && !poisoning {
        return;
    }
    let to = entry.entry.replacement.as_deref().unwrap_or("");
    let unwrapped = call
        .syntax()
        .parent()
        .and_then(ast::MethodCallExpr::cast)
        .filter(|it| it.receiver().is_some_and(|it| it.syntax() == call.syntax()))
        .filter(|it| it.name_ref().is_some_and(|it| matches!(it.text(), "unwrap" | "expect")));
    if let Some(why) = unfixable(cx, entry) {
        let message = format!("`{}::{name}`, which `{to}` has differently", entry.entry.path);
        report(cx, entry, call.syntax(), message, Some(Err(why)));
        return;
    }
    if TRYING.contains(&name) {
        if unwrapped.is_none() {
            let message = format!("the `TryLockResult` of `{}::{name}`, which `{to}` returns as an `Option`", entry.entry.path);
            report(cx, entry, call.syntax(), message, Some(Err("port it by hand".to_owned())));
        }
        return;
    }
    if condvar {
        let fix = wait_fix(cx, call, name, unwrapped);
        let message = format!("`{}::{name}` takes the guard and hands it back; `{to}` borrows it", entry.entry.path);
        report(cx, entry, call.syntax(), message, Some(fix));
        return;
    }
    match unwrapped {
        Some(unwrap) if POISONING.contains(&name) => {
            let range = TextRange::new(call.syntax().text_range().end(), unwrap.syntax().text_range().end());
            let fix = written(cx, unwrap.syntax(), range)
                .map(|it| vec![(it, String::new())])
                .ok_or_else(|| "the call is made by a macro: write it outside the macro".to_owned());
            let message = format!("the `LockResult` of `{}::{name}`: `{to}` never poisons, and returns what it holds", entry.entry.path);
            report(cx, entry, unwrap.syntax(), message, Some(fix));
        }
        _ => {
            let message = format!("the poisoning result of `{}::{name}` is used other than unwrapped", entry.entry.path);
            report(cx, entry, call.syntax(), message, Some(Err("port it by hand".to_owned())));
        }
    }
}

/// `g = cv.wait(g).unwrap()` as `cv.wait(&mut g)`, and `wait_while` the same; another form, or
/// a `wait_timeout`, which parking_lot names `wait_for`, is ported by hand.
fn wait_fix<'t>(
    cx: &Cx<'_, 't>,
    call: ast::MethodCallExpr<'t>,
    name: &str,
    unwrapped: Option<ast::MethodCallExpr<'t>>,
) -> Result<Vec<(TextRange, String)>, String> {
    let by_hand = || "port it by hand: only `g = cv.wait(g).unwrap()` and `wait_while` are rewritten".to_owned();
    if !matches!(name, "wait" | "wait_while") {
        return Err(by_hand());
    }
    let unwrap = unwrapped.ok_or_else(by_hand)?;
    let assign = unwrap.syntax().parent().and_then(ast::BinExpr::cast).ok_or_else(by_hand)?;
    if !matches!(assign.op_kind(), Some(ast::BinaryOp::Assignment { op: None })) {
        return Err(by_hand());
    }
    let (Some(lhs), Some(rhs)) = (assign.lhs(), assign.rhs()) else { return Err(by_hand()) };
    if rhs.syntax() != unwrap.syntax() {
        return Err(by_hand());
    }
    let receiver = call.receiver().ok_or_else(by_hand)?;
    let args: Vec<ast::Expr<'t>> = call.arg_list().map(|it| it.args().collect()).unwrap_or_default();
    let (Some(guard), rest) = (args.first(), args.get(1..).unwrap_or_default()) else { return Err(by_hand()) };
    if !matches!(guard, ast::Expr::PathExpr(_)) || guard.syntax().text() != lhs.syntax().text() {
        return Err(by_hand());
    }
    let mut text = format!("{}.{name}(&mut {}", receiver.syntax().text(), guard.syntax().text());
    for arg in rest {
        text.push_str(", ");
        text.push_str(arg.syntax().text());
    }
    text.push(')');
    let range = written(cx, assign.syntax(), assign.syntax().text_range()).ok_or_else(by_hand)?;
    Ok(vec![(range, text)])
}
