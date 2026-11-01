//! std at the walk's edge (docs/v2/plan/lanes/pure-scripts/design.md, section 5): the functions
//! of the toolchain's crates where the walk stops ([`Leaf`], with what each does) and those whose
//! bodies it walks but whose values the path analysis follows ([`Transfer`]).
//!
//! Each crate's table is a `const` list of paths in the pinned nightly's library
//! (`library/` of this tree, `harness/toolchain`), resolved once per toolchain crate through its
//! def map into a map by [`FunctionId`] ([`crate_models`]). A crate is the toolchain's by its
//! origin, never by a name a user crate could take. A path the crate does not have is left out of
//! its map and named by [`check_resolved`], which the harness and the daemon's first build call:
//! the table is then out of date with the toolchain.

use std::fmt::Write as _;

use base_db::{Crate, CrateOrigin, Db, LangCrateOrigin};
use blaze_hash::HashMap;
use either::Either;
use hir_def::{
    AdtId, FunctionId, HasModule, ImplId, ItemContainerId, Lookup, ModuleDefId, ModuleId, TraitId,
    attrs::AttrFlags,
    expr_store::Body,
    nameres::{ModuleData, crate_def_map},
    signatures::FunctionSignature,
};
use hir_expand::name::Name;
use hir_ty::{
    db::HirDatabase,
    method_resolution::{InherentImpls, TraitImpls},
    next_solver::{SimplifiedType, SolverDefId, TyKind},
};
use intern::Symbol;
use rustc_type_ir::inherent::IntoKind;

use crate::{Abs, Effects, Root, allowed_env, safe_relative};

/// What the walk knows of a toolchain function by its table.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Model {
    /// The walk stops here: what the function does is the leaf's.
    Leaf(Leaf),
    /// The walk goes into the body; the call's value is the transfer's.
    Transfer(Transfer),
}

/// What a function the walk stops at does (design section 5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Leaf {
    /// Reads the variable its first argument names: `env::var`, `env::var_os`.
    EnvVar,
    /// Prints to stdout or stderr: `io::_print`, `io::stdout`, `Write` for them, their locks.
    Print,
    /// Reads the file at its first argument, a package path: `fs::read`, `File::open`.
    ReadFile,
    /// Writes the file or directory at its first argument, an `OUT_DIR` path: `fs::write`,
    /// `File::create`, `fs::create_dir_all`.
    WriteFile,
    /// Reads its first argument, a package path, into its second, an `OUT_DIR` path: `fs::copy`.
    Copy,
    /// Tests whether its first argument, a package path, exists or is a file.
    Probe,
    /// Reads, writes, seeks or closes a file through a handle one of the leaves above opened.
    Handle,
    /// Makes a file handle of a number (`FromRawFd`, `FromRawHandle`): a descriptor the script
    /// was given, or any, which [`Leaf::Handle`] would then read or write. Always refused.
    RawHandle,
    /// Ends the run: `process::exit`, `process::abort`.
    Exit,
    /// Fails the run, which is never kept: the panic entry points.
    Panic,
    /// New or freed memory: the allocator's functions.
    Alloc,
    /// The C library's text for an error number (`strerror_r` on unix): fixed, since Rust never
    /// calls `setlocale` and the C locale's text does not change. Windows' `FormatMessageW`
    /// follows the user's language and is not one.
    ErrorText,
}

/// How a function's value follows from its arguments' (design section 6), by MIR operand index.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Transfer {
    /// The value is the argument's: a conversion between `str`, `String`, `OsStr`, `OsString`,
    /// `Path` and `PathBuf`, a clone, an `unwrap`.
    Same { arg: u8 },
    /// `base` joined with `rel`: `Path::join`.
    Join { base: u8, rel: u8 },
    /// `target` joined with `rel`, written back to `target`: `PathBuf::push`.
    Push { target: u8, rel: u8 },
}

/// The model of `func`: a function of the toolchain's `core`, `alloc` or `std` its crate's table
/// names, by path, by an allocator attribute or by link name.
pub fn model_of(db: &Db, func: FunctionId) -> Option<Model> {
    let krate = func.krate(db);
    toolchain(&krate.data(db).origin)?;
    crate_models(db, krate).by_fn.get(&func).copied()
}

/// What a leaf did, given its arguments' values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Applied {
    /// It is allowed: its effects, the variable it read and the value it returns.
    Done { effects: Effects, env: Option<Symbol>, ret: Abs },
    /// It depends on a parameter of the instance that calls it: apply it again at each call site
    /// with the caller's value.
    Deferred,
    /// It is not pure: what it does, said of the function ("reads the variable `PATH`, ...").
    Refused(String),
}

