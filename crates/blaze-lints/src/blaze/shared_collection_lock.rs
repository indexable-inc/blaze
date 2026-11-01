//! `shared_collection_lock`: a collection shared behind one lock where AGENTS.md rule 9 names
//! the structure to use instead.
//!
//! - A field whose type is a `Mutex` or `RwLock` (std's, or `lock_api`'s, which `parking_lot`'s
//!   are) around a map (std's `HashMap` with any hasher, `BTreeMap`, hashbrown's `HashMap`,
//!   indexmap's `IndexMap`), itself or inside an `Arc`: a shared map is `papaya::HashMap`. For a
//!   hash map whose type is written out the fix writes `papaya::HashMap<..>` with the map's own
//!   arguments for the lock; it is `MaybeIncorrect`, the field's uses changing with it.
//! - A `push` or `extend` onto a `Vec` through a lock guard, inside a closure handed to a
//!   function of `blaze_graph::par`: the results of parallel items come back from `par`
//!   (`map`, `filter_map`, `extend`) in their order, never pushed into a `Mutex<Vec>`.
//!
//! Types and calls are resolved, so a renamed lock or map, an alias (`FxHashMap`,
//! `parking_lot::Mutex`) and a macro-written field or loop are found.

use hir_def::hir::{Expr, ExprId};
use syntax::{
    TextRange,
    view::{self as ast, AstNode as _, HasGenericArgs as _, HasName as _},
};

use super::{
    Finding, Lint,
    exempt,
    facts::{self, Parents},
};
use crate::{check::Cx, diag::Applicability, lints::guards::{Guards, Kind}};

const LINT: Lint = Lint::SharedCollectionLock;

/// How many expressions up a push is followed to the closure it is in.
const UP: usize = 32;

/// The maps of crates other than std, by crate, and whether each hashes (an ordered map has no
/// fix to a hash map).
const MAP_CRATES: &[(&str, &str, bool)] =
    &[("hashbrown", "HashMap", true), ("indexmap", "IndexMap", false), ("blaze_hash", "HashMap", true)];

/// What a field's type locks.
struct Locked {
    /// The lock's type, as the message names it.
    lock: String,
    /// The map's type.
    map: String,
    /// Whether the map hashes, so `papaya::HashMap` keeps what it does.
    hashes: bool,
    /// How many `Arc`s the lock is inside.
    arcs: usize,
}

pub(super) fn check_field<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    let (written, name) = if let Some(field) = ast::RecordField::cast(node) {
        (field.ty(), field.name().map(|it| format!("`{}`", it.text())))
    } else if let Some(field) = ast::TupleField::cast(node) {
        let index = node.parent().and_then(|list| list.children().filter_map(ast::TupleField::cast).position(|it| it == field));
        (field.ty(), index.map(|it| format!("`{it}`")))
    } else {
        return;
    };
    let Some(written) = written else { return };
    if !cx.is_written(node) {
        return;
    }
    let Some(ty) = cx.resolve_type(written) else { return };
    let Some(locked) = locked_map(cx, &ty) else { return };
    let Some(range) = cx.range(written.syntax()) else { return };
    if exempt::exempt(cx.text(), range, LINT) {
        return;
    }
    let owner = node
        .ancestors()
        .find_map(|it| ast::Adt::cast(it).and_then(|it| it.name()))
        .map_or_else(|| "this type".to_owned(), |it| format!("`{}`", it.text())); // alloc: a message's part
    let field = name.unwrap_or_else(|| "this field".to_owned()); // alloc: a message's part
    let message = format!(
        "the field {field} of {owner} shares a `{}` behind one `{}`: every access, from every thread, takes the one lock",
        locked.map, locked.lock
    );
    let fix = match locked.hashes {
        true => fix(cx, written, locked.arcs).map(|(at, text)| (cx.span(at), text)),
        false => None,
    };
    let span = cx.span(range);
    let diagnostic = cx.blaze(LINT, node, span, message);
    match fix {
        Some(part) => {
            diagnostic.note(
                "the field's uses change with it: `.lock().insert(k, v)` becomes `.pin().insert(k, v)`, and a \
                 value read is guarded by the map's epoch, not by a lock",
            );
            // alloc: the fix's one part
            diagnostic.multipart_suggestion("share it as a `papaya::HashMap`", vec![part], Applicability::MaybeIncorrect);
        }
        None if locked.hashes => {
            diagnostic.note("share it as a `papaya::HashMap`");
        }
        None => {
            diagnostic.note("share it as a `papaya::HashMap`, sorting where its order is read");
        }
    }
    diagnostic.help(LINT.help()).help(format!("for further information visit {}", LINT.docs_url()));
}

