//! Replace: every reference the front end resolves to one item, `from`, retargeted to another,
//! `to`, each named by a def path from a crate's name (`std::sync::Mutex`,
//! `parking_lot::Mutex`). How one file is rewritten is [`crate::retarget`]'s, which the lints'
//! fixes share: a `use` of `from` imports `to` instead (under the old name when the new one
//! differs), a glob of a module outside the view gets an import of `to` beside it, and a path
//! that qualifies `from` is rewritten up to its name. This drives it over the view, and does
//! what one file's tree can't:
//!
//! - The files are those of the view's crates that can name `from` (its own crate and those
//!   depending on it), whose bytes hold a name it goes by: its own, the last segment of the
//!   path given, or one a dependency outside the view exports it under (`std::alias::Lock`).
//! - `to` is written as the crate naming it sees it: `crate::…` in its own crate, else under
//!   the name the crate's manifest gives the dependency, checked to resolve to `to` there. A
//!   crate with no such dependency is an error: add it to the manifest first.
//! - A path through one of the view's own imports (`crate::locks::Mutex`, `locks` holding
//!   `pub use std::sync::Mutex;`) stays as it is: the import moves instead.
//! - A name a macro expands from its input (`id!(std::sync::Mutex::new(1))`) is rewritten at
//!   the range of the input it came from, and a name of one segment that reaches `from`
//!   through the prelude gets an import of `to` in its module.
//!
//! Every replace of a request is made in one pass over the same base: each file's retarget moves
//! each of their items, so two of them in one `use` list make one rewrite of it. An item given
//! twice, or one's `to` being another's `from` (a chain), is an error.
//!
//! An occurrence of one of those names the front end can't place (unresolved, cfg'd out, in a
//! `macro_rules!` body, in a `use` a macro expands) is an error naming where it is: the replace
//! would otherwise miss a reference without saying so.

use base_db::{Db, FileId};
use blaze_api::{ApiError, How};
use blaze_hash::HashSet;
use hir::{Crate, ItemInNs, ModPath, ModuleDef, Name, PathKind, PathResolution};
use hir_def::{
    Lookup as _, ModuleDefId, ModuleId,
    item_scope::{GlobId, ImportOrExternCrate, ImportOrGlob},
    resolver::HasResolver as _,
};
use memchr::memmem::Finder;
use syntax::{
    SyntaxKind, TextRange, TextSize,
    view::{self as ast, AstNode as _},
};

use crate::{
    Edits, Files,
    edit::{self, At, Touch},
    path, rename,
    retarget::{self, Names, Retarget},
};

/// The item a def path names.
struct Def {
    krate: Crate,
    /// The path's segments after the crate's name, as given.
    rest: Vec<String>,
    names: Names,
}

/// One `from=to` of the request, resolved.
struct Pair<'a> {
    from: &'a str,
    to: &'a str,
    old: Def,
    new: Def,
    /// The names a reference to `old` goes by ([`names_of`]).
    names: Vec<String>,
}

/// One rewrite, in the base text of the request's `file`th file.
struct Hit {
    file: u32,
    range: TextRange,
    insert: String,
}

/// What an occurrence of one of `from`'s names is to it.
enum Found {
    No,
    /// A reference the file's retarget rewrites (a `use`, or a path that qualifies it), or one
    /// through an import of the view, which moves itself.
    Covered,
    /// A path of one segment through a glob of a module outside the view in its own scope,
    /// which the file's retarget gives an import of `to` beside it.
    Direct(GlobId),
    /// A reference rewritten here: `range` of the file by `to`'s path.
    Rewrite(TextRange),
    /// An import of `to` that the reference needs.
    Add(Add),
}

/// An import of `to`, under the name a path of one segment writes it by.
#[derive(Clone, PartialEq, Eq, Hash)]
enum Add {
    /// In the path's module: it reaches `from` through the prelude, or through a glob of the
    /// view's module that declares it.
    Prelude(hir::Module, String),
    /// Beside the last glob of the view on its way to `from`, the glob's module being outside
    /// the view: with the glob's visibility, so every glob of it in the view sees `to`.
    BesideGlob(GlobId, String),
}

/// How a module's scope binds a name to `from`.
enum Via {
    /// The scope declares it.
    Declared,
    /// A `use` in `module` imports it, by name, or by the glob `glob`.
    Import {
        module: hir::Module,
        glob: Option<GlobId>,
    },
    ExternCrate,
}