/// What `leaf` does with `args`, the values of the call's operands in order (a reference's is
/// its referent's). Deferred only when a parameter decides it; a refusal from another argument
/// wins. Monotone: a less known argument never gives a smaller result.
pub fn apply_leaf(leaf: Leaf, args: &[Abs]) -> Applied {
    let done = |effects| Applied::Done { effects, env: None, ret: Abs::Unknown };
    match leaf {
        Leaf::EnvVar => match arg(args, 0) {
            Abs::Const(name) if allowed_env(name.as_str()) => {
                Applied::Done { effects: Effects::NONE, env: Some(name), ret: Abs::Env(name) }
            }
            // alloc: a refusal's text, once per impure script
            Abs::Const(name) => Applied::Refused(format!(
                "reads the variable `{}`, not one cargo gives every build script from its unit",
                name.as_str()
            )),
            Abs::Param(_) => Applied::Deferred,
            // alloc: a refusal's text, once per impure script
            _ => Applied::Refused("reads a variable named by a value not known".to_owned()),
        },
        Leaf::ReadFile => path_leaf(&[(arg(args, 0), Root::Package, Verb::Read)], Effects::READ),
        Leaf::WriteFile => path_leaf(&[(arg(args, 0), Root::OutDir, Verb::Write)], Effects::WRITE),
        Leaf::Copy => path_leaf(
            &[(arg(args, 0), Root::Package, Verb::Read), (arg(args, 1), Root::OutDir, Verb::Write)],
            Effects::READ.union(Effects::WRITE),
        ),
        Leaf::Probe => path_leaf(&[(arg(args, 0), Root::Package, Verb::Probe)], Effects::PROBE),
        Leaf::Print => done(Effects::PRINT),
        Leaf::Exit => done(Effects::EXIT),
        Leaf::Handle | Leaf::Panic | Leaf::Alloc | Leaf::ErrorText => done(Effects::NONE),
        Leaf::RawHandle => {
            // alloc: a refusal's text, once per impure script
            Applied::Refused("makes a file handle of a number".to_owned())
        }
    }
}

/// What a transfer gives: the call's value, and the value written back to an argument's
/// referent, by operand index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Transferred {
    pub ret: Abs,
    pub update: Option<(u8, Abs)>,
}

/// What `transfer` gives for `args`, the values of the call's operands in order.
pub fn apply_transfer(transfer: Transfer, args: &[Abs]) -> Transferred {
    match transfer {
        Transfer::Same { arg: at } => Transferred { ret: arg(args, at), update: None },
        Transfer::Join { base, rel } => {
            Transferred { ret: join(arg(args, base), arg(args, rel)), update: None }
        }
        Transfer::Push { target, rel } => Transferred {
            ret: Abs::Unknown,
            update: Some((target, join(arg(args, target), arg(args, rel)))),
        },
    }
}

/// The byte a leaf is in an instance's identity (design section 4): fixed, whatever the order
/// of [`Leaf`]'s variants.
pub fn tag(leaf: Leaf) -> u8 {
    match leaf {
        Leaf::EnvVar => 1,
        Leaf::Print => 2,
        Leaf::ReadFile => 3,
        Leaf::WriteFile => 4,
        Leaf::Copy => 5,
        Leaf::Probe => 6,
        Leaf::Handle => 7,
        Leaf::Exit => 8,
        Leaf::Panic => 9,
        Leaf::Alloc => 10,
        Leaf::RawHandle => 11,
        Leaf::ErrorText => 12,
    }
}

/// The paths of the tables of `std_crate` and of the `core` and `alloc` it depends on that do
/// not resolve in them, for their platform: empty when the tables match the toolchain. The
/// harness asserts it of the pinned std, and the daemon's first build stops on any.
pub fn check_resolved(db: &Db, std_crate: Crate) -> Vec<&'static str> {
    let mut missing = Vec::with_capacity(4);
    // alloc: one list, once per toolchain, of the paths a stale table misses
    missing.extend_from_slice(&crate_models(db, std_crate).unresolved);
    for lang in [LangCrateOrigin::Core, LangCrateOrigin::Alloc] {
        match home_crate(db, std_crate, lang) {
            Some(krate) => missing.extend_from_slice(&crate_models(db, krate).unresolved),
            None => missing.push(match lang {
                LangCrateOrigin::Core => "core (no dependency of std)",
                _ => "alloc (no dependency of std)",
            }),
        }
    }
    missing
}

