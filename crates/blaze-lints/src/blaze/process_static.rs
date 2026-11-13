//! `process_static`: a `static`, or a key of a `thread_local!`, that holds a growable
//! collection beneath a lock or a cell, or a concurrent collection: state with no owner but
//! the process, which grows for the process's life (codegen's `KEPT`, `WITNESSES` and `FOUND`
//! grew so in the daemon, which lives for days).
//!
//! A finding, outside test code ([`test_code::is_test_code`]), is such an item whose resolved
//! type holds, through `LazyLock`, `OnceLock`, `OnceCell`, `Option`, `Arc`, `Rc`, `Box`,
//! tuples, arrays and the fields of the tree's own types, [`DEPTH`] types deep:
//!
//! - a growable collection ([`GROWABLE`]: `Vec`, `VecDeque`, `BinaryHeap`, `LinkedList`,
//!   `String`, `PathBuf`, `OsString`, the hash and B-tree maps and sets of std, hashbrown,
//!   blaze_hash and indexmap) beneath a lock or a cell: std's `Mutex`, `RwLock`, `RefCell`,
//!   `Cell` and `UnsafeCell`, `blaze_core::lock`'s `Lock` and `ReadLock`, and any type named
//!   `*Lock` or `*Mutex` other than `LazyLock` and `OnceLock`;
//! - a concurrent collection ([`CONCURRENT`]: papaya's and dashmap's maps and sets,
//!   crossbeam's `SegQueue`).
//!
//! Not found: plain data, atomics, a collection set once and never grown (`OnceLock<Vec<T>>`,
//! `LazyLock<HashMap<..>>`, no lock or cell inside), a bounded `ArrayQueue`. `global_state`
//! names every interior-mutable static for the blaze edition; this lint is the narrow deny of
//! the ones that grow.
//!
//! There is no fix the lint can write: the state goes to an owner with a lifetime (the daemon,
//! a build, a task) that drops it, or the item says what bounds it with
//! `#[expect(blaze::process_static, reason = "..")]` (on the `thread_local!` call for a key).

use base_db::Db;
use hir::HirDisplay as _;
use syntax::view::{self as ast, AstNode as _, HasName as _};

use super::{Lint, facts, general_facts, global_state, test_code};
use crate::check::Cx;

const LINT: Lint = Lint::ProcessStatic;

/// How many types deep an item's type is looked into.
const DEPTH: u32 = 4;

/// The growable collections, by crate (`std` meaning core, alloc or std).
const GROWABLE: &[(&str, &[&str])] = &[
    (
        "std",
        &[
            "Vec", "VecDeque", "BinaryHeap", "LinkedList", "String", "PathBuf", "OsString", "HashMap",
            "HashSet", "BTreeMap", "BTreeSet",
        ],
    ),
    ("hashbrown", &["HashMap", "HashSet", "HashTable"]),
    ("blaze_hash", &["HashMap", "HashSet", "Map", "Set"]),
    ("indexmap", &["IndexMap", "IndexSet"]),
];

/// The collections that grow through a shared reference, by crate.
const CONCURRENT: &[(&str, &[&str])] = &[
    ("papaya", &["HashMap", "HashSet"]),
    ("dashmap", &["DashMap", "DashSet"]),
    ("crossbeam_queue", &["SegQueue"]),
    ("crossbeam", &["SegQueue"]),
];

/// std's locks and cells.
const STD_LOCKS: &[&str] = &["Mutex", "RwLock", "RefCell", "Cell", "UnsafeCell"];

/// std's wrappers a type is looked through by its type arguments.
const STD_WRAPPERS: &[&str] =
    &["LazyLock", "OnceLock", "OnceCell", "LazyCell", "Option", "Arc", "Rc", "Box", "LocalKey", "ManuallyDrop"];

/// What an item's type holds that grows.
struct Grows {
    /// The collection.
    collection: String,
    /// The lock or cell it is beneath; `None` for a concurrent collection.
    lock: Option<String>,
}

/// A `static` item.
pub(super) fn check_static<'t>(cx: &mut Cx<'_, 't>, item: ast::Static<'t>) {
    if !cx.on_blaze(LINT) || cx.expansion().is_some() || !cx.is_written(item.syntax()) {
        return;
    }
    let Some(hir::ModuleDef::Static(def)) = cx.def(item.syntax()) else { return };
    let Some(name) = item.name() else { return };
    let db = cx.db();
    let ty = def.ty(db);
    let Some(grows) = grows(cx, &ty, None, DEPTH) else { return };
    if test_code::is_test_code(cx, item.syntax()) {
        return;
    }
    let Some(range) = cx.range(name.syntax()) else { return };
    let shown = ty.display(db, cx.krate().to_display_target(db)).to_string();
    report(cx, item.syntax(), range, &format!("`static {}: {shown}`", name.text()), grows);
}