/// Retargets every reference to the item each `from` names to the item its `to` names, every
/// pair of `asked` in one pass.
pub(crate) fn replace(
    cx: &mut Edits<'_, impl Files>,
    asked: &[(&str, &str)],
) -> Result<(), ApiError> {
    let fail = |what: String| ApiError::new(what, None);
    let db = cx.db;
    let files = cx.files;
    // alloc: one entry a crate of the view, once a replace
    let view: Vec<Crate> =
        Crate::all(db).into_iter().filter(|it| files.path(it.root_file(db)).is_some()).collect();
    if view.is_empty() {
        return Err(fail("the view has no crate the front end lowered".to_owned()));
    }
    let shares = |a: &Def, b: &Def| a.names.iter().flatten().any(|it| b.names.contains(&Some(*it)));
    let mut pairs: Vec<Pair<'_>> = Vec::with_capacity(asked.len()); // alloc: one a `--replace`, once a request
    for &(from, to) in asked {
        let old = def_of(db, &view, from).map_err(fail)?;
        let new = def_of(db, &view, to).map_err(fail)?;
        for (ns, (was, now)) in ["type", "value"].into_iter().zip(old.names.iter().zip(&new.names))
        {
            if was.is_some() && now.is_none() {
                return Err(fail(format!(
                    "`{from}` is in the {ns} namespace and `{to}` is not: a reference to it would name nothing"
                )));
            }
            if was.is_some() && was == now {
                return Err(fail(format!("`{from}` and `{to}` name the same item")));
            }
        }
        for pair in &pairs {
            if shares(&pair.old, &old) {
                return Err(fail(format!(
                    "`{from}` and `{}` name the same item, replaced twice in one request: give it once",
                    pair.from
                )));
            }
            if shares(&pair.new, &old) || shares(&new, &pair.old) {
                return Err(fail(format!(
                    "`{}={}` and `{from}={to}` chain, one's item being the other's target: give the \
                     chain's end instead",
                    pair.from, pair.to
                )));
            }
        }
        let names = names_of(db, &view, &old, from);
        pairs.push(Pair { from, to, old, new, names });
    }
    let reach: Vec<Vec<Crate>> = pairs.iter().map(|it| crates_of(db, &view, &it.old)).collect(); // alloc: the crates each pair reaches, once a request
    // alloc: the view's crates any pair reaches, once a request
    let scanned: Vec<Crate> =
        view.iter().copied().filter(|it| reach.iter().any(|reach| reach.contains(it))).collect();
    // alloc: one finder a name of each pair, once a request
    let finders: Vec<Vec<Finder<'_>>> = pairs
        .iter()
        .map(|pair| pair.names.iter().map(|it| Finder::new(it.as_bytes())).collect())
        .collect();

    let mut hits = Vec::new();
    let mut adds: Vec<(usize, Add)> = Vec::new();
    let mut direct = HashSet::default();
    for (krate, file) in files_of(db, &scanned) {
        let holds = |at: usize, bytes: &[u8]| {
            if !reach[at].contains(&krate) {
                return None;
            }
            let mut names = pairs[at].names.iter().zip(&finders[at]);
            names.find(|(_, it)| it.find(bytes).is_some()).map(|(name, _)| name)
        };
        let Some(path) = files.path(file) else {
            let text = db.file_text(file).text(db);
            for (at, pair) in pairs.iter().enumerate() {
                if let Some(name) = holds(at, text.as_bytes()) {
                    return Err(fail(format!(
                        "a file of crate `{}`, which can name `{}`, holds `{name}` and is outside the \
                         view (a `#[path]` or `include!` out of the checkout): the replace can't edit it",
                        name_of(db, krate),
                        pair.from
                    )));
                }
            }
            continue;
        };
        let (_, bytes) =
            files.file(path).ok_or_else(|| fail(format!("{path} is not in the view")))?;
        // alloc: whether the file holds a name of each pair, once a file
        let held: Vec<bool> = (0..pairs.len()).map(|at| holds(at, bytes).is_some()).collect();
        if !held.contains(&true) {
            continue;
        }
        let end = bytes.len();
        let index = cx.open_id(file).map_err(fail)?;
        let krate = cx
            .sema
            .file_to_module_def(file)
            .ok_or_else(|| fail(format!("{path} is no module of a crate the front end has")))?
            .krate(db);
        // alloc: `to` of each pair as this crate writes it, once a file
        let written: Vec<Result<String, String>> =
            pairs.iter().map(|it| spelled(db, &it.new, it.to, krate)).collect();
        let at_line = |at: TextSize, what: String| {
            let text = cx.working[index as usize].text();
            let line = memchr::memchr_iter(b'\n', &text.as_bytes()[..usize::from(at)]).count() + 1;
            ApiError::new(format!("{path}:{line}: {what}"), Some(format!("{path}:{line}")))
        };
        // alloc: whether the file's retarget moved an item of each pair, once a file
        let needed: Vec<std::cell::Cell<bool>> =
            pairs.iter().map(|_| std::cell::Cell::new(false)).collect();
        let placeholder = String::new();
        let resolve = |path| visible(cx, &view, &pairs, path);
        let to_of = |def: ModuleDef| {
            let at = pairs.iter().position(|it| it.old.names.contains(&Some(def)))?;
            needed[at].set(true);
            // alloc: the new path of one moved reference, which its rewrite owns
            Some(written[at].as_ref().unwrap_or(&placeholder).clone())
        };
        let retarget = Retarget { db, resolve: &resolve, to: &to_of };
        let root = cx.roots[index as usize];
        let rewrites = retarget.file(root).map_err(|what| fail(format!("{path}: {what}")))?;
        let mut found = Vec::new();
        for (at, pair) in pairs.iter().enumerate().filter(|(at, _)| held[*at]) {
            for (name, finder) in pair.names.iter().zip(&finders[at]) {
                for start in finder.find_iter(&root.file().text().as_bytes()[..end]) {
                    let Some(token) = rename::ident_at(root, start, name.len()) else { continue };
                    match classify(cx, &retarget, &view, &pair.old, index, token)
                        .map_err(|what| at_line(token.text_range().start(), what))?
                    {
                        Found::No | Found::Covered => {}
                        Found::Direct(glob) => {
                            direct.insert((at, glob));
                        }
                        Found::Add(add) => adds.push((at, add)),
                        Found::Rewrite(range) => found.push((at, token, range)),
                    }
                }
            }
        }
        for (at, written) in written.iter().enumerate() {
            if let Err(what) = written
                && (needed[at].get() || found.iter().any(|(it, ..)| *it == at))
            {
                return Err(fail(format!("{path}: {what}")));
            }
        }
        for rewrite in rewrites {
            let token = root
                .token_at_offset(rewrite.range.start())
                .right_biased()
                .ok_or_else(|| fail(format!("{path}: a rewrite past the file's end")))?;
            cx.touches.push(rename::touch_of(index, token));
            hits.push(Hit { file: index, range: rewrite.range, insert: rewrite.text });
        }
        for (at, token, range) in found {
            let text = written[at].as_ref().map_err(|what| fail(format!("{path}: {what}")))?;
            cx.touches.push(rename::touch_of(index, token));
            hits.push(Hit { file: index, range, insert: text.clone() }); // alloc: one rewrite's text, which its splice owns
        }
    }

    let mut seen = HashSet::default();
    for (at, add) in adds {
        let add = match add {
            Add::Prelude(module, name) => Add::Prelude(module.nearest_non_block_module(db), name),
            Add::BesideGlob(glob, _) if direct.contains(&(at, glob)) => continue,
            add => add,
        };
        // clone: the import's key, beside the import made of it
        if !seen.insert((at, add.clone())) {
            continue;
        }
        let (module, name) = match &add {
            Add::Prelude(module, name) => (*module, name),
            Add::BesideGlob(glob, name) => {
                (hir::Module::from(glob.use_.lookup(db).container), name)
            }
        };
        let pair = &pairs[at];
        let text = spelled(db, &pair.new, pair.to, module.krate(db)).map_err(fail)?;
        let line = match text.rsplit("::").next() == Some(name.as_str()) {
            true => text,
            false => format!("{text} as {name}"),
        };
        let hit = match add {
            Add::Prelude(..) => prelude_use(cx, module, &line),
            Add::BesideGlob(glob, _) => beside_glob(cx, glob, &line),
        };
        hits.push(hit.map_err(fail)?);
    }

    hits.sort_unstable_by(|a, b| {
        (b.file, b.range.start(), b.range.end(), &b.insert).cmp(&(
            a.file,
            a.range.start(),
            a.range.end(),
            &a.insert,
        ))
    });
    hits.dedup_by(|a, b| a.file == b.file && a.range == b.range && a.insert == b.insert);
    for hit in hits {
        let file = &mut cx.working[hit.file as usize];
        let now = file.to_now(hit.range).ok_or_else(|| {
            fail(format!(
                "{}: two rewrites of the replace overlap here, or an earlier edit of this request \
                 replaced this text",
                file.path
            ))
        })?;
        file.splice(now, hit.insert).map_err(fail)?;
    }
    Ok(())
}