/// One toolchain crate's models, by function, and the paths of its table it does not have.
#[derive(Debug, PartialEq, Eq)]
pub struct Models {
    by_fn: HashMap<FunctionId, Model>,
    unresolved: Box<[&'static str]>,
}

/// The models of `krate`, a toolchain crate, built once: its table resolved through its def
/// map, and the foreign functions it declares that are the allocator's or the panic runtime's.
#[blaze_query::tracked(returns(ref))]
pub fn crate_models(db: &Db, krate: Crate) -> Models {
    blaze_mem::frame::task(blaze_mem::Tag::Mir, |_| {
        let Some(home) = toolchain(&krate.data(db).origin) else {
            return Models { by_fn: HashMap::default(), unresolved: Box::default() };
        };
        let table = match home {
            Home::Core => CORE,
            Home::Alloc => ALLOC,
            Home::Std => STD,
        };
        let mut by_fn = HashMap::with_capacity_and_hasher(table.len() * 4, Default::default());
        // alloc: once per toolchain crate, the paths a stale table misses
        let mut unresolved = Vec::new();
        let resolver = Resolver { db, krate };
        let mut found = smallvec::SmallVec::<[FunctionId; 16]>::new();
        for entry in table {
            found.clear();
            resolver.functions(entry.site, &mut found);
            if found.is_empty() {
                if entry.only.applies(db, krate) {
                    unresolved.push(entry.what);
                }
                continue;
            }
            for &func in &found {
                by_fn.entry(func).or_insert(entry.model);
            }
        }
        foreign(db, krate, &mut by_fn);
        Models { by_fn, unresolved: unresolved.into_boxed_slice() }
    })
}

/// The toolchain crate an origin is, if it is one with a table.
fn toolchain(origin: &CrateOrigin) -> Option<Home> {
    match *origin {
        CrateOrigin::Lang(LangCrateOrigin::Core) => Some(Home::Core),
        CrateOrigin::Lang(LangCrateOrigin::Alloc) => Some(Home::Alloc),
        CrateOrigin::Lang(LangCrateOrigin::Std) => Some(Home::Std),
        _ => None,
    }
}

/// `krate` itself when it is `lang`, else its dependency that is.
fn home_crate(db: &Db, krate: Crate, lang: LangCrateOrigin) -> Option<Crate> {
    let is = |krate: Crate| matches!(krate.data(db).origin, CrateOrigin::Lang(it) if it == lang);
    if is(krate) {
        return Some(krate);
    }
    krate.data(db).dependencies.iter().map(|dep| dep.crate_id).find(|&dep| is(dep))
}

fn arg(args: &[Abs], at: u8) -> Abs {
    match args.get(usize::from(at)) {
        Some(Abs::Unset) | None => Abs::Unknown,
        Some(&value) => value,
    }
}

/// `base` joined with `rel`: a safe relative constant keeps the base's root, or keeps a
/// parameter's path under it; an absolute path (a variable's that is a root, or `OUT_DIR`'s) is
/// its own; a package path whose base is in the package stays there, relative or not; anything
/// else is not known.
fn join(base: Abs, rel: Abs) -> Abs {
    match rel {
        Abs::Const(text) if safe_relative(text.as_str()) => match base {
            Abs::Param(i) | Abs::Under(i) => Abs::Under(i),
            _ => base.as_path().map_or(Abs::Unknown, Abs::Root),
        },
        Abs::Env(_) => rel.as_path().map_or(Abs::Unknown, Abs::Root),
        Abs::Root(Root::OutDir) => Abs::Root(Root::OutDir),
        Abs::Root(Root::Package) if base.as_path() == Some(Root::Package) => Abs::Root(Root::Package),
        _ => Abs::Unknown,
    }
}

#[derive(Clone, Copy)]
enum Verb {
    Read,
    Write,
    Probe,
}

/// A leaf whose arguments are paths, each under the root it needs.
fn path_leaf(paths: &[(Abs, Root, Verb)], effects: Effects) -> Applied {
    let mut deferred = false;
    for &(value, want, verb) in paths {
        match value {
            Abs::Param(_) | Abs::Under(_) => deferred = true,
            _ if value.as_path() == Some(want) => {}
            _ => return Applied::Refused(refusal(value, want, verb)),
        }
    }
    if deferred { Applied::Deferred } else { Applied::Done { effects, env: None, ret: Abs::Unknown } }
}

/// What a leaf of `verb` does at `value`, which is not under `want`.
fn refusal(value: Abs, want: Root, verb: Verb) -> String {
    let verb = match verb {
        Verb::Read => "reads",
        Verb::Write => "writes",
        Verb::Probe => "tests whether there is a file at",
    };
    let wanted = match want {
        Root::Package => "inside its package",
        Root::OutDir => "inside its OUT_DIR",
    };
    // alloc: a refusal's text, once per impure script
    let mut said = String::with_capacity(96);
    let _ = match (value, value.as_path()) {
        (Abs::Const(text), None) => write!(said, "{verb} `{}`, not a path {wanted}", text.as_str()),
        (Abs::Const(text), Some(_)) => {
            write!(said, "{verb} `{}`, a path in its package, not {wanted}", text.as_str())
        }
        (_, Some(Root::Package)) => write!(said, "{verb} a path in its package, not {wanted}"),
        (_, Some(Root::OutDir)) => write!(said, "{verb} a path in its OUT_DIR, not {wanted}"),
        (Abs::Env(name), None) => write!(said, "{verb} the path the variable `{}` gives", name.as_str()),
        _ => write!(said, "{verb} a path not known to be {wanted}"),
    };
    said
}

/// Which toolchain crate a table or a path is of.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Home {
    Core,
    Alloc,
    Std,
}

impl Home {
    fn lang(self) -> LangCrateOrigin {
        match self {
            Home::Core => LangCrateOrigin::Core,
            Home::Alloc => LangCrateOrigin::Alloc,
            Home::Std => LangCrateOrigin::Std,
        }
    }
}

type Path = &'static [&'static str];

/// The type an impl is for, with its references taken off: `File` is also `&File`, `str` also
/// `&&str`.
#[derive(Clone, Copy)]
enum SelfTy {
    Adt(Home, Path),
    Str,
}

/// Where a table entry's functions are.
#[derive(Clone, Copy)]
enum Site {
    /// A free function of the table's crate, by path (re-exports followed).
    Fn(Path),
    /// The inherent method `name` of the type at `ty`, in the table's crate.
    Inherent { ty: Path, name: &'static str },
    /// The methods named in `methods` (every one when it is empty) of each impl, in the table's
    /// crate, of the trait at `trait_` for `ty`.
    Impls { trait_: (Home, Path), ty: SelfTy, methods: &'static [&'static str] },
    /// The foreign function `name` (its Rust name, whatever it links to) declared in a block of
    /// the body of the free function at `func`.
    ForeignIn { func: Path, name: &'static str },
}

/// On which platforms an entry must resolve.
#[derive(Clone, Copy)]
enum Only {
    All,
    /// `target_family = "unix"`: std's `os::fd`.
    Unix,
    /// `target_family = "windows"`: std's `os::windows`.
    Windows,
}

impl Only {
    fn applies(self, db: &Db, krate: Crate) -> bool {
        let family = |want: &str| {
            krate.cfg_options(db).get_cfg_values("target_family").any(|it| it.as_str() == want)
        };
        match self {
            Only::All => true,
            Only::Unix => family("unix"),
            Only::Windows => family("windows"),
        }
    }
}

struct Entry {
    /// The path as said to a person: `std::env::var`, `<std::fs::File as Read>`.
    what: &'static str,
    site: Site,
    model: Model,
    only: Only,
}

const fn func(what: &'static str, path: Path, model: Model) -> Entry {
    Entry { what, site: Site::Fn(path), model, only: Only::All }
}

const fn method(what: &'static str, ty: Path, name: &'static str, model: Model) -> Entry {
    Entry { what, site: Site::Inherent { ty, name }, model, only: Only::All }
}

const fn impls(
    what: &'static str,
    trait_: (Home, Path),
    ty: SelfTy,
    methods: &'static [&'static str],
    model: Model,
) -> Entry {
    Entry { what, site: Site::Impls { trait_, ty, methods }, model, only: Only::All }
}

