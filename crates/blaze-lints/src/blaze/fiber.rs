//! `fiber_boundary`: a path written outside `blaze_graph::sched` that names the scheduler's
//! fibers ([docs/v2/plan/lanes/may-suspend/fiber-boundary.md](../../../../docs/v2/plan/lanes/may-suspend/fiber-boundary.md)).
//!
//! A written path (one whose parent is not a path, in a `use` tree, a type, an expression or a
//! pattern) is a finding when it or one of its qualifiers resolves to the module
//! `blaze_graph::sched::fiber`, to the crate `corosensei` or an item defined in it, or when it
//! resolves to the fiber driver: an item of `sched::fiber` named `Driver`, `Cx`, `Work` or
//! `Config`, by where it is defined, so a re-export is caught too. `extern crate` is checked by
//! the crate it names. Where a path is written is decided by the module it is in, walked up to
//! the crate root, never by its file.

use base_db::Db;
use hir_def::{DefWithBodyId, HasModule as _};
use syntax::view as ast;

use ast::AstNode as _;

use crate::check::Cx;

const GRAPH: &str = "blaze_graph";
const SCHED: &str = "sched";
const FIBER: &str = "fiber";
const COROSENSEI: &str = "corosensei";
/// The fiber driver's items, in `sched::fiber`.
const DRIVER: [&str; 4] = ["Driver", "Cx", "Work", "Config"];

/// What a path names that only `blaze_graph::sched` may.
#[derive(Clone, Copy)]
enum Named {
    Fiber,
    Corosensei,
    Driver(&'static str),
}

impl Named {
    fn message(self) -> String {
        match self {
            Named::Fiber => "`blaze_graph::sched::fiber` is named outside `blaze_graph::sched`".to_owned(),
            Named::Corosensei => "`corosensei` is named outside `blaze_graph::sched`".to_owned(),
            Named::Driver(name) => format!("the fiber driver `{name}` is named outside `blaze_graph::sched`"),
        }
    }
}

/// The finding of `node`, if it is a written path or an `extern crate` outside
/// `blaze_graph::sched` naming what only that module may.
pub(super) fn check<'t>(cx: &Cx<'_, 't>, node: ast::Node<'t>) -> Option<String> {
    let named = match (ast::Path::cast(node), ast::ExternCrate::cast(node)) {
        (Some(path), _) if path.parent_path().is_none() => path_names(cx, path)?,
        (_, Some(item)) => extern_names(cx, item)?,
        _ => return None,
    };
    (!in_sched(cx, node)?).then(|| named.message())
}

/// What `path` names, from its outermost node in to its first qualifier.
fn path_names<'t>(cx: &Cx<'_, 't>, path: ast::Path<'t>) -> Option<Named> {
    let db = cx.db();
    std::iter::once(path).chain(path.qualifiers()).find_map(|it| match cx.path(it)? {
        hir::PathResolution::Def(def) => named_by(db, def),
        _ => None,
    })
}

/// What `extern crate` names: the dependency of the crate under the name it writes.
fn extern_names(cx: &Cx<'_, '_>, item: ast::ExternCrate<'_>) -> Option<Named> {
    let name = item.name_ref()?;
    let name = name.text();
    let db = cx.db();
    let krate = cx.krate().dependencies_iter(db).find(|it| it.name.as_str() == name)?.krate;
    is_crate(db, krate, COROSENSEI).then_some(Named::Corosensei)
}

fn named_by(db: &Db, def: hir::ModuleDef) -> Option<Named> {
    if let hir::ModuleDef::Module(it) = def {
        if is_fiber(db, it) {
            return Some(Named::Fiber);
        }
        return is_crate(db, it.krate(db), COROSENSEI).then_some(Named::Corosensei);
    }
    let module = def.module(db)?;
    if is_crate(db, module.krate(db), COROSENSEI) {
        return Some(Named::Corosensei);
    }
    if !is_fiber(db, module) {
        return None;
    }
    let name = def.name(db)?;
    DRIVER.into_iter().find(|it| *it == name.as_str()).map(Named::Driver)
}

/// Whether `node` is written in `blaze_graph::sched` or a module inside it; `None` where its
/// module is not known.
fn in_sched<'t>(cx: &Cx<'_, 't>, node: ast::Node<'t>) -> Option<bool> {
    let db = cx.db();
    let mut module = Some(module_of(cx, node)?);
    while let Some(it) = module {
        if is_sched(db, it) {
            return Some(true);
        }
        module = it.parent(db);
    }
    Some(false)
}

/// The module `node` is written in: a body's own, or the item scope's for the items of a file.
fn module_of<'t>(cx: &Cx<'_, 't>, node: ast::Node<'t>) -> Option<hir::Module> {
    if let Some(analysis) = cx.analysis() {
        let def = DefWithBodyId::try_from(analysis.def()).ok()?;
        return Some(hir::Module::from(def.module(cx.db())));
    }
    match cx.resolve_name(node, "self")? {
        hir::PathResolution::Def(hir::ModuleDef::Module(it)) => Some(it),
        _ => None,
    }
}

/// Whether `module` is `blaze_graph::sched`.
fn is_sched(db: &Db, module: hir::Module) -> bool {
    module.name(db).is_some_and(|it| it.as_str() == SCHED)
        && module.parent(db).is_some_and(|it| it.is_crate_root(db))
        && is_crate(db, module.krate(db), GRAPH)
}

/// Whether `module` is `blaze_graph::sched::fiber`.
fn is_fiber(db: &Db, module: hir::Module) -> bool {
    module.name(db).is_some_and(|it| it.as_str() == FIBER)
        && module.parent(db).is_some_and(|it| is_sched(db, it))
}

fn is_crate(db: &Db, krate: hir::Crate, name: &str) -> bool {
    krate.base().extra_data(db).display_name.as_ref().is_some_and(|it| it.crate_name().as_str() == name)
}
