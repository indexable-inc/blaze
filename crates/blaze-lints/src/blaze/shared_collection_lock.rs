//! `shared_collection_lock`: a collection shared behind one lock where AGENTS.md rule 9 names
//! the structure to use instead.
//!
//! - A field, a static, or a local in an `Arc` whose type is a `Mutex` or `RwLock` (std's, or
//!   `lock_api`'s, which `parking_lot`'s are) around a map (std's `HashMap` with any hasher,
//!   `BTreeMap`, hashbrown's and blaze-hash's `HashMap`, indexmap's `IndexMap`) or a set (std's
//!   `HashSet` and `BTreeSet`, hashbrown's and blaze-hash's `HashSet`, indexmap's `IndexSet`),
//!   the lock itself or inside `Arc`s, `LazyLock`s and `OnceLock`s (std's, or once_cell's
//!   `Lazy` and `OnceCell`): a shared map is `papaya::HashMap`, a shared set `papaya::HashSet`.
//!   For a hashed collection whose type is written out, in a field or in a static's lazy cell,
//!   the fix writes the papaya type with the collection's own arguments for the lock; it is
//!   `MaybeIncorrect`, the uses changing with it. A static with no lazy cell gets no fix (the
//!   papaya types have no `const` constructor), nor does a local (its type is inferred).
//! - A `push` or `extend` onto a `Vec` through a lock guard (a `Mutex`'s, or an `RwLock`'s
//!   write guard), inside a closure handed to a function of `blaze_graph::par`: the results of
//!   parallel items come back from `par` (`map`, `filter_map`, `extend`) in their order, never
//!   pushed into a `Mutex<Vec>`.
//!
//! Types and calls are resolved, so a renamed lock or collection, an alias (`FxHashMap`,
//! `parking_lot::Mutex`) and a macro-written field or loop are found. A local is found where
//! its `Arc` is made, not where it is cloned.

use hir_def::hir::{Expr, ExprId, Pat, PatId, Statement};
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

/// How many wrappers around the lock are looked through.
const LAYERS: usize = 4;

/// The collections of crates other than std, by crate: its name, whether it hashes (an ordered
/// one has no fix to a hash collection), and whether it is a set.
const COLLECTION_CRATES: &[(&str, &str, bool, Shape)] = &[
    ("hashbrown", "HashMap", true, Shape::Map),
    ("hashbrown", "HashSet", true, Shape::Set),
    ("indexmap", "IndexMap", false, Shape::Map),
    ("indexmap", "IndexSet", false, Shape::Set),
    ("blaze_hash", "HashMap", true, Shape::Map),
    ("blaze_hash", "HashSet", true, Shape::Set),
];

/// std's collections, by diagnostic item.
const STD_COLLECTIONS: [(&str, bool, Shape); 4] =
    [("HashMap", true, Shape::Map), ("BTreeMap", false, Shape::Map), ("HashSet", true, Shape::Set), ("BTreeSet", false, Shape::Set)];

/// The wrappers with no diagnostic item that hold a lock in place for every thread, by crate
/// and name: std's lazy and once cells and once_cell's `sync` ones.
const CELLS: &[(&str, &str)] = &[("std", "LazyLock"), ("std", "OnceLock"), ("once_cell", "Lazy"), ("once_cell", "OnceCell")];

/// A map or a set.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Shape {
    Map,
    Set,
}

impl Shape {
    /// What a collection of this shape is shared as.
    fn papaya(self) -> &'static str {
        match self {
            Shape::Map => "papaya::HashMap",
            Shape::Set => "papaya::HashSet",
        }
    }
}

/// What a type locks.
struct Locked {
    /// The lock's type, as the message names it.
    lock: String,
    /// The collection's type.
    collection: String,
    /// Whether the collection hashes, so the papaya type keeps what it does.
    hashes: bool,
    shape: Shape,
    /// How many wrappers (`Arc`s and cells) the lock is inside.
    layers: usize,
    /// How many of them are `Arc`s.
    arcs: usize,
    /// Whether one of them is a lazy or once cell.
    in_cell: bool,
}

/// What is declared with the locked type, named only once it is found.
enum Declared<'t> {
    Field(Option<ast::Name<'t>>),
    TupleField(Option<usize>),
    Static(Option<ast::Name<'t>>),
}

impl Declared<'_> {
    /// How a message names it, a field with the type it is of.
    fn said(&self, node: ast::Node<'_>) -> String {
        let owner = || {
            node.ancestors()
                .find_map(|it| ast::Adt::cast(it).and_then(|it| it.name()))
                .map_or_else(|| "this type".to_owned(), |it| format!("`{}`", it.text()))
        };
        // alloc: a message's part, made once a finding is
        match self {
            Declared::Field(Some(name)) => format!("the field `{}` of {}", name.text(), owner()),
            Declared::TupleField(Some(index)) => format!("the field `{index}` of {}", owner()),
            Declared::Field(None) | Declared::TupleField(None) => format!("this field of {}", owner()),
            Declared::Static(Some(name)) => format!("the static `{}`", name.text()),
            Declared::Static(None) => "this static".to_owned(),
        }
    }
}