const fn foreign_in(what: &'static str, func: Path, name: &'static str, model: Model) -> Entry {
    Entry { what, site: Site::ForeignIn { func, name }, model, only: Only::All }
}

const fn on(only: Only, entry: Entry) -> Entry {
    Entry { only, ..entry }
}

const ENV: Model = Model::Leaf(Leaf::EnvVar);
const PRINT: Model = Model::Leaf(Leaf::Print);
const READ: Model = Model::Leaf(Leaf::ReadFile);
const WRITE: Model = Model::Leaf(Leaf::WriteFile);
const COPY: Model = Model::Leaf(Leaf::Copy);
const PROBE: Model = Model::Leaf(Leaf::Probe);
const HANDLE: Model = Model::Leaf(Leaf::Handle);
const RAW: Model = Model::Leaf(Leaf::RawHandle);
const EXIT: Model = Model::Leaf(Leaf::Exit);
const PANIC: Model = Model::Leaf(Leaf::Panic);
const ALLOC_LEAF: Model = Model::Leaf(Leaf::Alloc);
const ERROR_TEXT: Model = Model::Leaf(Leaf::ErrorText);
const SAME: Model = Model::Transfer(Transfer::Same { arg: 0 });
const JOIN: Model = Model::Transfer(Transfer::Join { base: 0, rel: 1 });
const PUSH: Model = Model::Transfer(Transfer::Push { target: 0, rel: 1 });

const ALL: &[&str] = &[];

const STD_WRITE: (Home, Path) = (Home::Std, &["io", "Write"]);
const STD_READ: (Home, Path) = (Home::Std, &["io", "Read"]);
const STD_SEEK: (Home, Path) = (Home::Std, &["io", "Seek"]);
const DROP: (Home, Path) = (Home::Core, &["ops", "Drop"]);
const FROM: (Home, Path) = (Home::Core, &["convert", "From"]);
const AS_REF: (Home, Path) = (Home::Core, &["convert", "AsRef"]);
const DEREF: (Home, Path) = (Home::Core, &["ops", "Deref"]);
const BORROW: (Home, Path) = (Home::Core, &["borrow", "Borrow"]);
const CLONE: (Home, Path) = (Home::Core, &["clone", "Clone"]);
const TRY: (Home, Path) = (Home::Core, &["ops", "Try"]);
const TO_OWNED: (Home, Path) = (Home::Alloc, &["borrow", "ToOwned"]);
const SPEC_TO_STRING: (Home, Path) = (Home::Alloc, &["string", "SpecToString"]);

const FILE: SelfTy = SelfTy::Adt(Home::Std, &["fs", "File"]);
const PATH: SelfTy = SelfTy::Adt(Home::Std, &["path", "Path"]);
const PATH_BUF: SelfTy = SelfTy::Adt(Home::Std, &["path", "PathBuf"]);
const OS_STR: SelfTy = SelfTy::Adt(Home::Std, &["ffi", "OsStr"]);
const OS_STRING: SelfTy = SelfTy::Adt(Home::Std, &["ffi", "OsString"]);
const STRING: SelfTy = SelfTy::Adt(Home::Alloc, &["string", "String"]);
const OWNED_FD: SelfTy = SelfTy::Adt(Home::Std, &["os", "fd", "OwnedFd"]);
const OWNED_HANDLE: SelfTy = SelfTy::Adt(Home::Std, &["os", "windows", "io", "OwnedHandle"]);