/// The map `ty` locks: a lock around a map, itself or inside `Arc`s.
fn locked_map(cx: &Cx<'_, '_>, ty: &hir::Type<'_>) -> Option<Locked> {
    let db = cx.db();
    let krate = cx.krate();
    let item = |name: &str| krate.diagnostic_item_adt(db, &intern::Symbol::intern(name));
    let mut ty = *ty;
    let mut arcs = 0;
    let arc = item("Arc");
    while arc.is_some() && ty.as_adt() == arc {
        let inner = ty.type_arguments().next()?;
        ty = inner;
        arcs += 1;
    }
    let adt = ty.as_adt()?;
    let is_std_lock = [item("Mutex"), item("RwLock")].contains(&Some(adt));
    let is_lock_api = facts::adt_is(db, adt, "lock_api", "Mutex") || facts::adt_is(db, adt, "lock_api", "RwLock");
    if !is_std_lock && !is_lock_api {
        return None;
    }
    let inner = ty.type_arguments().last()?;
    let map = inner.as_adt()?;
    let hashes = if Some(map) == item("HashMap") {
        true
    } else if Some(map) == item("BTreeMap") {
        false
    } else {
        let name = map.name(db);
        let map_krate = map.module(db).krate(db);
        let (_, _, hashes) = MAP_CRATES
            .iter()
            .find(|(krate_name, it, _)| name.as_str() == *it && facts::crate_is(db, map_krate, krate_name))?;
        *hashes
    };
    let shown = |it: hir::Adt| it.name(db).as_str().to_owned(); // alloc: a message's part
    Some(Locked { lock: shown(adt), map: shown(map), hashes, arcs })
}

/// The fix of a field written `written`, its lock inside `arcs` `Arc`s: the written lock type
/// replaced by `papaya::HashMap` with the written map's arguments. `None` when the types are
/// not written out to the map's arguments (an alias).
fn fix(cx: &Cx<'_, '_>, written: ast::Type<'_>, arcs: usize) -> Option<(TextRange, String)> {
    let mut lock = written;
    for _ in 0..arcs {
        let inner = type_args(lock)?.next()?;
        lock = inner;
    }
    let map = type_args(lock)?.last()?;
    let ast::Type::PathType(map) = map else { return None };
    let args = map.path()?.segment()?.generic_arg_list()?;
    let range = cx.range(lock.syntax())?;
    let args = cx.range(args.syntax())?;
    Some((range, format!("papaya::HashMap{}", cx.snippet(args))))
}

