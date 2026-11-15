//! `global_state` (in `blaze::edition`, allowed by default): state of the whole program that
//! any code can reach by name, which the blaze edition removes (docs/v2/plan/design/edition.md):
//!
//! - a `static mut`: every access is `unsafe`, and nothing orders one thread's write before
//!   another's read;
//! - a `static` whose type has interior mutability, decided by the type system: its type is
//!   not `Freeze` (an atomic, a `Mutex`, `RwLock`, `OnceLock` or `LazyLock`, a `Cell`,
//!   `RefCell` or `UnsafeCell`, a `blaze_core::lock` lock, or anything holding one), so a
//!   shared `&` to it changes it, the same rule `parallel`'s effects use
//!   ([`effects::interior_mutable`]);
//! - an item of a `thread_local!` call (`std`'s macro, by its definition, not its name as
//!   written): a key each thread has its own value under, which a fiber that moves between
//!   threads sees change under it.
//!
//! A `static` of plain data that is not `mut` is not found. The finding names the item and
//! lists where in the crate it is used, in the order the crate's bodies are read
//! ([`frozen_collection::crate_bodies`], deterministic) and each body once: for a `static mut`
//! the bodies that write it (assign to it or a place in it, borrow it mutably, as
//! [`frozen_collection::changes`] decides) and those that only read it; for the rest, the
//! bodies that use it, since a use through `&` may write. The list is what chooses the owner
//! the state moves to. What a macro wrote with no input of the file is not found. There is no
//! fix: each use changes with the owner.

use base_db::Db;
use hir::HirDisplay as _;
use hir_def::{
    ConstId, DefWithBodyId, StaticId,
    hir::{Expr, Pat},
    resolver::ValueNs,
};
use hir_ty::{BindingMode, ByRef};
use syntax::WalkEvent;
use syntax::view::{self as ast, AstNode as _, HasName as _};

use super::{
    Lint, facts, frozen_collection, general_facts, parallel::effects,
    thread_local_in_waiting_fn::owner_said,
};
use crate::check::Cx;

const LINT: Lint = Lint::GlobalState;

/// How many bodies a list of uses names before it says how many more there are.
const LISTED: usize = 8;

/// How many macro calls deep a `thread_local!` call's expansion is followed for its items.
const MACRO_DEPTH: u32 = 8;

/// The atomics of `core::sync::atomic`, by the primitive type each holds.
const ATOMICS: &[(&str, &str)] = &[
    ("bool", "AtomicBool"),
    ("u8", "AtomicU8"),
    ("u16", "AtomicU16"),
    ("u32", "AtomicU32"),
    ("u64", "AtomicU64"),
    ("usize", "AtomicUsize"),
    ("i8", "AtomicI8"),
    ("i16", "AtomicI16"),
    ("i32", "AtomicI32"),
    ("i64", "AtomicI64"),
    ("isize", "AtomicIsize"),
];

/// An item of global state a body can name: a `static`, or the `const` key a `thread_local!`
/// call declares.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Global {
    Static(StaticId),
    Const(ConstId),
}

/// Every use of a `static` or `const` by a body of a crate, in the order of
/// [`frozen_collection::crate_bodies`] and, in a body, of its expressions: the item, the body,
/// and whether the use changes it.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Uses(Vec<(Global, DefWithBodyId, bool)>);

/// The uses of `krate`'s bodies, as [`Uses`] says, once per crate.
#[blaze_query::tracked(returns(ref))]
pub(crate) fn global_uses(db: &Db, krate: base_db::Crate) -> Uses {
    // alloc: every use of a static or const by the crate, once per crate and edit
    let mut uses = Vec::new();
    for def in frozen_collection::crate_bodies(db, krate) {
        uses_in(db, def, &mut uses);
    }
    Uses(uses)
}

/// Adds the uses of `static`s and `const`s by the body of `def` to `out`.
fn uses_in(db: &Db, def: DefWithBodyId, out: &mut Vec<(Global, DefWithBodyId, bool)>) {
    let analysis = hir::BodyAnalysis::new(db, hir::DefWithBody::from(def));
    let body = analysis.body();
    let mut parents = None;
    let mut any_ref_mut = None;
    for (id, expr) in body.exprs() {
        if !matches!(expr, Expr::Path(_)) {
            continue;
        }
        let global = match general_facts::value_of(db, def, body, id) {
            Some(ValueNs::StaticId(it)) => Global::Static(it),
            Some(ValueNs::ConstId(it)) => Global::Const(it),
            _ => continue,
        };
        let parents = parents.get_or_insert_with(|| facts::Parents::of(body));
        let any_ref_mut = *any_ref_mut.get_or_insert_with(|| {
            let infer = analysis.infer();
            body.pats().any(|(pat, it)| {
                matches!(it, Pat::Bind { .. })
                    && matches!(infer.binding_mode(pat), Some(BindingMode(ByRef::Yes(m), _)) if m.is_mut())
            })
        });
        out.push((global, def, frozen_collection::changes(&analysis, parents, id, any_ref_mut)));
    }
}