/// std's table: library/std/src at the pinned nightly.
const STD: &[Entry] = &[
    func("std::env::var", &["env", "var"], ENV),
    func("std::env::var_os", &["env", "var_os"], ENV),
    func("std::io::_print", &["io", "_print"], PRINT),
    func("std::io::_eprint", &["io", "_eprint"], PRINT),
    func("std::io::stdout", &["io", "stdout"], PRINT),
    func("std::io::stderr", &["io", "stderr"], PRINT),
    method("std::io::Stdout::lock", &["io", "Stdout"], "lock", PRINT),
    method("std::io::Stderr::lock", &["io", "Stderr"], "lock", PRINT),
    impls("<std::io::Stdout as Write>", STD_WRITE, SelfTy::Adt(Home::Std, &["io", "Stdout"]), ALL, PRINT),
    impls("<std::io::StdoutLock as Write>", STD_WRITE, SelfTy::Adt(Home::Std, &["io", "StdoutLock"]), ALL, PRINT),
    impls("<std::io::Stderr as Write>", STD_WRITE, SelfTy::Adt(Home::Std, &["io", "Stderr"]), ALL, PRINT),
    impls("<std::io::StderrLock as Write>", STD_WRITE, SelfTy::Adt(Home::Std, &["io", "StderrLock"]), ALL, PRINT),
    // The guard inside `StdoutLock` and `StderrLock`: their drop glue releases it.
    impls(
        "<std::sync::ReentrantLockGuard as Drop>",
        DROP,
        SelfTy::Adt(Home::Std, &["sync", "ReentrantLockGuard"]),
        ALL,
        PRINT,
    ),
    func("std::fs::read", &["fs", "read"], READ),
    func("std::fs::read_to_string", &["fs", "read_to_string"], READ),
    method("std::fs::File::open", &["fs", "File"], "open", READ),
    func("std::fs::write", &["fs", "write"], WRITE),
    method("std::fs::File::create", &["fs", "File"], "create", WRITE),
    func("std::fs::create_dir", &["fs", "create_dir"], WRITE),
    func("std::fs::create_dir_all", &["fs", "create_dir_all"], WRITE),
    func("std::fs::copy", &["fs", "copy"], COPY),
    method("std::path::Path::exists", &["path", "Path"], "exists", PROBE),
    method("std::path::Path::is_file", &["path", "Path"], "is_file", PROBE),
    func("std::fs::exists", &["fs", "exists"], PROBE),
    impls("<std::fs::File as Read>", STD_READ, FILE, ALL, HANDLE),
    impls("<std::fs::File as Write>", STD_WRITE, FILE, ALL, HANDLE),
    impls("<std::fs::File as Seek>", STD_SEEK, FILE, ALL, HANDLE),
    on(Only::Unix, impls("<std::os::fd::OwnedFd as Drop>", DROP, OWNED_FD, ALL, HANDLE)),
    on(Only::Windows, impls("<std::os::windows::io::OwnedHandle as Drop>", DROP, OWNED_HANDLE, ALL, HANDLE)),
    on(
        Only::Unix,
        impls("<std::os::fd::OwnedFd as FromRawFd>", (Home::Std, &["os", "fd", "FromRawFd"]), OWNED_FD, ALL, RAW),
    ),
    on(Only::Unix, impls("<std::fs::File as FromRawFd>", (Home::Std, &["os", "fd", "FromRawFd"]), FILE, ALL, RAW)),
    on(
        Only::Windows,
        impls(
            "<std::os::windows::io::OwnedHandle as FromRawHandle>",
            (Home::Std, &["os", "windows", "io", "FromRawHandle"]),
            OWNED_HANDLE,
            ALL,
            RAW,
        ),
    ),
    on(
        Only::Windows,
        impls(
            "<std::fs::File as FromRawHandle>",
            (Home::Std, &["os", "windows", "io", "FromRawHandle"]),
            FILE,
            ALL,
            RAW,
        ),
    ),
    // `io::Error`'s `Display` and `Debug` text, declared inside `format_error`'s body and linked
    // as `__xpg_strerror_r` on glibc.
    on(
        Only::Unix,
        foreign_in(
            "strerror_r in std::sys::io::format_error",
            &["sys", "io", "format_error"],
            "strerror_r",
            ERROR_TEXT,
        ),
    ),
    func("std::process::exit", &["process", "exit"], EXIT),
    func("std::process::abort", &["process", "abort"], EXIT),
    func("std::panicking::begin_panic", &["panicking", "begin_panic"], PANIC),
    func("std::panicking::panic_handler", &["panicking", "panic_handler"], PANIC),
    func("std::panicking::panic_with_hook", &["panicking", "panic_with_hook"], PANIC),
    func("std::panicking::resume_unwind", &["panicking", "resume_unwind"], PANIC),
    func("std::panicking::rust_panic", &["panicking", "rust_panic"], PANIC),
    func("std::panic::panic_any", &["panic", "panic_any"], PANIC),
    func("std::panic::resume_unwind", &["panic", "resume_unwind"], PANIC),
    method("std::path::Path::new", &["path", "Path"], "new", SAME),
    method("std::path::Path::join", &["path", "Path"], "join", JOIN),
    method("std::path::Path::to_path_buf", &["path", "Path"], "to_path_buf", SAME),
    method("std::path::Path::as_os_str", &["path", "Path"], "as_os_str", SAME),
    method("std::path::Path::to_str", &["path", "Path"], "to_str", SAME),
    method("std::path::PathBuf::push", &["path", "PathBuf"], "push", PUSH),
    method("std::path::PathBuf::as_path", &["path", "PathBuf"], "as_path", SAME),
    method("std::path::PathBuf::into_os_string", &["path", "PathBuf"], "into_os_string", SAME),
    method("std::ffi::OsStr::to_str", &["ffi", "OsStr"], "to_str", SAME),
    method("std::ffi::OsStr::to_os_string", &["ffi", "OsStr"], "to_os_string", SAME),
    method("std::ffi::OsString::as_os_str", &["ffi", "OsString"], "as_os_str", SAME),
    method("std::ffi::OsString::into_string", &["ffi", "OsString"], "into_string", SAME),
    impls("<std::path::PathBuf as From>", FROM, PATH_BUF, &["from"], SAME),
    impls("<std::ffi::OsString as From>", FROM, OS_STRING, &["from"], SAME),
    impls("<str as AsRef> in std", AS_REF, SelfTy::Str, &["as_ref"], SAME),
    impls("<String as AsRef> in std", AS_REF, STRING, &["as_ref"], SAME),
    impls("<std::path::Path as AsRef>", AS_REF, PATH, &["as_ref"], SAME),
    impls("<std::path::PathBuf as AsRef>", AS_REF, PATH_BUF, &["as_ref"], SAME),
    impls("<std::ffi::OsStr as AsRef>", AS_REF, OS_STR, &["as_ref"], SAME),
    impls("<std::ffi::OsString as AsRef>", AS_REF, OS_STRING, &["as_ref"], SAME),
    impls("<std::path::PathBuf as Deref>", DEREF, PATH_BUF, &["deref"], SAME),
    impls("<std::ffi::OsString as Deref>", DEREF, OS_STRING, &["deref"], SAME),
    impls("<std::path::PathBuf as Borrow>", BORROW, PATH_BUF, &["borrow"], SAME),
    impls("<std::ffi::OsString as Borrow>", BORROW, OS_STRING, &["borrow"], SAME),
    impls("<std::path::Path as ToOwned>", TO_OWNED, PATH, &["to_owned"], SAME),
    impls("<std::ffi::OsStr as ToOwned>", TO_OWNED, OS_STR, &["to_owned"], SAME),
    impls("<std::path::PathBuf as Clone>", CLONE, PATH_BUF, &["clone"], SAME),
    impls("<std::ffi::OsString as Clone>", CLONE, OS_STRING, &["clone"], SAME),
];