/// The item `path` names: a crate's name (as its `[lib]` is named), then the item's path in
/// it. Of several crates by that name, the one the view's crates depend on.
fn def_of(db: &Db, view: &[Crate], path: &str) -> Result<Def, String> {
    let mut segments = path.split("::").map(str::trim);
    let first = segments.next().unwrap_or("");
    let rest: Vec<String> = segments.map(str::to_owned).collect(); // alloc: a path's few segments, once a replace
    if first.is_empty() || rest.is_empty() || rest.iter().any(String::is_empty) {
        return Err(format!(
            "`{path}` is no def path: give the crate's name, then the item's path in it (`std::sync::Mutex`)"
        ));
    }
    if matches!(first, "crate" | "self" | "super") {
        return Err(format!(
            "`{path}` starts at `{first}`: a replace names an item from its crate's name, as a crate \
             depending on it would"
        ));
    }
    let named: Vec<Crate> =
        Crate::all(db).into_iter().filter(|it| name_of(db, *it) == first).collect(); // alloc: the crates by one name, once a replace
    let depended = |krate: Crate| {
        view.contains(&krate)
            || view.iter().any(|it| it.dependencies_iter(db).any(|dep| dep.krate == krate))
    };
    let krate = match named.as_slice() {
        [] => {
            return Err(format!(
                "no crate of the build is named `{first}`, which `{path}` starts at"
            ));
        }
        [one] => *one,
        many => {
            let mut seen = many.iter().copied().filter(|it| depended(*it));
            match (seen.next(), seen.next()) {
                (Some(one), None) => one,
                (None, _) => {
                    return Err(format!(
                        "{} crates are named `{first}`, and none is the view's or a dependency of it",
                        many.len()
                    ));
                }
                (Some(_), Some(_)) => {
                    return Err(format!(
                        "more than one crate named `{first}` is the view's or a dependency of it, so `{path}` \
                         names no one item"
                    ));
                }
            }
        }
    };
    let root = krate.root_module(db);
    let found =
        root.resolve_mod_path(db, rest.iter().map(|it| Name::new_root(it))).into_iter().flatten();
    let mut names: Names = [None, None];
    for item in found {
        let (slot, def) = match item {
            ItemInNs::Types(def) => (0, def),
            ItemInNs::Values(def) => (1, def),
            ItemInNs::Macros(_) => {
                return Err(format!(
                    "`{path}` names a macro, and a replace moves types, functions, consts and statics"
                ));
            }
        };
        match def {
            ModuleDef::Module(_) => {
                return Err(format!(
                    "`{path}` names a module: replace the items in it, each by its path"
                ));
            }
            ModuleDef::BuiltinType(_) => {
                return Err(format!("`{path}` names a builtin type, which no path resolves to"));
            }
            _ => names[slot] = Some(def),
        }
    }
    if names == [None, None] {
        return Err(format!("`{path}` names nothing in crate `{first}`"));
    }
    Ok(Def { krate, rest, names })
}