pub(super) fn check_declaration<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    let (written, declared) = if let Some(field) = ast::RecordField::cast(node) {
        (field.ty(), Declared::Field(field.name()))
    } else if let Some(field) = ast::TupleField::cast(node) {
        let index = node.parent().and_then(|list| list.children().filter_map(ast::TupleField::cast).position(|it| it == field));
        (field.ty(), Declared::TupleField(index))
    } else if let Some(item) = ast::Static::cast(node) {
        (item.ty(), Declared::Static(item.name()))
    } else {
        return;
    };
    let Some(written) = written else { return };
    if !cx.is_written(node) {
        return;
    }
    let Some(ty) = cx.resolve_type(written) else { return };
    let Some(locked) = locked_collection(cx, &ty) else { return };
    let Some(range) = cx.range(written.syntax()) else { return };
    if exempt::exempt(cx.text(), range, LINT) {
        return;
    }
    let fixable = !matches!(declared, Declared::Static(_)) || locked.in_cell;
    let what = declared.said(node);
    let message = format!(
        "{what} shares a `{}` behind one `{}`: every access, from every thread, takes the one lock",
        locked.collection, locked.lock
    );
    let papaya = locked.shape.papaya();
    let fix = match locked.hashes && fixable {
        true => fix(cx, written, locked.layers, papaya).map(|(at, text)| (cx.span(at), text)),
        false => None,
    };
    let span = cx.span(range);
    let diagnostic = cx.blaze(LINT, node, span, message);
    match fix {
        Some(part) => {
            diagnostic.note(
                "its uses change with it: `.lock().insert(..)` becomes `.pin().insert(..)`, and a value read is \
                 guarded by the collection's epoch, not by a lock",
            );
            // alloc: the fix's one part and its message
            diagnostic.multipart_suggestion(format!("share it as a `{papaya}`"), vec![part], Applicability::MaybeIncorrect);
        }
        None if locked.hashes && !fixable => {
            diagnostic.note(format!("share it as a `{papaya}` in a `std::sync::LazyLock`, which makes it on first use"));
        }
        None if locked.hashes => {
            diagnostic.note(format!("share it as a `{papaya}`"));
        }
        None => {
            diagnostic.note(format!("share it as a `{papaya}`, sorting where its order is read"));
        }
    }
    diagnostic.help(LINT.help()).help(format!("for further information visit {}", LINT.docs_url()));
}

/// The collection `ty` locks: a lock around a map or a set, itself or inside `Arc`s and cells.
fn locked_collection(cx: &Cx<'_, '_>, ty: &hir::Type<'_>) -> Option<Locked> {
    let db = cx.db();
    let krate = cx.krate();
    let item = |name: &str| krate.diagnostic_item_adt(db, &intern::Symbol::intern(name));
    let mut ty = *ty;
    let (mut layers, mut arcs, mut in_cell) = (0, 0, false);
    let arc = item("Arc");
    while layers < LAYERS {
        let Some(adt) = ty.as_adt() else { break };
        let is_arc = arc == Some(adt);
        let is_cell = CELLS.iter().any(|(krate, name)| facts::adt_is(db, adt, krate, name));
        if !is_arc && !is_cell {
            break;
        }
        let inner = ty.type_arguments().next()?;
        ty = inner;
        layers += 1;
        arcs += usize::from(is_arc);
        in_cell |= is_cell;
    }
    let adt = ty.as_adt()?;
    let is_std_lock = [item("Mutex"), item("RwLock")].contains(&Some(adt));
    let is_lock_api = facts::adt_is(db, adt, "lock_api", "Mutex") || facts::adt_is(db, adt, "lock_api", "RwLock");
    if !is_std_lock && !is_lock_api {
        return None;
    }
    let inner = ty.type_arguments().last()?;
    let collection = inner.as_adt()?;
    let (hashes, shape) = match STD_COLLECTIONS.iter().find(|(name, ..)| item(name) == Some(collection)) {
        Some(&(_, hashes, shape)) => (hashes, shape),
        None => {
            let name = collection.name(db);
            let of = collection.module(db).krate(db);
            let &(_, _, hashes, shape) = COLLECTION_CRATES
                .iter()
                .find(|(krate_name, it, ..)| name.as_str() == *it && facts::crate_is(db, of, krate_name))?;
            (hashes, shape)
        }
    };
    let shown = |it: hir::Adt| it.name(db).as_str().to_owned(); // alloc: a message's part
    Some(Locked { lock: shown(adt), collection: shown(collection), hashes, shape, layers, arcs, in_cell })
}