/// alloc's table: library/alloc/src at the pinned nightly.
const ALLOC: &[Entry] = &[
    func("alloc::alloc::__rust_alloc", &["alloc", "__rust_alloc"], ALLOC_LEAF),
    func("alloc::alloc::__rust_dealloc", &["alloc", "__rust_dealloc"], ALLOC_LEAF),
    func("alloc::alloc::__rust_realloc", &["alloc", "__rust_realloc"], ALLOC_LEAF),
    func("alloc::alloc::__rust_alloc_zeroed", &["alloc", "__rust_alloc_zeroed"], ALLOC_LEAF),
    func(
        "alloc::alloc::__rust_no_alloc_shim_is_unstable_v2",
        &["alloc", "__rust_no_alloc_shim_is_unstable_v2"],
        ALLOC_LEAF,
    ),
    // std's default handler prints and aborts: the run fails.
    func("alloc::alloc::__rust_alloc_error_handler", &["alloc", "__rust_alloc_error_handler"], PANIC),
    method("alloc::string::String::as_str", &["string", "String"], "as_str", SAME),
    impls("<String as From>", FROM, STRING, &["from"], SAME),
    impls("<String as AsRef>", AS_REF, STRING, &["as_ref"], SAME),
    impls("<String as Deref>", DEREF, STRING, &["deref"], SAME),
    impls("<String as Borrow>", BORROW, STRING, &["borrow"], SAME),
    impls("<String as Clone>", CLONE, STRING, &["clone"], SAME),
    impls("<str as ToOwned>", TO_OWNED, SelfTy::Str, &["to_owned"], SAME),
    impls("<str as SpecToString>", SPEC_TO_STRING, SelfTy::Str, &["spec_to_string"], SAME),
    impls("<String as SpecToString>", SPEC_TO_STRING, STRING, &["spec_to_string"], SAME),
    impls(
        "<Cow<str> as SpecToString>",
        SPEC_TO_STRING,
        SelfTy::Adt(Home::Alloc, &["borrow", "Cow"]),
        &["spec_to_string"],
        SAME,
    ),
];