/// A crate's name as a path writes it: its `[lib]` name.
fn name_of(db: &Db, krate: Crate) -> String {
    match krate.display_name(db) {
        Some(name) => name.crate_name().symbol().as_str().to_owned(),
        None => String::new(),
    }
}

/// The names a reference to `old` goes by, longest first: its own, the last segment of `from`,
/// and each a dependency outside the view exports it under.
fn names_of(db: &Db, view: &[Crate], old: &Def, from: &str) -> Vec<String> {
    let last = from.rsplit("::").next().unwrap_or(from).trim();
    let mut names = vec![last.strip_prefix("r#").unwrap_or(last).to_owned()]; // alloc: a few names, once a replace
    let mut deps = HashSet::default();
    for krate in view {
        deps.extend(krate.dependencies_iter(db).map(|it| it.krate).filter(|it| !view.contains(it)));
    }
    for def in old.names.iter().flatten() {
        if let Some(name) = def.name(db) {
            names.push(name.as_str().to_owned());
        }
        let Ok(id) = ModuleDefId::try_from(*def) else { continue };
        for dep in &deps {
            let map = hir_def::import_map::ImportMap::of(db, dep.base());
            for item in [
                hir_def::item_scope::ItemInNs::Types(id),
                hir_def::item_scope::ItemInNs::Values(id),
            ] {
                names.extend(
                    map.import_info_for(item)
                        .into_iter()
                        .flatten()
                        .map(|it| it.name.as_str().to_owned()),
                );
            }
        }
    }
    names.sort_unstable_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    names.dedup();
    names
}