/// The keys of a `thread_local!` call of std's.
pub(super) fn check_thread_local<'t>(cx: &mut Cx<'_, 't>, call: ast::MacroCall<'t>) {
    if !cx.on_blaze(LINT) || cx.in_bang_call() || cx.expansion().is_some() || !cx.is_written(call.syntax()) {
        return;
    }
    if cx.std_macro(call).is_none_or(|it| it.as_str() != "thread_local") {
        return;
    }
    let Some(root) = cx.expand(call) else { return };
    // alloc: the few keys of one call
    let mut keys = Vec::new();
    global_state::keys_in(cx, root, 0, &mut keys);
    if keys.is_empty() || test_code::is_test_code(cx, call.syntax()) {
        return;
    }
    let db = cx.db();
    let target = cx.krate().to_display_target(db);
    for (item, key) in keys {
        let ty = key.ty(db);
        let Some(grows) = grows(cx, &ty, None, DEPTH) else { continue };
        let Some(name) = item.name() else { continue };
        let Some(range) = cx.range(name.syntax()).or_else(|| cx.range(call.syntax())) else { continue };
        let shown = ty.display(db, target).to_string();
        report(cx, call.syntax(), range, &format!("the `thread_local!` `{}: {shown}`", name.text()), grows);
    }
}

fn report<'t>(cx: &mut Cx<'_, 't>, at: ast::Node<'t>, range: syntax::TextRange, item: &str, grows: Grows) {
    let message = match &grows.lock {
        Some(lock) => format!(
            "{item} holds a `{}` beneath a `{lock}`: it grows for the process's life, with no owner to drop it",
            grows.collection
        ),
        None => format!(
            "{item} holds a `{}`, which grows through a shared reference: it grows for the process's life, with no \
             owner to drop it",
            grows.collection
        ),
    };
    let span = cx.span(range);
    cx.blaze(LINT, at, span, message)
        .note(
            "a static lives as long as the process, and the daemon lives for days: what it gathers is never \
             dropped, and every build after the first reads what earlier ones left",
        )
        .help(LINT.help())
        .help(format!("for further information visit {}", LINT.docs_url()));
}

/// What of `ty` grows: a growable collection beneath `lock`, the lock or cell it is under, or a
/// concurrent collection; `depth` types deep at most.
fn grows<'db>(cx: &Cx<'db, '_>, ty: &hir::Type<'db>, lock: Option<&str>, depth: u32) -> Option<Grows> {
    let db = cx.db();
    let ty = ty.strip_references();
    if let Some((inner, _)) = ty.as_array(db) {
        return grows(cx, &inner, lock, depth);
    }
    if ty.is_tuple() {
        return ty.tuple_fields(db).iter().find_map(|it| grows(cx, it, lock, depth));
    }
    let adt = ty.as_adt()?;
    let name = adt.name(db);
    let name = name.as_str();
    let krate = adt.module(db).krate(db);
    let std = general_facts::is_std_crate(db, krate);
    let of = |table: &[(&str, &[&str])]| {
        table.iter().any(|(it, names)| {
            names.contains(&name) && if *it == "std" { std } else { facts::crate_is(db, krate, it) }
        })
    };
    if of(CONCURRENT) {
        // alloc: a finding's message part
        return Some(Grows { collection: name.to_owned(), lock: None });
    }
    if of(GROWABLE) {
        // alloc: a finding's message part
        return lock.map(|lock| Grows { collection: name.to_owned(), lock: Some(lock.to_owned()) });
    }
    if depth == 0 {
        return None;
    }
    let is_lock = (std && STD_LOCKS.contains(&name))
        || facts::is_blaze_core_lock(db, adt)
        || ((name.ends_with("Lock") || name.ends_with("Mutex")) && !matches!(name, "LazyLock" | "OnceLock"));
    if is_lock {
        return ty.type_arguments().find_map(|it| grows(cx, &it, Some(name), depth - 1));
    }
    if std || !in_tree(db, krate) {
        if std && !STD_WRAPPERS.contains(&name) {
            return None;
        }
        return ty.type_arguments().find_map(|it| grows(cx, &it, lock, depth - 1));
    }
    ty.fields(db).iter().find_map(|(_, it)| grows(cx, it, lock, depth - 1))
}