/// core's table: library/core/src at the pinned nightly.
const CORE: &[Entry] = &[
    func("core::panicking::panic_fmt", &["panicking", "panic_fmt"], PANIC),
    func("core::panicking::panic_nounwind_fmt", &["panicking", "panic_nounwind_fmt"], PANIC),
    func("core::panicking::panic", &["panicking", "panic"], PANIC),
    func("core::panicking::panic_nounwind", &["panicking", "panic_nounwind"], PANIC),
    func("core::panicking::panic_nounwind_nobacktrace", &["panicking", "panic_nounwind_nobacktrace"], PANIC),
    func("core::panicking::unreachable_display", &["panicking", "unreachable_display"], PANIC),
    func("core::panicking::panic_str_2015", &["panicking", "panic_str_2015"], PANIC),
    func("core::panicking::panic_display", &["panicking", "panic_display"], PANIC),
    func("core::panicking::panic_bounds_check", &["panicking", "panic_bounds_check"], PANIC),
    func(
        "core::panicking::panic_misaligned_pointer_dereference",
        &["panicking", "panic_misaligned_pointer_dereference"],
        PANIC,
    ),
    func(
        "core::panicking::panic_null_pointer_dereference",
        &["panicking", "panic_null_pointer_dereference"],
        PANIC,
    ),
    func(
        "core::panicking::panic_null_reference_constructed",
        &["panicking", "panic_null_reference_constructed"],
        PANIC,
    ),
    func(
        "core::panicking::panic_invalid_enum_construction",
        &["panicking", "panic_invalid_enum_construction"],
        PANIC,
    ),
    func("core::panicking::panic_cannot_unwind", &["panicking", "panic_cannot_unwind"], PANIC),
    func("core::panicking::panic_in_cleanup", &["panicking", "panic_in_cleanup"], PANIC),
    func("core::panicking::const_panic_fmt", &["panicking", "const_panic_fmt"], PANIC),
    func("core::panicking::assert_failed", &["panicking", "assert_failed"], PANIC),
    func("core::panicking::assert_matches_failed", &["panicking", "assert_matches_failed"], PANIC),
    func("core::panicking::assert_failed_inner", &["panicking", "assert_failed_inner"], PANIC),
    func("core::panicking::panic_default_msg", &["panicking", "panic_default_msg"], PANIC),
    func("core::panicking::unreachable_default_msg", &["panicking", "unreachable_default_msg"], PANIC),
    func("core::panicking::unimplemented_default_msg", &["panicking", "unimplemented_default_msg"], PANIC),
    func("core::panicking::todo_default_msg", &["panicking", "todo_default_msg"], PANIC),
    func("core::panicking::panic_const::panic_const_add_overflow", &["panicking", "panic_const", "panic_const_add_overflow"], PANIC),
    func("core::panicking::panic_const::panic_const_sub_overflow", &["panicking", "panic_const", "panic_const_sub_overflow"], PANIC),
    func("core::panicking::panic_const::panic_const_mul_overflow", &["panicking", "panic_const", "panic_const_mul_overflow"], PANIC),
    func("core::panicking::panic_const::panic_const_div_overflow", &["panicking", "panic_const", "panic_const_div_overflow"], PANIC),
    func("core::panicking::panic_const::panic_const_rem_overflow", &["panicking", "panic_const", "panic_const_rem_overflow"], PANIC),
    func("core::panicking::panic_const::panic_const_neg_overflow", &["panicking", "panic_const", "panic_const_neg_overflow"], PANIC),
    func("core::panicking::panic_const::panic_const_shr_overflow", &["panicking", "panic_const", "panic_const_shr_overflow"], PANIC),
    func("core::panicking::panic_const::panic_const_shl_overflow", &["panicking", "panic_const", "panic_const_shl_overflow"], PANIC),
    func("core::panicking::panic_const::panic_const_div_by_zero", &["panicking", "panic_const", "panic_const_div_by_zero"], PANIC),
    func("core::panicking::panic_const::panic_const_rem_by_zero", &["panicking", "panic_const", "panic_const_rem_by_zero"], PANIC),
    func("core::panicking::panic_const::panic_const_coroutine_resumed", &["panicking", "panic_const", "panic_const_coroutine_resumed"], PANIC),
    func("core::panicking::panic_const::panic_const_async_fn_resumed", &["panicking", "panic_const", "panic_const_async_fn_resumed"], PANIC),
    func("core::panicking::panic_const::panic_const_async_gen_fn_resumed", &["panicking", "panic_const", "panic_const_async_gen_fn_resumed"], PANIC),
    func("core::panicking::panic_const::panic_const_gen_fn_none", &["panicking", "panic_const", "panic_const_gen_fn_none"], PANIC),
    func("core::panicking::panic_const::panic_const_coroutine_resumed_panic", &["panicking", "panic_const", "panic_const_coroutine_resumed_panic"], PANIC),
    func("core::panicking::panic_const::panic_const_async_fn_resumed_panic", &["panicking", "panic_const", "panic_const_async_fn_resumed_panic"], PANIC),
    func("core::panicking::panic_const::panic_const_async_gen_fn_resumed_panic", &["panicking", "panic_const", "panic_const_async_gen_fn_resumed_panic"], PANIC),
    func("core::panicking::panic_const::panic_const_gen_fn_none_panic", &["panicking", "panic_const", "panic_const_gen_fn_none_panic"], PANIC),
    func("core::panicking::panic_const::panic_const_coroutine_resumed_drop", &["panicking", "panic_const", "panic_const_coroutine_resumed_drop"], PANIC),
    func("core::panicking::panic_const::panic_const_async_fn_resumed_drop", &["panicking", "panic_const", "panic_const_async_fn_resumed_drop"], PANIC),
    func("core::panicking::panic_const::panic_const_async_gen_fn_resumed_drop", &["panicking", "panic_const", "panic_const_async_gen_fn_resumed_drop"], PANIC),
    func("core::panicking::panic_const::panic_const_gen_fn_none_drop", &["panicking", "panic_const", "panic_const_gen_fn_none_drop"], PANIC),
    method("core::result::Result::unwrap", &["result", "Result"], "unwrap", SAME),
    method("core::result::Result::expect", &["result", "Result"], "expect", SAME),
    method("core::option::Option::unwrap", &["option", "Option"], "unwrap", SAME),
    method("core::option::Option::expect", &["option", "Option"], "expect", SAME),
    impls(
        "<core::result::Result as Try>",
        TRY,
        SelfTy::Adt(Home::Core, &["result", "Result"]),
        &["branch", "from_output"],
        SAME,
    ),
    impls(
        "<core::option::Option as Try>",
        TRY,
        SelfTy::Adt(Home::Core, &["option", "Option"]),
        &["branch", "from_output"],
        SAME,
    ),
    impls("<str as AsRef> in core", AS_REF, SelfTy::Str, &["as_ref"], SAME),
    // What `unwrap` and `expect` call on failure: the run fails, so nothing past them is kept.
    func("core::result::unwrap_failed", &["result", "unwrap_failed"], PANIC),
    func("core::option::unwrap_failed", &["option", "unwrap_failed"], PANIC),
    func("core::option::expect_failed", &["option", "expect_failed"], PANIC),
];

/// The foreign functions `krate` declares that are the allocator's (by attribute or link name)
/// or the panic runtime's (by link name), into `by_fn`.
fn foreign(db: &Db, krate: Crate, by_fn: &mut HashMap<FunctionId, Model>) {
    let allocator = AttrFlags::RUSTC_ALLOCATOR
        | AttrFlags::RUSTC_DEALLOCATOR
        | AttrFlags::RUSTC_REALLOCATOR
        | AttrFlags::RUSTC_ALLOCATOR_ZEROED;
    for (_, module) in crate_def_map(db, krate).modules(db) {
        let scope = module.scope(db);
        if scope.extern_blocks().next().is_none() {
            continue;
        }
        for def in scope.declarations() {
            let ModuleDefId::FunctionId(func) = def else { continue };
            if !matches!(func.lookup(db).container, ItemContainerId::ExternBlockId(_)) {
                continue;
            }
            let model = if AttrFlags::query(db, func.into()).intersects(allocator) {
                Some(ALLOC_LEAF)
            } else {
                let link = AttrFlags::link_name(db, func.into());
                let name = &FunctionSignature::of(db, func).name;
                let symbol = match &link {
                    Some(link) => link.as_str(),
                    None => name.as_str(),
                };
                SYMBOLS.iter().find(|&&(it, _)| it == symbol).map(|&(_, model)| model)
            };
            if let Some(model) = model {
                by_fn.entry(func).or_insert(model);
            }
        }
    }
}