/// A `static mut`, or a `static` whose type has interior mutability.
pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, item: ast::Static<'t>) {
    if !cx.on_blaze(LINT) || cx.expansion().is_some() || !cx.is_written(item.syntax()) {
        return;
    }
    let Some(hir::ModuleDef::Static(def)) = cx.def(item.syntax()) else { return };
    let Some(name) = item.name() else { return };
    let Some(range) = cx.range(name.syntax()) else { return };
    let db = cx.db();
    let krate = cx.krate();
    let id = StaticId::from(def);
    let mutable = item.mut_token().is_some();
    if !mutable && !effects::interior_mutable(db, krate.base(), id) {
        return;
    }
    let target = krate.to_display_target(db);
    let ty = def.ty(db);
    let shown = ty.display(db, target).to_string();
    let name = name.text();
    let uses = uses_said(db, krate.base(), Global::Static(id), mutable);
    let (message, why, instead) = if mutable {
        let instead = match atomic_of(db, &ty, target) {
            Some(atomic) => format!(
                "an atomic takes its place: `static {name}: core::sync::atomic::{atomic}`, read and written \
                 with `load`, `store` and `fetch_*`; or the state goes to the one owner the uses above choose"
            ),
            None => format!(
                "the state goes to the one owner the uses above choose, handed to the code that uses it; or, \
                 shared, `static {name}: blaze_core::lock::Lock<{shown}>`, a lock named for what it guards"
            ),
        };
        (
            format!("`static mut {name}: {shown}` is global state, which the blaze edition removes: {uses}"),
            "every access to a `static mut` is `unsafe`, and nothing orders one thread's write before another's \
             read",
            instead,
        )
    } else {
        (
            format!(
                "`static {name}: {shown}` has interior mutability, so it is global state, which the blaze edition \
                 removes: {uses}"
            ),
            "its type is not `Freeze` (it holds an atomic, a lock or a cell), so a shared `&` to it changes it: \
             any code that names it reads or writes state every caller shares",
            // alloc: a finding's note
            "the state goes to the one owner the uses above choose, handed to the code that uses it".to_owned(),
        )
    };
    let span = cx.span(range);
    cx.blaze(LINT, item.syntax(), span, message)
        .note(why)
        .note(instead)
        .help(LINT.help())
        .help(format!("for further information visit {}", LINT.docs_url()));
}

/// The items of a `thread_local!` call of `std`'s, each found once.
pub(super) fn check_thread_local<'t>(cx: &mut Cx<'_, 't>, call: ast::MacroCall<'t>) {
    // A `thread_local!` its own expansion calls (one per item after the first) is walked in
    // the call: [`keys_in`] already found its items.
    if !cx.on_blaze(LINT)
        || cx.in_bang_call()
        || cx.expansion().is_some()
        || !cx.is_written(call.syntax())
    {
        return;
    }
    if cx.std_macro(call).is_none_or(|it| it.as_str() != "thread_local") {
        return;
    }
    let Some(root) = cx.expand(call) else { return };
    // alloc: the few keys of one call
    let mut keys = Vec::new();
    keys_in(cx, root, 0, &mut keys);
    let db = cx.db();
    let krate = cx.krate();
    let target = krate.to_display_target(db);
    for (item, key) in keys {
        let Some(name) = item.name() else { continue };
        let Some(range) = cx.range(name.syntax()).or_else(|| cx.range(call.syntax())) else {
            continue;
        };
        let shown = key.ty(db).display(db, target).to_string();
        let uses = uses_said(db, krate.base(), Global::Const(ConstId::from(key)), false);
        let message = format!(
            "the `thread_local!` `{}: {shown}` is global state, which the blaze edition removes: {uses}",
            name.text()
        );
        let span = cx.span(range);
        cx.blaze(LINT, call.syntax(), span, message)
            .note(
                "each thread has its own value under the key, and code on a fiber can move between threads, \
                 so the value it sees can change between two uses",
            )
            .note("the state goes to the one owner the uses above choose, handed to the code that uses it")
            .help(LINT.help())
            .help(format!("for further information visit {}", LINT.docs_url()));
    }
}