/// Whether `krate` is one of the workspace's own, whose types are looked into by their fields.
fn in_tree(db: &Db, krate: hir::Crate) -> bool {
    krate.origin(db).is_local()
}

#[cfg(test)]
mod tests {
    use crate::tests::lint;

    const FOUND: &str = "grows for the process's life";

    /// A `std` with the collections, locks and once-cells the rule names.
    const CRATES: &str = r#"
//- minicore: sized, copy, clone, option, result, fn
//- /std/lib.rs crate:std
pub mod prelude {
    pub mod rust_2015 { pub use core::prelude::rust_2015::*; }
    pub mod rust_2018 { pub use core::prelude::rust_2018::*; }
    pub mod rust_2021 { pub use core::prelude::rust_2021::*; }
    pub mod rust_2024 { pub use core::prelude::rust_2024::*; }
}
pub mod vec { pub struct Vec<T>(*const T); unsafe impl<T> Sync for Vec<T> {} }
pub mod sync {
    pub struct Mutex<T>(T);
    unsafe impl<T> Sync for Mutex<T> {}
    impl<T> Mutex<T> { pub const fn new(t: T) -> Mutex<T> { Mutex(t) } }
    pub struct OnceLock<T>(Option<T>);
    unsafe impl<T> Sync for OnceLock<T> {}
    impl<T> OnceLock<T> { pub const fn new() -> OnceLock<T> { OnceLock(None) } }
    pub struct AtomicU64(u64);
    unsafe impl Sync for AtomicU64 {}
    impl AtomicU64 { pub const fn new(v: u64) -> AtomicU64 { AtomicU64(v) } }
}
//- /src/lib.rs crate:main deps:std
"#;

    fn found(main: &str) -> String {
        let fixture = format!("{}{}", CRATES.trim_start_matches('\n'), main.trim_start_matches('\n'));
        lint(&fixture, "-A clippy::all --warn=blaze::process_static")
    }

    /// The bug in miniature: codegen's kept functions, gathered in a locked `Vec` for the
    /// process's life, directly and through a struct of the tree.
    #[test]
    fn a_locked_collection_in_a_static_is_found() {
        let out = found(
            r#"
use std::{sync::{Mutex, OnceLock}, vec::Vec};
pub static KEPT: Mutex<Vec<u32>> = Mutex::new(Vec(core::ptr::null()));
pub struct Witnesses { pub found: Vec<u64> }
pub static WITNESSES: OnceLock<Mutex<Witnesses>> = OnceLock::new();
"#,
        );
        assert_eq!(out.matches(FOUND).count(), 2, "{out}");
        assert!(out.contains("`static KEPT: Mutex<Vec<u32>>` holds a `Vec` beneath a `Mutex`"), "{out}");
    }

    /// Not a finding: an atomic, a collection set once with no lock inside, plain data, and a
    /// test's static.
    #[test]
    fn atomics_set_once_collections_and_tests_are_left_alone() {
        let out = found(
            r#"
use std::{sync::{AtomicU64, Mutex, OnceLock}, vec::Vec};
pub static COUNT: AtomicU64 = AtomicU64::new(0);
pub static TABLE: OnceLock<Vec<u32>> = OnceLock::new();
pub static LIMIT: Mutex<u64> = Mutex::new(8);
pub static NAMES: [&str; 2] = ["a", "b"];
#[cfg(test)]
mod tests {
    pub static SEEN: super::Mutex<super::Vec<u32>> = super::Mutex::new(super::Vec(core::ptr::null()));
}
"#,
        );
        assert_eq!(out.matches(FOUND).count(), 0, "{out}");
    }

    /// A pool bounded by a cap is kept by an expectation on the static, which the finding
    /// fulfils.
    #[test]
    fn an_expectation_on_the_static_is_fulfilled() {
        let out = found(
            r#"
use std::{sync::Mutex, vec::Vec};
#[expect(blaze::process_static, reason = "a spare pool cleared to 16 buffers on every return")]
pub static SPARE: Mutex<Vec<u32>> = Mutex::new(Vec(core::ptr::null()));
"#,
        );
        assert_eq!(out.matches(FOUND).count(), 0, "{out}");
        assert!(!out.contains("unfulfilled"), "{out}");
    }
}