/// The view's crates that can name `old`: the crates its items are in, and those depending on
/// them.
fn crates_of(db: &Db, view: &[Crate], old: &Def) -> Vec<Crate> {
    let mut homes = HashSet::default();
    homes.insert(old.krate);
    for def in old.names.iter().flatten() {
        if let Some(module) = def.module(db) {
            homes.insert(module.krate(db));
        }
    }
    let mut reach = HashSet::default();
    for home in homes {
        reach.insert(home);
        reach.extend(home.transitive_reverse_dependencies(db));
    }
    view.iter().copied().filter(|it| reach.contains(it)).collect()
}

/// The files of `crates`' modules, each once, with the crate it is first a module of.
fn files_of(db: &Db, crates: &[Crate]) -> Vec<(Crate, FileId)> {
    let mut seen = HashSet::default();
    let mut files = Vec::new();
    for krate in crates {
        for module in krate.modules(db) {
            if let Some(file) = module.as_source_file_id(db) {
                let file = file.file_id(db);
                if seen.insert(file) {
                    files.push((*krate, file));
                }
            }
        }
    }
    files
}

/// `to`'s path as `krate` writes it: from `crate` in its own crate, else from the name
/// `krate`'s manifest gives the dependency; checked to resolve to `to` at `krate`'s root.
fn spelled(db: &Db, new: &Def, to: &str, krate: Crate) -> Result<String, String> {
    let rest = new.rest.join("::");
    let (head, from_root) = if krate == new.krate {
        ("crate".to_owned(), true)
    } else {
        match krate.dependencies_iter(db).find(|it| it.krate == new.krate) {
            Some(dep) => (dep.name.as_str().to_owned(), false),
            None => {
                return Err(format!(
                    "crate `{}` has no dependency on `{}`, which `{to}` is in: add it to the crate's manifest first",
                    name_of(db, krate),
                    name_of(db, new.krate)
                ));
            }
        }
    };
    let text = format!("{head}::{rest}");
    let segments = (!from_root)
        .then(|| Name::new_root(&head))
        .into_iter()
        .chain(new.rest.iter().map(|it| Name::new_root(it)));
    let found: Vec<ModuleDef> = krate
        .root_module(db)
        .resolve_mod_path(db, segments)
        .into_iter()
        .flatten()
        .filter_map(|it| match it {
            ItemInNs::Types(def) | ItemInNs::Values(def) => Some(def),
            ItemInNs::Macros(_) => None,
        })
        .collect(); // alloc: the few items one path names
    if new.names.iter().flatten().any(|it| !found.contains(it)) {
        return Err(format!("`{text}` does not name `{to}` in crate `{}`", name_of(db, krate)));
    }
    Ok(text)
}

