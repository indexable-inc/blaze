//! `syscall_in_loop`: a call that stats or opens a path once per element of a collection, in a
//! crate whose file system work is batched: one whose root denies `blaze::hot` (or one of its
//! lints), the store and file-system crates `blaze_cas` and `blaze_vfs`, and `blaze_core`'s
//! `journal`. Each call is a kernel round trip; blaze's driver takes the whole batch at once.
//!
//! Per element is [`per_item`]'s: the body of a `for` loop, or a closure given to an
//! iterator's method. A call reaches a stat or an open through `hot_alloc`'s walk
//! ([`alloc::reaches_root`]), its root set [`Roots::Syscall`] ([`syscall`]): std's
//! `fs::{metadata, symlink_metadata, read_dir, canonicalize, exists}`, `File::{open, create,
//! create_new, open_buffered, metadata}`, `OpenOptions::open` and `DirEntry::metadata`, and
//! the stat and open calls of `libc`, `rustix` and `nix`. So `Path::exists`, `fs::read` and a
//! workspace helper that stats are found at the call in the loop. std's locks, formatting,
//! panic machinery and collections end the walk not reaching, as `tracing`, `log` and
//! `blaze_mem` do.
//!
//! The message names the call, the stat or open it reaches, and what it runs once per element
//! of; the help names what blaze has instead: `blaze_core::Effect::Stamp` (each name's
//! `Stamp` as one batch: `IORING_OP_STATX`s in flight together on io_uring, `fstatat` sweeps on
//! the driver's threads elsewhere; there is no `getattrlistbulk` path on macOS yet),
//! `Effect::Entries` (a directory's names with their stamps), and `Effect::Journal` (what
//! changed since a cursor, so nothing is stat-ed to find out). Test code is not checked.

use base_db::Db;
use hir_def::{
    DefWithBodyId, FunctionId, HasModule as _, hir::Expr, signatures::FunctionSignature,
};
use hir_ty::{ParamEnvAndCrate, db::HirDatabase as _};

use super::{
    Finding, Lint,
    alloc::{self, Resolved, Roots},
    facts::{self, Parents},
    per_item::{per_item, written_call},
};
use crate::check::Cx;

const LINT: Lint = Lint::SyscallInLoop;

/// The most steps followed from a call to the stat or open it reaches, to name that (the walk
/// itself is bounded by its memo; this bounds only the naming, rule 18).
const LONGEST_CHAIN: usize = 24;

/// The crates whose file system work is batched, besides those denying `blaze::hot`.
const STORE_CRATES: &[&str] = &["blaze_cas", "blaze_vfs"];

/// std's free functions of `fs` that stat or open.
const STD_FS: &[&str] = &["metadata", "symlink_metadata", "read_dir", "canonicalize", "exists"];
/// std's methods of `fs` that stat or open, by type.
const STD_FS_METHODS: &[(&str, &[&str])] = &[
    ("File", &["open", "create", "create_new", "open_buffered", "metadata"]),
    ("OpenOptions", &["open"]),
    ("DirEntry", &["metadata"]),
];
/// std's modules no stat or open is reached through.
const STD_STOPS: &[&str] =
    &["sync", "panicking", "rt", "alloc", "fmt", "collections", "hash", "thread", "time"];
/// The stat and open calls of `libc`, `rustix` and `nix`.
const SYSCALLS: &[&str] = &[
    "stat",
    "stat64",
    "lstat",
    "lstat64",
    "fstat",
    "fstat64",
    "fstatat",
    "fstatat64",
    "statat",
    "statx",
    "open",
    "open64",
    "openat",
    "openat64",
    "openat2",
    "access",
    "faccessat",
    "realpath",
];
/// The crates no stat or open is reached through.
const STOP_CRATES: &[&str] =
    &["parking_lot", "parking_lot_core", "lock_api", "tracing", "tracing_core", "log", "blaze_mem"];

/// What a function is to the walk for stats and opens.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Syscall {
    /// Walked into: its calls decide.
    Walk,
    /// Not followed.
    Stop,
    /// A stat or an open itself.
    Root,
}