/// The `const` items `root`, the expansion of a `thread_local!` call, declares at its top
/// level, through the macro calls there (`std` declares one item per call and calls itself
/// for the rest), to [`MACRO_DEPTH`] calls deep; in the order written.
pub(super) fn keys_in<'t>(
    cx: &Cx<'_, 't>,
    root: ast::Node<'t>,
    depth: u32,
    out: &mut Vec<(ast::Const<'t>, hir::Const)>,
) {
    let mut events = root.preorder();
    while let Some(event) = events.next() {
        let WalkEvent::Enter(it) = event else { continue };
        if let Some(item) = ast::Const::cast(it) {
            if let Some(hir::ModuleDef::Const(key)) = cx.def(it) {
                out.push((item, key));
            }
            events.skip_subtree();
            continue;
        }
        if let Some(call) = ast::MacroCall::cast(it) {
            if depth < MACRO_DEPTH
                && let Some(expansion) = cx.expand(call)
            {
                keys_in(cx, expansion, depth + 1, out);
            }
            events.skip_subtree();
            continue;
        }
        // An item's body (a `fn`, a `static`'s initializer) holds no key.
        if it != root && ast::Item::cast(it).is_some() {
            events.skip_subtree();
        }
    }
}

/// Where the crate uses `global`: for a `static mut` (`writes`), "written in `fn a`; read in
/// `fn b`"; for the rest, "used in `fn a`, `fn b`"; each body once, in [`Uses`]' order.
fn uses_said(db: &Db, krate: base_db::Crate, global: Global, writes: bool) -> String {
    // alloc: the bodies that use one item, a finding's message part
    let mut bodies: Vec<(DefWithBodyId, bool)> = Vec::new();
    for &(it, def, changes) in &global_uses(db, krate).0 {
        if it != global {
            continue;
        }
        match bodies.iter_mut().find(|(at, _)| *at == def) {
            Some((_, changed)) => *changed |= changes,
            None => bodies.push((def, changes)),
        }
    }
    if bodies.is_empty() {
        return "nothing in the crate uses it".to_owned(); // alloc: a finding's message part
    }
    if !writes {
        return format!("used in {}", listed(db, bodies.iter().map(|&(def, _)| def)));
    }
    let written = bodies.iter().filter(|(_, changed)| *changed).map(|&(def, _)| def);
    let read = bodies.iter().filter(|(_, changed)| !*changed).map(|&(def, _)| def);
    match (written.clone().next().is_some(), read.clone().next().is_some()) {
        (true, true) => format!("written in {}; read in {}", listed(db, written), listed(db, read)),
        (true, false) => format!("written in {}; read nowhere else", listed(db, written)),
        (false, _) => format!("read in {}; written nowhere", listed(db, read)),
    }
}

/// `defs` named, `` `fn a`, `fn b` ``, at most [`LISTED`] of them and then how many more.
fn listed(db: &Db, defs: impl Iterator<Item = DefWithBodyId>) -> String {
    // alloc: a finding's message part
    let mut out = String::new();
    let mut more = 0usize;
    for (index, def) in defs.enumerate() {
        if index >= LISTED {
            more += 1;
            continue;
        }
        if index > 0 {
            out.push_str(", ");
        }
        out.push_str(&owner_said(db, def));
    }
    if more > 0 {
        out.push_str(&format!(" and {more} more"));
    }
    out
}

/// The atomic of `core::sync::atomic` that holds a `ty`: [`ATOMICS`]' for a primitive, an
/// `AtomicPtr<T>` for a raw pointer to `T`.
fn atomic_of<'db>(
    db: &'db base_db::Db,
    ty: &hir::Type<'db>,
    target: hir::DisplayTarget,
) -> Option<String> {
    if let Some((pointee, _)) = ty.as_raw_ptr() {
        // alloc: a finding's message part
        return Some(format!("AtomicPtr<{}>", pointee.display(db, target)));
    }
    let builtin = ty.as_builtin()?;
    let name = builtin.name();
    ATOMICS
        .iter()
        .find(|(held, _)| *held == name.as_str())
        .map(|(_, atomic)| (*atomic).to_owned()) // alloc: a finding's message part
}