/// What `path` names, as the file's retarget sees it: nothing of `old` when it reaches `old`
/// through an import of the view (`crate::locks::Mutex`), which moves itself; and nothing for
/// a glob of the view's module, whose references [`classify_one`] follows to the module that
/// has `old`.
fn visible<'db>(
    cx: &Edits<'db, impl Files>,
    view: &[Crate],
    pairs: &[Pair<'_>],
    path: ast::Path<'db>,
) -> Names {
    let mut names = retarget::names_in(&cx.sema, path);
    if let [Some(ModuleDef::Module(module)), _] = names
        && path
            .syntax()
            .parent()
            .and_then(ast::UseTree::cast)
            .is_some_and(|it| it.star_token().is_some())
        && view.contains(&module.krate(cx.db))
    {
        return [None, None];
    }
    let moved = |it: &ModuleDef| pairs.iter().find(|pair| pair.old.names.contains(&Some(*it)));
    let Some(pair) = names.iter().flatten().find_map(moved) else { return names };
    let old = &pair.old;
    let Some(qualifier) = path.qualifier() else { return names };
    let Some(name) = path.segment().and_then(|it| it.name_ref()) else { return names };
    let Some(PathResolution::Def(ModuleDef::Module(module))) =
        cx.sema.resolve_path_per_ns(qualifier).and_then(|it| it.type_ns)
    else {
        return names;
    };
    if !view.contains(&module.krate(cx.db)) {
        return names;
    }
    if let Some(Via::Import { module: by, glob: None }) =
        via(cx.db, module, Name::new_root(name.text()), old)
        && by == module
    {
        for slot in &mut names {
            if slot.is_some_and(|it| old.names.contains(&Some(it))) {
                *slot = None;
            }
        }
    }
    names
}

/// How `module`'s scope, with the scopes around it and the preludes, binds `name` to `old`;
/// `None` when it binds it to something else.
fn via(db: &Db, module: hir::Module, name: Name, old: &Def) -> Option<Via> {
    let path = ModPath::from_segments(PathKind::Plain, [name]);
    let per_ns = ModuleId::from(module).resolver(db).resolve_module_path_in_items(db, &path);
    let is_old = |def: ModuleDefId| old.names.contains(&Some(ModuleDef::from(def)));
    let by = |use_: hir_def::UseId, glob| Via::Import {
        module: hir::Module::from(use_.lookup(db).container),
        glob,
    };
    if let Some(item) = per_ns.types
        && is_old(item.def)
    {
        return Some(match item.import {
            None => Via::Declared,
            Some(ImportOrExternCrate::Import(it)) => by(it.use_, None),
            Some(ImportOrExternCrate::Glob(it)) => by(it.use_, Some(it)),
            Some(ImportOrExternCrate::ExternCrate(_)) => Via::ExternCrate,
        });
    }
    if let Some(item) = per_ns.values
        && is_old(item.def)
    {
        return Some(match item.import {
            None => Via::Declared,
            Some(ImportOrGlob::Import(it)) => by(it.use_, None),
            Some(ImportOrGlob::Glob(it)) => by(it.use_, Some(it)),
        });
    }
    None
}

/// The module a glob imports from.
fn glob_source(cx: &Edits<'_, impl Files>, glob: GlobId) -> Result<hir::Module, String> {
    let loc = glob.use_.lookup(cx.db);
    cx.sema.parse_or_expand(loc.id.file_id);
    let tree = hir_def::src::use_tree_to_view(cx.db, loc.id, glob.idx);
    let path = tree.path().ok_or("a glob with no path")?;
    match cx.sema.resolve_path_per_ns(path).and_then(|it| it.type_ns) {
        Some(PathResolution::Def(ModuleDef::Module(module))) => Ok(module),
        Some(_) => Err("a glob of an enum or a trait on the way to the replaced item, which a replace can't follow".to_owned()),
        None => Err("a glob on the way to the replaced item that the front end can't resolve".to_owned()),
    }
}

/// What a path of one segment, `name` in `module`, needs to name `to`: its scopes followed
/// through the view's globs to the one that binds `old`. `here` is the range a rewrite of the
/// path replaces; `expanded`, whether a macro expanded it, which the file's retarget doesn't
/// see.
fn one_segment(
    cx: &Edits<'_, impl Files>,
    view: &[Crate],
    old: &Def,
    module: hir::Module,
    name: Name,
    expanded: bool,
    here: impl FnOnce() -> Result<TextRange, String>,
) -> Result<Found, String> {
    let db = cx.db;
    let add = || Found::Add(Add::Prelude(module, name.as_str().to_owned()));
    let mut at = module;
    let mut followed: Vec<GlobId> = Vec::new(); // alloc: the globs one path goes through, a few
    loop {
        let first = followed.is_empty();
        match via(db, at, name, old) {
            Some(Via::Declared) => {
                let home =
                    old.names.iter().flatten().find_map(|it| it.module(db)).map(|it| it.krate(db));
                let ours = home.is_some_and(|it| view.contains(&it));
                return Ok(if ours && first { Found::Rewrite(here()?) } else { add() });
            }
            Some(Via::Import { module: by, .. }) if !view.contains(&by.krate(db)) => {
                return Ok(add());
            }
            Some(Via::Import { glob: None, .. }) => return Ok(Found::Covered),
            Some(Via::Import { glob: Some(glob), .. }) => {
                if followed.contains(&glob) || followed.len() > 64 {
                    return Err("a cycle of globs on the way to the replaced item".to_owned());
                }
                let source = glob_source(cx, glob)?;
                if !view.contains(&source.krate(db)) {
                    let name = name.as_str().to_owned();
                    return Ok(match first && !expanded {
                        true => Found::Direct(glob),
                        false => Found::Add(Add::BesideGlob(glob, name)),
                    });
                }
                followed.push(glob);
                at = source;
            }
            Some(Via::ExternCrate) => return Ok(Found::No),
            None => {
                return Err("the front end resolves this path to the replaced item, and no scope around it binds the name".to_owned());
            }
        }
    }
}