/// The symbols of the allocator shim and the panic runtime the toolchain's crates declare.
const SYMBOLS: &[(&str, Model)] = &[
    ("__rust_alloc", ALLOC_LEAF),
    ("__rust_dealloc", ALLOC_LEAF),
    ("__rust_realloc", ALLOC_LEAF),
    ("__rust_alloc_zeroed", ALLOC_LEAF),
    ("__rust_no_alloc_shim_is_unstable_v2", ALLOC_LEAF),
    ("__rust_alloc_error_handler", PANIC),
    ("__rust_start_panic", PANIC),
    ("rust_begin_unwind", PANIC),
];

/// Resolves a table's sites in its crate.
struct Resolver<'db> {
    db: &'db Db,
    krate: Crate,
}

impl<'db> Resolver<'db> {
    /// The functions of `site`, into `found`; none when it does not resolve.
    fn functions(&self, site: Site, found: &mut smallvec::SmallVec<[FunctionId; 16]>) {
        let db = self.db;
        match site {
            Site::Fn(path) => {
                if let Some(ModuleDefId::FunctionId(func)) = self.item(self.krate, path, false)
                    && func.krate(db) == self.krate
                {
                    found.push(func);
                }
            }
            Site::Inherent { ty, name } => {
                let Some(ModuleDefId::AdtId(adt)) = self.item(self.krate, ty, true) else { return };
                let name = Symbol::intern(name);
                let self_ty = SimplifiedType::Adt(SolverDefId::AdtId(adt));
                for &imp in InherentImpls::for_crate(db, self.krate).for_self_ty(&self_ty) {
                    methods(db, imp, std::slice::from_ref(&name), found);
                }
            }
            Site::Impls { trait_: (home, trait_path), ty, methods: names } => {
                let Some(trait_) = self.trait_at(home, trait_path) else { return };
                let want = match ty {
                    SelfTy::Adt(home, path) => match self.at(home, path) {
                        Some(ModuleDefId::AdtId(adt)) => Some(adt),
                        _ => return,
                    },
                    SelfTy::Str => None,
                };
                // alloc: setup, once per toolchain crate's table entry
                let names: smallvec::SmallVec<[Symbol; 2]> = names.iter().map(|it| Symbol::intern(it)).collect();
                TraitImpls::for_crate(db, self.krate).for_trait(trait_, |impls| {
                    let Either::Left(impls) = impls else { return };
                    for &imp in impls {
                        if self_ty_is(db, imp, want) {
                            methods(db, imp, &names, found);
                        }
                    }
                });
            }
            Site::ForeignIn { func, name } => {
                let Some(ModuleDefId::FunctionId(owner)) = self.item(self.krate, func, false) else {
                    return;
                };
                let name = Symbol::intern(name);
                for (_, block) in Body::of(db, owner.into()).blocks(db) {
                    for (_, module) in block.modules(db) {
                        for def in module.scope(db).declarations() {
                            if let ModuleDefId::FunctionId(it) = def
                                && matches!(it.lookup(db).container, ItemContainerId::ExternBlockId(_))
                                && *FunctionSignature::of(db, it).name.symbol() == name
                            {
                                found.push(it);
                            }
                        }
                    }
                }
            }
        }
    }

    fn trait_at(&self, home: Home, path: Path) -> Option<TraitId> {
        match self.at(home, path)? {
            ModuleDefId::TraitId(it) => Some(it),
            _ => None,
        }
    }

    /// The item at `path` in the toolchain crate `home`, in the type namespace.
    fn at(&self, home: Home, path: Path) -> Option<ModuleDefId> {
        let krate = home_crate(self.db, self.krate, home.lang())?;
        self.item(krate, path, true)
    }

    /// The item at `path` in `krate`, in the type namespace or the value namespace.
    fn item(&self, krate: Crate, path: Path, types: bool) -> Option<ModuleDefId> {
        let (last, modules) = path.split_last()?;
        let module = self.module(krate, modules)?;
        let per_ns = data(self.db, module).name_in_scope(self.db, &name_of(last));
        if types { per_ns.take_types() } else { per_ns.take_values() }
    }

    /// The module at `path` in `krate`, through re-exports.
    fn module(&self, krate: Crate, path: &[&'static str]) -> Option<ModuleId> {
        let mut module = crate_def_map(self.db, krate).root_module_id();
        for segment in path {
            match data(self.db, module).name_in_scope(self.db, &name_of(segment)).take_types()? {
                ModuleDefId::ModuleId(it) => module = it,
                _ => return None,
            }
        }
        Some(module)
    }
}

fn data(db: &Db, module: ModuleId) -> &ModuleData {
    module.to_db(db).def_map(db).module(db, module)
}

fn name_of(segment: &str) -> Name {
    Name::new_symbol_root(Symbol::intern(segment))
}

/// Whether `imp` is for the ADT `want` (`str` when `None`), its references taken off.
fn self_ty_is(db: &Db, imp: ImplId, want: Option<AdtId>) -> bool {
    let mut ty = db.impl_self_ty(imp).skip_binder();
    while let TyKind::Ref(_, inner, _) = ty.kind() {
        ty = inner;
    }
    match (ty.kind(), want) {
        (TyKind::Adt(def, _), Some(adt)) => def.def_id() == adt,
        (TyKind::Str, None) => true,
        _ => false,
    }
}

/// The methods of `imp` named in `names` (every one when it is empty), into `found`.
fn methods(db: &Db, imp: ImplId, names: &[Symbol], found: &mut smallvec::SmallVec<[FunctionId; 16]>) {
    for (name, item) in imp.impl_items(db).items.iter() {
        if let hir_def::AssocItemId::FunctionId(func) = *item
            && (names.is_empty() || names.contains(name.symbol()))
        {
            found.push(func);
        }
    }
}