/// What `f` is to the walk for stats and opens, by its crate, its module path and its name:
/// once per function.
#[blaze_query::tracked(returns(copy))]
pub(crate) fn syscall(db: &Db, f: FunctionId) -> Syscall {
    let function = hir::Function::from(f);
    let module = function.module(db);
    let krate = module.krate(db);
    let is = |name: &str| facts::crate_is(db, krate, name);
    let name = FunctionSignature::of(db, f).name.as_str();
    if is("std") {
        let top = module.path_to_root(db).into_iter().rev().find_map(|it| it.name(db));
        let self_adt = facts::impl_adt(db, f).map(|it| it.name(db));
        return match top.as_ref().map(hir::Name::as_str) {
            Some("fs") => {
                let root = match &self_adt {
                    None => STD_FS.contains(&name),
                    Some(adt) => STD_FS_METHODS
                        .iter()
                        .any(|(ty, names)| adt.as_str() == *ty && names.contains(&name)),
                };
                if root { Syscall::Root } else { Syscall::Walk }
            }
            Some(it) if STD_STOPS.contains(&it) => Syscall::Stop,
            _ => Syscall::Walk,
        };
    }
    if is("libc") || is("rustix") || is("nix") {
        return if SYSCALLS.contains(&name) { Syscall::Root } else { Syscall::Stop };
    }
    if STOP_CRATES.iter().any(|it| is(it)) {
        return Syscall::Stop;
    }
    Syscall::Walk
}

pub(super) fn check(cx: &mut Cx<'_, '_>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    let found = find(cx);
    facts::report(cx, found);
}

/// Whether the body of `def` is in a crate whose file system work is batched.
fn in_scope(cx: &Cx<'_, '_>, def: hir::DefWithBody) -> bool {
    let db = cx.db();
    let krate = cx.krate();
    if STORE_CRATES.iter().any(|it| facts::crate_is(db, krate, it)) {
        return true;
    }
    if facts::crate_is(db, krate, "blaze_core") {
        let top = def.module(db).path_to_root(db).into_iter().rev().find_map(|it| it.name(db));
        if top.is_some_and(|it| it.as_str() == "journal") {
            return true;
        }
    }
    super::HOT.iter().any(|&it| cx.root_denies(it))
}

fn find(cx: &Cx<'_, '_>) -> Vec<Finding> {
    // alloc: the findings of one body, reported at the edge
    let mut found = Vec::new();
    let Some(analysis) = cx.analysis() else { return found };
    let Ok(def) = DefWithBodyId::try_from(analysis.def()) else { return found };
    if !in_scope(cx, analysis.def()) || cx.is_in_test(cx.root()) {
        return found;
    }
    let db = cx.db();
    let env = ParamEnvAndCrate {
        param_env: db.trait_environment(def.generic_def(db)),
        krate: def.krate(db),
    };
    let body = analysis.body();
    let infer = analysis.infer();
    let mut parents = None;
    for (id, expr) in body.exprs() {
        if !matches!(expr, Expr::Call { .. } | Expr::MethodCall { .. })
            || !written_call(analysis, id)
        {
            continue;
        }
        let parents = parents.get_or_insert_with(|| Parents::of(body));
        let Some(over) = per_item(cx, analysis, parents, id) else { continue };
        let Some((callee, args)) = alloc::written_callee(infer, body, id) else { continue };
        let resolved = alloc::resolve(db, env, callee, args);
        if !alloc::instance_reaches_root(db, env, resolved, true, Roots::Syscall) {
            continue;
        }
        let Some(range) = analysis.expr_range(id) else { continue };
        let called = fn_name(db, callee);
        let reached = root_of(db, env, resolved);
        let via = match reached == called {
            true => format!("`{called}`"),
            false => format!("`{called}`, which reaches `{reached}`,"),
        };
        let message = format!(
            "this call to {via} stats or opens a path once per element of {}: a system call per item",
            over.say(db)
        );
        let note = "a call per item is a kernel round trip each, where blaze's driver takes the whole batch in \
                    one submission; this crate's file system work is batched";
        facts::push(&mut found, LINT, range, message, Some(note.to_owned())); // alloc: a finding's note
    }
    found
}

/// The stat or open that `first`, what a call runs, reaches: at most [`LONGEST_CHAIN`] steps
/// down, named as `Type::name` or a free function's name.
fn root_of<'db>(db: &'db Db, env: ParamEnvAndCrate<'db>, first: Resolved<'db>) -> String {
    let mut at = first;
    for _ in 0..LONGEST_CHAIN {
        match alloc::next_step(db, env, at, true, Roots::Syscall) {
            Some(next) => at = next,
            None => break,
        }
    }
    match at {
        Resolved::Function(f, _) => fn_name(db, f),
        Resolved::Derived(..) => "a derived method".to_owned(), // alloc: a message's part
    }
}

/// `f` as `Type::name`, or its name for a free function.
fn fn_name(db: &Db, f: FunctionId) -> String {
    let name = FunctionSignature::of(db, f).name.as_str();
    match facts::impl_adt(db, f) {
        Some(adt) => format!("{}::{name}", adt.name(db).as_str()),
        None => name.to_owned(), // alloc: a message's part
    }
}