fn unresolved() -> String {
    "the front end can't resolve this occurrence (cfg'd out under this view's configuration, or \
     unresolved), so can't tell whether it names the replaced item"
        .to_owned()
}

/// What `token`, one of `old`'s names in the `index`th file, is to it, across every macro
/// expansion it is in.
fn classify<'db>(
    cx: &Edits<'db, impl Files>,
    retarget: &Retarget<'_, 'db>,
    view: &[Crate],
    old: &Def,
    index: u32,
    token: ast::Token<'db>,
) -> Result<Found, String> {
    let mut found = Found::No;
    let mut other = false;
    for mapped in cx.sema.descend_into_macros_exact(token) {
        match classify_one(cx, retarget, view, old, index, token, mapped)? {
            Found::No => other = true,
            it => found = it,
        }
    }
    if !matches!(found, Found::No) && other {
        return Err(
            "a macro uses this token both for the replaced item and for something else".to_owned()
        );
    }
    Ok(found)
}

/// What `mapped`, `token` or a token a macro expanded from it, is to `old`.
fn classify_one<'db>(
    cx: &Edits<'db, impl Files>,
    retarget: &Retarget<'_, 'db>,
    view: &[Crate],
    old: &Def,
    index: u32,
    token: ast::Token<'db>,
    mapped: ast::Token<'db>,
) -> Result<Found, String> {
    let sema = &cx.sema;
    let expanded = mapped != token;
    let parent = mapped.parent().ok_or("a token outside any node")?;
    match parent.kind() {
        SyntaxKind::NAME => Ok(Found::No),
        SyntaxKind::NAME_REF => {
            let context = parent.parent().ok_or("a name reference outside any node")?;
            if context.kind() != SyntaxKind::PATH_SEGMENT {
                return Ok(Found::No);
            }
            let path = context
                .parent()
                .and_then(ast::Path::cast)
                .ok_or("a path segment outside a path")?;
            let in_use = path.syntax().ancestors().any(|it| it.kind() == SyntaxKind::USE_TREE);
            let is_old = |it: Option<PathResolution<'db>>| matches!(it, Some(PathResolution::Def(def)) if old.names.contains(&Some(def)));
            let hit = if in_use {
                let per_ns = sema.resolve_path_per_ns(path).ok_or_else(unresolved)?;
                is_old(per_ns.type_ns) || is_old(per_ns.value_ns)
            } else {
                match sema.resolve_path(path) {
                    Some(it) => is_old(Some(it)),
                    None => return Err(unresolved()),
                }
            };
            if !hit {
                return Ok(Found::No);
            }
            if in_use {
                if expanded {
                    return Err("a `use` a macro expands, which a replace can't rewrite: write it outside the macro".to_owned());
                }
                return Ok(Found::Covered);
            }
            if path.qualifier().is_some() {
                if !expanded {
                    return Ok(Found::Covered);
                }
                return Ok(match retarget.path(path) {
                    Some(_) => Found::Rewrite(original(cx, index, token, path)?),
                    None => Found::Covered,
                });
            }
            let module =
                sema.scope(path.syntax()).ok_or("a path the front end has no scope for")?.module();
            let here = || match expanded {
                true => original(cx, index, token, path),
                false => Ok(token.text_range()),
            };
            one_segment(cx, view, old, module, Name::new_root(mapped.text()), expanded, here)
        }
        SyntaxKind::TOKEN_TREE => Err(
            "in tokens no macro expanded (a `macro_rules!` body, an attribute's arguments, or a \
             macro call the front end could not expand)"
                .to_owned(),
        ),
        kind => Err(format!("an identifier in a {kind:?}, which a replace can't place")),
    }
}