/// The fix of a type written `written`, its lock inside `layers` wrappers: the written lock
/// type replaced by `papaya` with the written collection's arguments. `None` when the types are
/// not written out to the collection's arguments (an alias).
fn fix(cx: &Cx<'_, '_>, written: ast::Type<'_>, layers: usize, papaya: &str) -> Option<(TextRange, String)> {
    let mut lock = written;
    for _ in 0..layers {
        let inner = type_args(lock)?.next()?;
        lock = inner;
    }
    let collection = type_args(lock)?.last()?;
    let ast::Type::PathType(collection) = collection else { return None };
    let args = collection.path()?.segment()?.generic_arg_list()?;
    let range = cx.range(lock.syntax())?;
    let args = cx.range(args.syntax())?;
    Some((range, format!("{papaya}{}", cx.snippet(args))))
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
    for (_, expr) in body.exprs() {
        let Expr::Block { statements, .. } = expr else { continue };
        for statement in statements.iter() {
            let Statement::Let { pat, initializer: Some(init), .. } = statement else { continue };
            shared_local(cx, analysis, pat, init, &mut found);
        }
    }
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

/// A local bound by `pat` to `init`, a locked collection in an `Arc` made there, into `found`:
/// an initializer that is a path or a `clone` hands on an `Arc` made elsewhere.
fn shared_local(
    cx: &Cx<'_, '_>,
    analysis: &hir::BodyAnalysis<'_>,
    pat: PatId,
    init: ExprId,
    found: &mut Vec<Finding>,
) -> Option<()> {
    let db = cx.db();
    let body = analysis.body();
    if matches!(body.expr(init), Expr::Path(_))
        || analysis.callee(init).is_some_and(|(f, _)| hir::Function::from(f).name(db).as_str() == "clone")
    {
        return None;
    }
    let ty = analysis.ty(analysis.infer().expr_ty(init));
    let locked = locked_collection(cx, &ty)?;
    if locked.arcs == 0 {
        return None;
    }
    let range = analysis.expr_range(init)?;
    if exempt::exempt(cx.text(), range, LINT) {
        return None;
    }
    let local = match body.pat(pat) {
        Pat::Bind { id, .. } => format!("the local `{}`", body[id].name.as_str()),
        _ => "this local".to_owned(), // alloc: a message's part
    };
    let message = format!(
        "{local} shares a `{}` behind one `{}` in an `Arc`: every access, from every thread, takes the one lock",
        locked.collection, locked.lock
    );
    let papaya = locked.shape.papaya();
    let note = match locked.hashes {
        true => format!("share it as an `Arc<{papaya}<..>>`"),
        false => format!("share it as an `Arc<{papaya}<..>>`, sorting where its order is read"),
    };
    facts::push(found, LINT, range, message, Some(note));
    Some(())
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

    #[test]
    fn a_set_behind_a_lock_in_a_field_is_found_and_fixed_to_papayas_set() {
        let found = blaze_only(
            LINT,
            r#"
use std::{collections::{BTreeSet, HashSet}, sync::{Mutex, RwLock}};
pub struct Seen {
    names: Mutex<HashSet<u32>>,
    sorted: RwLock<BTreeSet<u32>>,
    brown: parking_lot::Mutex<hashbrown::HashSet<u32>>,
    indexed: Mutex<indexmap::IndexSet<u32>>,
    map: Mutex<indexmap::IndexMap<u32, u32>>,
}
"#,
        );
        assert_eq!(found.matches(FIELD).count(), 5, "{found}");
        assert!(found.contains("the field `names` of `Seen` shares a `HashSet` behind one `Mutex`"), "{found}");
        assert!(found.contains("the field `sorted` of `Seen` shares a `BTreeSet` behind one `RwLock`"), "{found}");
        assert!(found.contains("the field `brown` of `Seen` shares a `HashSet` behind one `Mutex`"), "{found}");
        assert!(found.contains("the field `indexed` of `Seen` shares a `IndexSet` behind one `Mutex`"), "{found}");
        assert!(found.contains("papaya::HashSet<u32>"), "{found}");
        assert_eq!(
            found.matches("sorting where its order is read").count(),
            3,
            "an ordered collection is not fixed to a hash one:\n{found}"
        );
    }

    /// A static with no lazy cell is found with no fix (papaya's types have no `const`
    /// constructor); one in a `LazyLock`, a `OnceLock` (around an `Arc` too) or once_cell's
    /// `Lazy` is fixed inside the cell.
    #[test]
    fn a_static_collection_behind_a_lock_is_found_and_fixed_inside_its_cell() {
        let found = blaze_only(
            LINT,
            r#"
use std::{collections::{HashMap, HashSet}, sync::{Arc, LazyLock, Mutex, OnceLock}};
pub static PLAIN: Mutex<HashMap<u32, u32>> = Mutex::new(loop {});
pub static LAZY: LazyLock<Mutex<HashMap<u32, u32>>> = LazyLock::new(|| loop {});
pub static ONCE: OnceLock<Arc<Mutex<HashMap<u32, u32>>>> = OnceLock::new();
pub static CELL: once_cell::sync::Lazy<Mutex<HashSet<u32>>> = loop {};
pub static COUNT: Mutex<u32> = Mutex::new(0);
pub static MAP: LazyLock<HashMap<u32, u32>> = LazyLock::new(|| loop {});
"#,
        );
        assert_eq!(found.matches(FIELD).count(), 4, "{found}");
        assert!(found.contains("the static `PLAIN` shares a `HashMap` behind one `Mutex`"), "{found}");
        assert!(found.contains("the static `LAZY` shares a `HashMap` behind one `Mutex`"), "{found}");
        assert!(found.contains("the static `ONCE` shares a `HashMap` behind one `Mutex`"), "{found}");
        assert!(found.contains("the static `CELL` shares a `HashSet` behind one `Mutex`"), "{found}");
        assert_eq!(found.matches("in a `std::sync::LazyLock`").count(), 1, "only `PLAIN` has no cell:\n{found}");
        assert!(found.contains("papaya::HashSet<u32>"), "{found}");
        assert!(found.matches("papaya::HashMap<u32, u32>").count() >= 2, "{found}");
    }

    /// A local is found where its `Arc` is made, not where it is cloned or moved; a lock that
    /// is not in an `Arc` is the function's own.
    #[test]
    fn a_collection_behind_a_lock_in_an_arc_local_is_found_where_it_is_made() {
        let found = blaze_only(
            LINT,
            r#"
use std::{collections::{BTreeSet, HashMap}, sync::{Arc, Mutex, RwLock}};
fn made() -> HashMap<u32, u32> { loop {} }
fn sorted() -> BTreeSet<u32> { loop {} }
pub fn shared() {
    let names = Arc::new(Mutex::new(made()));
    let again = names.clone();
    let handed = Arc::clone(&names);
    let moved = names;
    let lone = Mutex::new(made());
    let sets = Arc::new(RwLock::new(sorted()));
}
"#,
        );
        assert_eq!(found.matches(FIELD).count(), 2, "{found}");
        assert!(found.contains("the local `names` shares a `HashMap` behind one `Mutex` in an `Arc`"), "{found}");
        assert!(found.contains("the local `sets` shares a `BTreeSet` behind one `RwLock` in an `Arc`"), "{found}");
        assert!(found.contains("share it as an `Arc<papaya::HashMap<..>>`"), "{found}");
        assert!(found.contains("sorting where its order is read"), "{found}");
    }

    /// The fix applied as it reads lints clean, in a field and in a static's cell.
    #[test]
    fn the_fix_applied_lints_clean() {
        let lock = "std::sync::Mutex<std::collections::HashMap<u32, u32>>";
        for before in [
            format!("pub struct Cache {{ names: {lock} }}\n"),
            format!("pub static NAMES: std::sync::LazyLock<{lock}> = std::sync::LazyLock::new(|| loop {{}});\n"),
        ] {
            let found = blaze_only(LINT, &before);
            assert!(found.contains("papaya::HashMap<u32, u32>"), "{found}");
            let after = before.replace(lock, "papaya::HashMap<u32, u32>");
            assert_ne!(after, before);
            let left = blaze_only(LINT, &after);
            assert!(left.is_empty(), "the fixed file lints clean:\n{after}\n{left}");
        }
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
pub fn written(items: &[u32], out: &std::sync::RwLock<Vec<u32>>) {
    blaze_graph::par::for_each(items, |x| out.write().unwrap().push(*x));
}
pub fn mapped(items: &[u32]) -> Vec<u32> {
    blaze_graph::par::map(items, |x| *x)
}
"#,
        );
        assert_eq!(found.matches(PUSH).count(), 4, "{found}");
        assert!(
            found.contains("this `push` onto a `Vec` behind a `MutexGuard`, in the closure handed to `blaze_graph::par::for_each` in `gather`"),
            "{found}"
        );
        assert!(found.contains("in `guarded`"), "{found}");
        assert!(found.contains("in `handed`"), "{found}");
        assert!(found.contains("behind a `RwLockWriteGuard`, in the closure handed to `blaze_graph::par::for_each` in `written`"), "{found}");
        assert!(found.contains("`par::map`"), "{found}");
    }
}