/// The types among the generic arguments of the last segment of a written path type.
fn type_args(ty: ast::Type<'_>) -> Option<impl Iterator<Item = ast::Type<'_>>> {
    let ast::Type::PathType(path) = ty else { return None };
    let args = path.path()?.segment()?.generic_arg_list()?;
    Some(args.generic_args().filter_map(|it| match it {
        ast::GenericArg::TypeArg(it) => it.ty(),
        _ => None,
    }))
}

/// The body form: a push onto a locked `Vec` in a `blaze_graph::par` closure.
pub(super) fn check(cx: &mut Cx<'_, '_>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    let found = find(cx);
    facts::report(cx, found);
}

fn find(cx: &Cx<'_, '_>) -> Vec<Finding> {
    // alloc: the findings of one body, reported at the edge
    let mut found = Vec::new();
    let Some(analysis) = cx.analysis() else { return found };
    let db = cx.db();
    let body = analysis.body();
    let infer = analysis.infer();
    let guards = Guards::new(cx);
    let vec = cx.krate().diagnostic_item_adt(db, &intern::Symbol::intern("Vec"));
    let mut parents = None;
    for (id, expr) in body.exprs() {
        let Expr::MethodCall { receiver, .. } = expr else { continue };
        let Some((callee, _)) = analysis.callee(id) else { continue };
        let method = hir::Function::from(callee).name(db);
        if !matches!(method.as_str(), "push" | "extend") || vec.is_none() || facts::impl_adt(db, callee) != vec {
            continue;
        }
        let receiver_ty = analysis.ty(infer.expr_ty(receiver)).strip_references();
        let Some(guard) = receiver_ty.as_adt() else { continue };
        if guards.kind(db, guard) != Some(Kind::Lock) {
            continue;
        }
        let parents = parents.get_or_insert_with(|| Parents::of(body));
        let Some(par) = par_call(cx, analysis, parents, id) else { continue };
        let Some(range) = analysis.expr_range(id) else { continue };
        if exempt::exempt(cx.text(), range, LINT) {
            continue;
        }
        let function = match analysis.def() {
            hir::DefWithBody::Function(f) => format!("`{}`", f.name(db).as_str()),
            _ => "this item".to_owned(), // alloc: a message's part
        };
        let message = format!(
            "this `{}` onto a `Vec` behind a `{}`, in the closure handed to `blaze_graph::par::{par}` in {function}, \
             makes every item take the one lock, in the order the threads get to it",
            method.as_str(),
            guard.name(db).as_str()
        );
        let note = "return each item's value from the closure of `par::map` (or `filter_map`, or `par::extend` \
                    into the `Vec`), which hands them back in the items' order";
        facts::push(&mut found, LINT, range, message, Some(note.to_owned())); // alloc: reported at the edge
    }
    found
}

/// The function of `blaze_graph::par` whose closure argument `expr` is inside, by its name.
fn par_call(cx: &Cx<'_, '_>, analysis: &hir::BodyAnalysis<'_>, parents: &Parents, expr: ExprId) -> Option<String> {
    let db = cx.db();
    let body = analysis.body();
    let mut child = expr;
    for parent in parents.up(expr, UP) {
        if matches!(body.expr(child), Expr::Closure { .. })
            && let Expr::Call { args, .. } = body.expr(parent)
            && args.contains(&child)
            && let Some((callee, _)) = analysis.callee(parent)
        {
            let f = hir::Function::from(callee);
            let module = f.module(db);
            if module.name(db).is_some_and(|it| it.as_str() == "par") && facts::crate_is(db, module.krate(db), "blaze_graph")
            {
                return Some(f.name(db).as_str().to_owned()); // alloc: a message's part
            }
        }
        child = parent;
    }
    None
}

#[cfg(test)]
mod tests {
    use crate::lints::drops_tests::blaze_only;

    const LINT: &str = "shared_collection_lock";
    const FIELD: &str = "every access, from every thread, takes the one lock";
    const PUSH: &str = "makes every item take the one lock";

    #[test]
    fn a_map_behind_a_lock_in_a_field_is_found_and_fixed_to_papaya() {
        let found = blaze_only(
            LINT,
            r#"
use std::{collections::{BTreeMap, HashMap}, sync::{Arc, Mutex, RwLock}};
pub struct Cache {
    names: Mutex<HashMap<u32, u32>>,
    shared: Arc<RwLock<HashMap<u32, u32>>>,
    sorted: Mutex<BTreeMap<u32, u32>>,
}
pub struct Pair(parking_lot::Mutex<hashbrown::HashMap<u32, u32>>);
"#,
        );
        assert_eq!(found.matches(FIELD).count(), 4, "{found}");
        assert!(
            found.contains("the field `names` of `Cache` shares a `HashMap` behind one `Mutex`"),
            "{found}"
        );
        assert!(found.contains("the field `shared` of `Cache` shares a `HashMap` behind one `RwLock`"), "{found}");
        assert!(found.contains("the field `0` of `Pair` shares a `HashMap` behind one `Mutex`"), "{found}");
        assert!(found.contains("papaya::HashMap<u32, u32>"), "{found}");
        assert!(
            found.contains("sorting where its order is read"),
            "an ordered map is not fixed to a hash map:\n{found}"
        );
    }

    /// A map alone, a lock around something else, `papaya`'s own map, and a local: none is a
    /// field sharing a map behind a lock.
    #[test]
    fn what_is_not_a_locked_map_field_is_left_alone() {
        let found = blaze_only(
            LINT,
            r#"
use std::{collections::HashMap, sync::Mutex};
pub struct Fine {
    map: HashMap<u32, u32>,
    count: Mutex<u32>,
    list: Mutex<Vec<u32>>,
    shared: papaya::HashMap<u32, u32>,
}
pub fn local() -> usize {
    let m: Mutex<HashMap<u32, u32>> = loop {};
    0
}
"#,
        );
        assert_eq!(found.matches(FIELD).count(), 0, "{found}");
    }

    #[test]
    fn a_renamed_lock_an_alias_and_a_macro_written_field_are_found() {
        let found = blaze_only(
            LINT,
            r#"
mod shared { pub use std::sync::Mutex as Lock; pub type Map = std::collections::HashMap<u32, u32>; }
use shared::{Lock, Map};
macro_rules! item { ($($t:tt)*) => { $($t)* }; }
pub struct Renamed { map: Lock<Map> }
item! {
    pub struct Handed { map: std::sync::Mutex<std::collections::HashMap<u32, u32>> }
}
"#,
        );
        assert_eq!(found.matches(FIELD).count(), 2, "{found}");
        assert!(found.contains("of `Renamed`"), "{found}");
        assert!(found.contains("of `Handed`"), "{found}");
    }

    /// The fix applied as it reads lints clean.
    #[test]
    fn the_fix_applied_lints_clean() {
        let before = "pub struct Cache { names: std::sync::Mutex<std::collections::HashMap<u32, u32>> }\n";
        let found = blaze_only(LINT, before);
        assert!(found.contains("papaya::HashMap<u32, u32>"), "{found}");
        let after = before.replace("std::sync::Mutex<std::collections::HashMap<u32, u32>>", "papaya::HashMap<u32, u32>");
        assert_ne!(after, before);
        let left = blaze_only(LINT, &after);
        assert!(left.is_empty(), "the fixed file lints clean:\n{after}\n{left}");
    }

    #[test]
    fn a_push_onto_a_locked_vec_in_a_par_closure_is_found() {
        let found = blaze_only(
            LINT,
            r#"
use blaze_graph::par as parallel;
use std::sync::Mutex;
macro_rules! push_it { ($out:expr, $x:expr) => { $out.lock().unwrap().push($x) }; }
pub fn gather(items: &[u32]) -> Vec<u32> {
    let out = Mutex::new(Vec::new());
    parallel::for_each(items, |x| out.lock().unwrap().push(*x));
    out.lock().unwrap().push(0);
    Vec::new()
}
pub fn guarded(items: &[u32], out: &parking_lot::Mutex<Vec<u32>>) {
    blaze_graph::par::for_each(items, |x| {
        let mut held = out.lock();
        held.push(*x);
    });
}
pub fn handed(items: &[u32], out: &Mutex<Vec<u32>>) {
    blaze_graph::par::for_each(items, |x| push_it!(out, *x));
}
pub fn mapped(items: &[u32]) -> Vec<u32> {
    blaze_graph::par::map(items, |x| *x)
}
"#,
        );
        assert_eq!(found.matches(PUSH).count(), 3, "{found}");
        assert!(
            found.contains("this `push` onto a `Vec` behind a `MutexGuard`, in the closure handed to `blaze_graph::par::for_each` in `gather`"),
            "{found}"
        );
        assert!(found.contains("in `guarded`"), "{found}");
        assert!(found.contains("in `handed`"), "{found}");
        assert!(found.contains("`par::map`"), "{found}");
    }
}