/// The range of the `index`th file a path a macro expanded comes from: from where its input
/// starts to the end of `token`, the name in the input.
fn original<'db>(
    cx: &Edits<'db, impl Files>,
    index: u32,
    token: ast::Token<'db>,
    path: ast::Path<'db>,
) -> Result<TextRange, String> {
    let range = cx
        .sema
        .original_range_opt(path.syntax())
        .ok_or("a path a macro expands from no one range of its input")?;
    let end = token.text_range().end();
    if range.file_id.file_id(cx.db) != cx.ids[index as usize]
        || range.range.start() > token.text_range().start()
        || range.range.end() < end
    {
        return Err("a path a macro puts together from pieces of its input, which a replace can't rewrite in place".to_owned());
    }
    Ok(TextRange::new(range.range.start(), end))
}

/// The import of `line` in `module`, a module of a file: after its last `use`, or before its
/// first item.
fn prelude_use(
    cx: &mut Edits<'_, impl Files>,
    module: hir::Module,
    line: &str,
) -> Result<Hit, String> {
    let db = cx.db;
    let (file, range) = match module.as_source_file_id(db) {
        Some(file) => (file.file_id(db), None),
        None => {
            let decl = module
                .declaration_source_range(db)
                .ok_or("a module with no source the front end knows")?;
            let file = decl
                .file_id
                .file_id()
                .ok_or("a module a macro expands, which a replace can't add an import to")?;
            (file.file_id(db), Some(decl.value))
        }
    };
    let index = cx.open_id(file)?;
    let root = cx.roots[index as usize];
    let scope = match range {
        None => root,
        Some(range) => {
            let covering = match root.covering_element(range) {
                syntax::NodeOrToken::Node(it) => it,
                syntax::NodeOrToken::Token(it) => it.parent().ok_or("a token outside any node")?,
            };
            covering
                .ancestors()
                .find(|it| it.kind() == SyntaxKind::MODULE)
                .ok_or("a module's declaration that is no `mod`")?
        }
    };
    let files = cx.files;
    let at_path = &*cx.working[index as usize].path;
    let (_, bytes) = files.file(at_path).ok_or_else(|| format!("{at_path} is not in the view"))?;
    let (range, insert) = edit::add_use(root.file().text(), bytes.len(), scope, line)?;
    let at = match path::top_of(scope).filter(|_| scope != root) {
        Some(top) => At::Item(top.text_range().start()),
        None => At::File,
    };
    cx.touches.push(Touch { file: index, at, how: How::Scope });
    Ok(Hit { file: index, range, insert })
}

/// The import of `line` right after the `use` item of `glob`, with its attributes and
/// visibility: a glob of the module the glob is in then sees `to`, which shadows the glob's.
fn beside_glob(cx: &mut Edits<'_, impl Files>, glob: GlobId, line: &str) -> Result<Hit, String> {
    let db = cx.db;
    let loc = glob.use_.lookup(db);
    let file = loc
        .id
        .file_id
        .file_id()
        .ok_or("a glob a macro expands, which a replace can't add an import beside")?;
    let index = cx.open_id(file.file_id(db))?;
    let tree = hir_def::src::use_tree_to_view(db, loc.id, glob.idx);
    let item =
        tree.syntax().ancestors().find_map(ast::Use::cast).ok_or("a glob outside a `use`")?;
    let use_token = item.use_token().ok_or("a `use` item without `use`")?;
    let range = item.syntax().text_range();
    let head = &item.syntax().text()[..usize::from(use_token.text_range().start() - range.start())];
    let text = cx.roots[index as usize].file().text();
    let at = usize::from(range.start());
    let lead = &text[text[..at].rfind('\n').map_or(0, |it| it + 1)..at];
    let indent = if lead.trim().is_empty() { lead } else { "" };
    cx.touches.push(rename::touch_of(index, use_token));
    Ok(Hit {
        file: index,
        range: TextRange::empty(range.end()),
        insert: format!("\n{indent}{head}use {line};"),
    })
}
