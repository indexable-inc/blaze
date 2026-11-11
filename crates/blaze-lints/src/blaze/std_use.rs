//! `std_use`: a use of an item of Rust's standard library that blaze's own standard library
//! (bstd, docs/v2/plan/design/std.md) replaces or forbids, naming what stands in its place.
//!
//! What is replaced is one table, [`TABLE`]: data, a row per group of items, each with where
//! std defines it, its [`Kind`], why it goes and what is written instead. bstd's API is still
//! moving, so a change of mind there is a change of a row here and of the same row of
//! docs/lints.md's table, which a test holds to this one (`Entry::docs_row`).
//!
//! The kinds:
//!
//! - **ambient authority** ([`Kind::Ambient`]): the file system, the environment, processes,
//!   the network, the clock, the standard streams, threads, thread-locals, `static mut` and
//!   randomness, reached from nowhere. A capability or a value someone owns replaces each.
//! - **hidden cost** ([`Kind::Hidden`]): a call that decides something costly without an
//!   argument for it: durability (`fs::write`, `fs::rename`, `File::sync_all`), a buffer lost
//!   on drop (`BufWriter`), a wait with no deadline (`thread::park`), a lock that poisons.
//! - **global-heap allocation** ([`Kind::Alloc`]): `Vec::new`, `Box::new`, `String::from`,
//!   `format!`, `vec!` and the like. `blaze::hot_alloc` and `blaze::implicit_allocator`
//!   already find these, so each such row names them ([`Entry::covered_by`]) and this lint
//!   says nothing of the row wherever one of them is on.
//! - **renamed** ([`Kind::Renamed`]): a type bstd re-exports under its one name (`Arc` is
//!   `bstd::sync::Shared`). The two names are one definition, so resolution cannot tell them
//!   apart: such a row counts a path only where the type's segment is written by std's name,
//!   and those are the rows with a machine-applicable fix.
//!
//! What bstd re-exports unchanged (`UNCHANGED`: `core`'s arithmetic, iterators, `Option`,
//! `Result`, slices, `str`) has no row and is never found.
//!
//! A use is found by what it resolves to, never by its text: a path that names a function
//! (`std::fs::read(p)`, `.map(std::fs::read)`, `File::open(p)`), through a `use`, an alias, a
//! glob or a re-export; a method call, by the function it resolves to (`file.sync_all()`); a
//! trait's method, by the type it is called on (`file.write_all(..)` is `std::fs::File`'s,
//! `String::from(..)` is `String`'s); and a macro, by what its expansion calls (`println!` and
//! `print!` are `std::io::_print`, `format!` is `alloc::fmt::format`) or, for `thread_local!`,
//! by the crate that defines it. [`uses_in`] is the one walk; `blaze::build_script_ambient` is the same walk
//! and the same table over a build script, its rows those with an [`Authority`], reported once
//! per script.
//!
//! One finding per distinct item per body (a free function, the type for a type's functions,
//! or the function for a row that names functions of a type), at its first use, with the
//! count of its uses in that body. A `static mut` and a `thread_local!` are found where they
//! are declared.
//!
//! Not found: a build script (that is `build_script_ambient`'s); test code (`#[cfg(test)]`,
//! `#[test]`), which drives the code from outside; a type only named in a signature or a
//! field (its value came from a path that is found); a renamed type's path inside a macro
//! call's arguments, whose text the walk does not have, or under a name a `use .. as` gave it; `collect()`, which allocates or not
//! by its target type (`hot_alloc` follows it); `AtomicPtr`, whose bstd form needs the pointee
//! written.

use base_db::{Db, TargetKind};
use blaze_diagnostics::lints::registry::ready;
use hir_def::{CallableDefId, FunctionId, hir::Expr};
use hir_ty::next_solver::{GenericArgs, TyKind};
use rustc_type_ir::inherent::IntoKind as _;
use syntax::{NodeOrToken, TextRange, TextSize, view as ast};

use ast::{AstNode as _, HasName as _};

use super::{Lint, alloc, facts};
use crate::{check::Cx, diag::Applicability};

const LINT: Lint = Lint::StdUse;

/// The crate a fix's paths start at: bstd's library name (crates/blaze-std). A fix is offered
/// only in a crate that has a dependency of this name, so it never writes a path that does
/// not resolve.
pub(crate) const BSTD: &str = "bstd";

/// What a finding says while bstd is not in this blaze ([`ready::BSTD_IN_TREE`]).
const PLANNED: &str = "blaze's standard library (bstd) is planned and not in this blaze: this \
                       is where the code is going, and there is nothing to change yet";

/// Why an item of std has a row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Ambient,
    Hidden,
    Alloc,
    Renamed,
}

impl Kind {
    #[cfg(test)]
    pub(crate) const ALL: [Kind; 4] = [Kind::Ambient, Kind::Hidden, Kind::Alloc, Kind::Renamed];

    /// As a finding's first line and docs/lints.md's table name it; harness/v2/std_uses.py
    /// reads it from the message.
    pub(crate) const fn say(self) -> &'static str {
        match self {
            Kind::Ambient => "ambient authority",
            Kind::Hidden => "hidden cost",
            Kind::Alloc => "global-heap allocation",
            Kind::Renamed => "renamed",
        }
    }

    /// What every finding of the kind says after its first line.
    const fn note(self) -> &'static str {
        match self {
            Kind::Ambient => {
                "with no capability, code is pure computation: what a function can reach \
                 outside its own values is handed to it, so its signature is the whole list"
            }
            Kind::Hidden => {
                "the call decides something costly that none of its arguments says: bstd's \
                 form takes it as an argument or puts it in the type"
            }
            Kind::Alloc => {
                "bstd has no global heap: every collection is made in an allocator its code \
                 was handed, so every byte belongs to a task or a named arena"
            }
            Kind::Renamed => {
                "bstd re-exports this type under one name: it is the same type, so only the \
                 path changes"
            }
        }
    }
}

/// Which ambient authority a row is, for the rows `blaze::build_script_ambient` reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Authority {
    Fs,
    Env,
    Process,
    Net,
    Clock,
}

/// Which items of its home a row covers. A function is matched by its name and by the type
/// its impl is of (or, a trait's method, the type it is called on), as resolution gives them.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Items {
    /// Every free function of the module.
    AnyFn,
    /// These free functions.
    Fns(&'static [&'static str]),
    /// These free functions, each named in a finding by what calls it: `_print` is what
    /// `println!` expands to.
    Shown(&'static [(&'static str, &'static str)]),
    /// Every function of these types, and every method called on a value of one.
    Types(&'static [&'static str]),
    /// These functions of these types.
    Methods(&'static [&'static str], &'static [&'static str]),
    /// Types bstd re-exports under another name: the definitions' names, then std's written
    /// name of each with bstd's path for it. A path counts where its type segment is written
    /// by std's name, and the fix writes bstd's.
    Renamed(&'static [&'static str], &'static [(&'static str, &'static str)]),
    /// The macro of this name, where it is called.
    Macro(&'static str),
    /// A `static mut`, where it is declared.
    StaticMut,
}

/// One row of the table.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Entry {
    /// Where std defines the items: the crate and the first module under its root
    /// (`("alloc", "sync")` for `Arc`, `("std", "fs")` for `std::fs::File::open`).
    pub(crate) home: &'static [(&'static str, &'static str)],
    /// As a finding names the items' module: `std::fs`.
    pub(crate) path: &'static str,
    pub(crate) items: Items,
    pub(crate) kind: Kind,
    /// Why it goes, one clause that follows "`item`, used N times in `f`, ".
    pub(crate) why: &'static str,
    /// What is written instead.
    pub(crate) instead: &'static str,
    /// The lints that already report the row: where one is on, this lint leaves it to them.
    pub(crate) covered_by: &'static [Lint],
    /// Set on the rows `blaze::build_script_ambient` reports of a build script.
    pub(crate) authority: Option<Authority>,
}

const fn row(
    home: &'static [(&'static str, &'static str)],
    path: &'static str,
    items: Items,
    kind: Kind,
    why: &'static str,
    instead: &'static str,
) -> Entry {
    Entry { home, path, items, kind, why, instead, covered_by: &[], authority: None }
}

impl Entry {
    const fn covered(self, by: &'static [Lint]) -> Entry {
        let Entry { home, path, items, kind, why, instead, covered_by: _, authority } = self;
        Entry { home, path, items, kind, why, instead, covered_by: by, authority }
    }

    const fn authority(self, authority: Authority) -> Entry {
        let Entry { home, path, items, kind, why, instead, covered_by, authority: _ } = self;
        Entry { home, path, items, kind, why, instead, covered_by, authority: Some(authority) }
    }

    /// Whether the row's findings carry a machine-applicable fix: a rename with the same
    /// arguments. Every other row changes what a function is handed, which is not mechanical.
    #[cfg(test)]
    pub(crate) const fn has_fix(&self) -> bool {
        matches!(self.items, Items::Renamed(..))
    }

    /// The items as docs/lints.md's table lists them.
    #[cfg(test)]
    fn what(&self) -> String {
        let braces = |names: &[&str]| format!("{{{}}}", names.join(", "));
        match self.items {
            Items::AnyFn => format!("`{}`'s functions", self.path),
            Items::Fns(names) => format!("`{}::{}`", self.path, braces(names)),
            Items::Shown(pairs) => {
                // alloc: a docs row's names, a few
                let mut shown: Vec<String> = Vec::new();
                for (_, name) in pairs {
                    let name = format!("`{name}`");
                    if !shown.contains(&name) {
                        shown.push(name);
                    }
                }
                shown.join(", ")
            }
            Items::Types(types) => format!("`{}::{}`", self.path, braces(types)),
            Items::Methods(types, fns) => {
                format!("`{}::{}::{}`", self.path, braces(types), braces(fns))
            }
            Items::Renamed(_, written) => {
                // alloc: a docs row's names, a few
                let names: Vec<&str> = written.iter().map(|(from, _)| *from).collect();
                format!("`{}::{}`", self.path, braces(&names))
            }
            Items::Macro(name) => format!("`std::{name}!`"),
            Items::StaticMut => "`static mut`".to_owned(), // alloc: a docs row's part
        }
    }

    /// What replaces the items, as docs/lints.md's table has it.
    #[cfg(test)]
    fn replaced_by(&self) -> String {
        match self.items {
            Items::Renamed(_, written) => {
                // alloc: a docs row's names, a few
                let names: Vec<String> = written.iter().map(|(_, to)| format!("`{to}`")).collect();
                names.join(", ")
            }
            _ => self.instead.to_owned(), // alloc: a docs row's part
        }
    }

    /// The row of docs/lints.md's table for this one: the items, the kind, what replaces
    /// them, whether a finding has a fix, and the lints it is left to.
    #[cfg(test)]
    pub(crate) fn docs_row(&self) -> String {
        // alloc: a docs row's names, a few
        let covered: Vec<String> =
            self.covered_by.iter().map(|it| format!("`blaze::{}`", it.name())).collect();
        format!(
            "| {} | {} | {} | {} | {} |",
            self.what(),
            self.kind.say(),
            self.replaced_by(),
            if self.has_fix() { "yes" } else { "no" },
            covered.join(", "),
        )
    }
}

use Authority as A;
use Items as I;
use Kind as K;

/// `std::env`'s functions that read or change the process's environment; `split_paths` and
/// `join_paths` only compute.
const ENV_VARS: &[&str] = &["var", "var_os", "vars", "vars_os", "set_var", "remove_var"];

/// `std::fs`'s types whose functions reach the file system; `Metadata`, `Permissions` and
/// `FileType` are data already read.
const FS_TYPES: &[&str] = &["File", "OpenOptions", "DirBuilder", "ReadDir", "DirEntry"];

/// `std::net`'s types that own a socket; an address is data.
const NET_TYPES: &[&str] = &["TcpStream", "TcpListener", "UdpSocket"];

/// The lints that already find a global-heap allocation.
const ALLOC_LINTS: &[Lint] = &[Lint::HotAlloc, Lint::ImplicitAllocator];

const NOWHERE_FS: &str = "takes the file system from nowhere";
const GLOBAL_HEAP: &str = "allocates from the global heap, which nobody handed it";
const ONE_NAME: &str = "has one name in bstd, and this is not it";

/// What bstd replaces or forbids of Rust's standard library. A function is given the first
/// row that matches it, so a row of named functions comes before its module's catch-all.
pub(crate) const TABLE: &[Entry] = &[
    // Hidden cost: what the call decides without an argument for it.
    row(&[("std", "fs")], "std::fs", I::Fns(&["write"]), K::Hidden, "writes a file with no word on whether its bytes reach the disk", "`dir.write(cx, path, bytes, Durable::..)` or `dir.publish(path, content, durable)` on a `Dir`: durability is an argument with no default").authority(A::Fs),
    row(&[("std", "fs")], "std::fs", I::Fns(&["rename"]), K::Hidden, "replaces a file with no word on whether the rename is synced", "`dir.rename(cx, from, to, Durable::..)` or `dir.publish(path, content, durable)` on a `Dir`").authority(A::Fs),
    row(&[("std", "fs")], "std::fs", I::Fns(&["copy"]), K::Hidden, "clones or copies as the system picks, and a clone of unflushed data forces its writeback", "`flushed.clone_to(..)`, which exists on a `Flushed` file alone, or `copy_to(..)`, a plain write of the bytes").authority(A::Fs),
    row(&[("std", "fs")], "std::fs", I::Methods(&["File"], &["sync_all", "sync_data"]), K::Hidden, "waits for the device with no deadline, and nothing in the file's type says it was synced", "`written.flush(cx)`, which gives a `Flushed`, or `Durable::Yes` on the write").authority(A::Fs),
    row(&[("std", "io")], "std::io", I::Types(&["BufWriter", "LineWriter"]), K::Hidden, "loses its last error, and its last bytes, when it is dropped unflushed", "build the bytes in a buffer you own (`mem::Bytes`), write them with one call, and end the file with `file.close(cx)?`"),
    row(&[("std", "thread")], "std::thread", I::Fns(&["park", "park_timeout"]), K::Hidden, "parks the thread with no deadline and no name", "the one wait of `Cx`, which ends in what it waited for or in a `Stopped`"),
    row(&[("std", "sync")], "std::sync", I::Types(&["Mutex", "RwLock", "MutexGuard", "RwLockReadGuard", "RwLockWriteGuard"]), K::Hidden, "poisons on a panic and waits with no deadline or name", "`Lock` and `RwLock` of `bstd::sync`, each made with its name, whose `lock` returns the guard and not a `Result`"),
    // Ambient authority: reached from nowhere.
    row(&[("std", "fs")], "std::fs", I::AnyFn, K::Ambient, NOWHERE_FS, "take a `Dir` (from `Fs`: `io.fs().open_dir(cx, path)`) and call its method (`dir.read_all(cx, path, &mut bytes)`, `dir.stat(cx, path)`, `dir.remove(cx, path)`)").authority(A::Fs),
    row(&[("std", "fs")], "std::fs", I::Types(FS_TYPES), K::Ambient, NOWHERE_FS, "take a `Dir` and open beneath it (`dir.open(cx, path)`, `dir.create(cx, path)`, `dir.list(cx, frame)`), which gives a `File` of that directory's rights").authority(A::Fs),
    row(&[("std", "env")], "std::env", I::Fns(ENV_VARS), K::Ambient, "reads or changes the process's environment from nowhere", "take an `Env` and call `env.get(&var!(\"NAME\"))`; hand on less with `env.only(..)`").authority(A::Env),
    row(&[("std", "env")], "std::env", I::Fns(&["args", "args_os"]), K::Ambient, "reads the command line from nowhere", "take `Args` (`io.args()`)").authority(A::Env),
    row(&[("std", "env")], "std::env", I::Fns(&["current_dir", "set_current_dir", "current_exe", "temp_dir", "home_dir"]), K::Ambient, "asks the process where it is, and bstd has no current directory", "take the `Dir` the path stood for; the home directory is `User`'s").authority(A::Env),
    row(&[("std", "process")], "std::process", I::Fns(&["exit", "abort", "id"]), K::Ambient, "ends or names the process from nowhere", "take a `Host` (`host.exit(status)`, `host.pid()`), or return from `main`").authority(A::Process),
    row(&[("std", "process")], "std::process", I::Types(&["Command", "Child"]), K::Ambient, "starts a program with everything this process can reach", "take a `Runner` of a sandbox and call `run.run(cx, &Command::new(prog!(\"..\")))`: what the program can touch is the sandbox's spec").authority(A::Process),
    row(&[("std", "net")], "std::net", I::Types(NET_TYPES), K::Ambient, "reaches the network from nowhere", "take a `Net` (`net.connect(cx, &addr)`, `net.listen(cx, &addr)`), or the `Http` made over it").authority(A::Net),
    row(&[("std", "time")], "std::time", I::Methods(&["Instant", "SystemTime"], &["now", "elapsed"]), K::Ambient, "reads the clock from nowhere", "take a `Clock` and call `clock.now()`; a simulated run then has a simulated time").authority(A::Clock),
    row(&[("std", "io")], "std::io", I::Fns(&["stdin", "stdout", "stderr"]), K::Ambient, "takes the process's standard streams from nowhere", "take `Out` (`io.out()`)"),
    row(&[("std", "io")], "std", I::Shown(&[("_print", "std::println!"), ("_eprint", "std::eprintln!")]), K::Ambient, "writes to the process's standard streams from nowhere", "take `Out` and write to it; what a program says to a person or an agent is a `Report`"),
    row(&[("std", "thread")], "std::thread", I::Fns(&["sleep", "sleep_until"]), K::Ambient, "blocks the thread on the wall clock, with no deadline of its task's", "take a `Clock` and call `clock.sleep(cx, wait)`"),
    row(&[("std", "thread")], "std::thread", I::Fns(&["available_parallelism"]), K::Ambient, "asks the machine for its core count from nowhere", "`host.cores()` of a `Host`"),
    row(&[("std", "thread")], "std::thread", I::Types(&["LocalKey"]), K::Ambient, "keeps state per thread, out of every signature", "`Cx` for the current span, deadline or runtime, `cx.frame()` for scratch, a local the task owns, or a `PerWorker<T>` someone passes"),
    row(&[("std", "thread")], "std::thread", I::AnyFn, K::Ambient, "starts, names or yields a thread from nowhere", "start a task through `Cx`: `let s = cx.scope();`, `s.spawn(\"name\", ..)`, `s.join(cx)?`"),
    row(&[("std", "thread")], "std::thread", I::Types(&["Builder", "JoinHandle", "Thread", "Scope", "ScopedJoinHandle"]), K::Ambient, "starts or holds a thread nothing handed it", "a `Scope` from `cx.scope()` and the `Task` its `spawn` gives, joined with `join(cx)`"),
    row(&[], "std", I::Macro("thread_local"), K::Ambient, "keeps state per thread, out of every signature", "`Cx` for the current span, deadline or runtime, `cx.frame()` for scratch, a local the task owns, or a `PerWorker<T>` someone passes"),
    row(&[], "", I::StaticMut, K::Ambient, "is global mutable state, out of every signature", "a value someone owns and passes down, or a context"),
    row(&[("std", "sync")], "std::sync::mpsc", I::Types(&["Sender", "SyncSender", "Receiver"]), K::Ambient, "is a second channel, whose waits have no deadline", "the one channel of `bstd::chan`"),
    row(&[("std", "sync")], "std::sync::mpsc", I::Fns(&["channel", "sync_channel"]), K::Ambient, "makes a second channel, whose waits have no deadline", "the one channel of `bstd::chan`"),
    row(&[("std", "sync")], "std::sync", I::Types(&["Condvar", "Barrier"]), K::Ambient, "waits with no deadline or name", "`Signal` of `bstd::sync`, or a scope's `join(cx)`"),
    row(&[("std", "sync")], "std::sync", I::Types(&["Once", "OnceState"]), K::Ambient, "is one-time state nobody owns", "`Once<T>` of `bstd::sync`, a value to own and pass, never in a `static`"),
    row(&[("std", "hash"), ("std", "collections")], "std::hash", I::Types(&["RandomState", "DefaultHasher"]), K::Ambient, "seeds a hasher from the system's randomness, so a map's order differs from run to run", "`bstd::hash::FixedState`, the one fixed seed; under keys an attacker picks, `SipKey` from an `Rng`").covered(&[Lint::DefaultHasher]),
    row(&[("std", "collections")], "std::collections", I::Types(&["HashMap", "HashSet"]), K::Ambient, "seeds its hasher from the system's randomness and allocates from the global heap", "`bstd::map::Map` and `bstd::set::Set`, made with `new_in(alloc)`").covered(&[Lint::DefaultHasher, Lint::ImplicitAllocator]),
    row(&[("std", "random")], "std::random", I::AnyFn, K::Ambient, "takes randomness from the system", "take an `Rng` (`rng.next_u64()`, `rng.fill(..)`), which is seeded in a simulation"),
    // Renamed: one definition, bstd's name for it.
    row(&[("alloc", "sync")], "std::sync", I::Renamed(&["Arc"], &[("Arc", "bstd::sync::Shared")]), K::Renamed, ONE_NAME, ""),
    row(&[("alloc", "rc")], "std::rc", I::Renamed(&["Rc"], &[("Rc", "bstd::sync::LocalShared")]), K::Renamed, ONE_NAME, ""),
    row(&[("core", "cell")], "std::cell", I::Renamed(&["RefCell", "OnceCell", "LazyCell", "UnsafeCell"], &[("RefCell", "bstd::sync::LocalLock"), ("OnceCell", "bstd::sync::LocalOnce"), ("LazyCell", "bstd::sync::LocalLazy"), ("UnsafeCell", "bstd::cell::RawCell")]), K::Renamed, ONE_NAME, ""),
    row(&[("std", "sync")], "std::sync", I::Renamed(&["OnceLock", "LazyLock"], &[("OnceLock", "bstd::sync::Once"), ("LazyLock", "bstd::sync::Lazy")]), K::Renamed, ONE_NAME, ""),
    row(&[("core", "sync")], "std::sync::atomic", I::Renamed(&["Atomic", "AtomicBool", "AtomicU8", "AtomicU16", "AtomicU32", "AtomicU64", "AtomicUsize", "AtomicI8", "AtomicI16", "AtomicI32", "AtomicI64", "AtomicIsize"], &[("AtomicBool", "bstd::sync::Atomic::<bool>"), ("AtomicU8", "bstd::sync::Atomic::<u8>"), ("AtomicU16", "bstd::sync::Atomic::<u16>"), ("AtomicU32", "bstd::sync::Atomic::<u32>"), ("AtomicU64", "bstd::sync::Atomic::<u64>"), ("AtomicUsize", "bstd::sync::Atomic::<usize>"), ("AtomicI8", "bstd::sync::Atomic::<i8>"), ("AtomicI16", "bstd::sync::Atomic::<i16>"), ("AtomicI32", "bstd::sync::Atomic::<i32>"), ("AtomicI64", "bstd::sync::Atomic::<i64>"), ("AtomicIsize", "bstd::sync::Atomic::<isize>")]), K::Renamed, ONE_NAME, ""),
    // Global-heap allocation: left to `hot_alloc` and `implicit_allocator` where one is on.
    row(&[("alloc", "vec")], "std::vec", I::Methods(&["Vec"], &["new", "with_capacity", "from", "from_iter", "default"]), K::Alloc, GLOBAL_HEAP, "`Vec::new_in(alloc)` or `Vec::with_capacity_in(n, alloc)`, in the allocator the code was handed (`cx.frame()`, an `Arena`)").covered(ALLOC_LINTS),
    row(&[("alloc", "vec"), ("alloc", "boxed")], "std", I::Shown(&[("from_elem", "std::vec!"), ("box_assume_init_into_vec_unsafe", "std::vec!")]), K::Alloc, GLOBAL_HEAP, "`Vec::with_capacity_in(n, alloc)` and its pushes, in the allocator the code was handed").covered(ALLOC_LINTS),
    row(&[("alloc", "boxed")], "std::boxed", I::Methods(&["Box"], &["new", "pin", "from", "default"]), K::Alloc, GLOBAL_HEAP, "`Box::new_in(value, alloc)`").covered(ALLOC_LINTS),
    row(&[("alloc", "string")], "std::string", I::Methods(&["String"], &["new", "with_capacity", "from", "from_utf8", "from_utf8_lossy", "from_iter", "default"]), K::Alloc, GLOBAL_HEAP, "`String::new_in(alloc)` of `bstd::mem`, text in the allocator the code was handed").covered(ALLOC_LINTS),
    row(&[("alloc", "string")], "std", I::Shown(&[("to_string", "std::string::ToString::to_string")]), K::Alloc, GLOBAL_HEAP, "`format_in!(alloc, \"{value}\")` of `bstd::mem`").covered(ALLOC_LINTS),
    row(&[("alloc", "fmt")], "std", I::Shown(&[("format", "std::format!")]), K::Alloc, GLOBAL_HEAP, "`format_in!(alloc, ..)` of `bstd::mem`").covered(ALLOC_LINTS),
    row(&[("alloc", "collections")], "std::collections", I::Methods(&["BTreeMap", "BTreeSet", "VecDeque", "BinaryHeap", "LinkedList"], &["new", "with_capacity", "from", "from_iter", "default"]), K::Alloc, GLOBAL_HEAP, "its `new_in(alloc)`, in the allocator the code was handed").covered(ALLOC_LINTS),
    row(&[("std", "path")], "std::path", I::Methods(&["PathBuf"], &["new", "with_capacity", "from", "default"]), K::Alloc, GLOBAL_HEAP, "`bstd::path::Path`, relative and beneath a `Dir`; an owned one is `mem::PathBuf<A>` in an allocator").covered(ALLOC_LINTS),
];

/// What bstd re-exports unchanged, so no row may name it: plain computation that reaches
/// nothing. docs/lints.md lists these under the table.
#[cfg(test)]
pub(crate) const UNCHANGED: &[&str] = &[
    "core::option",
    "core::result",
    "core::iter",
    "core::slice",
    "core::str",
    "core::num",
    "core::cmp",
    "core::ops",
    "core::mem",
    "core::convert",
    "core::fmt",
    "core::time::Duration",
];

/// One item of [`TABLE`] a body uses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Use {
    /// Its row of [`TABLE`].
    pub(crate) row: usize,
    /// As a finding names it: `std::fs::read`, `std::process::Command`, `std::println!`.
    pub(crate) name: String,
    /// How many of the body's paths and method calls are of it.
    pub(crate) count: u32,
    /// The first of them that is written somewhere, as the body's analysis places it; `None`
    /// when no use has a place.
    pub(crate) first: Option<TextRange>,
    /// For a renamed type: each place std's path of it is written, with bstd's path.
    pub(crate) renames: Vec<(TextRange, &'static str)>,
}

/// The standard library's crate `module` is in and the first module under that crate's root:
/// `("std", fs)` for `std::fs::File::open`, `("alloc", sync)` for `Arc::new`.
fn home(db: &Db, module: hir::Module) -> Option<(&'static str, hir::Name)> {
    let krate = module.krate(db);
    let krate = ["std", "alloc", "core"].into_iter().find(|it| facts::crate_is(db, krate, it))?;
    let top = module.path_to_root(db).into_iter().rev().find_map(|it| it.name(db))?;
    Some((krate, top))
}

/// The first row of [`TABLE`] for the function `name` defined in `top` of `krate`, a free
/// one or one of the type `owner`, and how a finding names it.
fn row_of(krate: &str, top: &str, owner: Option<&str>, name: &str) -> Option<(usize, String)> {
    TABLE.iter().enumerate().find_map(|(at, entry)| {
        if !entry.home.iter().any(|(k, t)| *k == krate && *t == top) {
            return None;
        }
        let path = entry.path;
        let named = match (entry.items, owner) {
            (Items::AnyFn, None) => format!("{path}::{name}"),
            (Items::Fns(names), None) if names.contains(&name) => format!("{path}::{name}"),
            (Items::Shown(pairs), None) => {
                // alloc: a finding's name
                pairs.iter().find(|(it, _)| *it == name)?.1.to_owned()
            }
            (Items::Types(types), Some(ty)) if types.contains(&ty) => format!("{path}::{ty}"),
            (Items::Methods(types, fns), Some(ty))
                if types.contains(&ty) && fns.contains(&name) =>
            {
                format!("{path}::{ty}::{name}")
            }
            (Items::Renamed(types, _), Some(ty)) if types.contains(&ty) => {
                format!("{path}::{ty}")
            }
            _ => return None,
        };
        Some((at, named))
    })
}

/// The row of the function `f`, called with `args`: by where it is defined and the type its
/// impl is of; then, for a trait's method, by the type it is called on (`Self`, the first of
/// `args`), so `file.write_all(..)` is `std::fs::File`'s and `String::from(..)` is `String`'s.
fn classify<'db>(db: &'db Db, f: FunctionId, args: GenericArgs<'db>) -> Option<(usize, String)> {
    let function = hir::Function::from(f);
    let name = function.name(db);
    let name = name.as_str();
    let owner = facts::impl_adt(db, f).map(|it| it.name(db));
    let own = home(db, function.module(db)).and_then(|(krate, top)| {
        row_of(krate, top.as_str(), owner.as_ref().map(hir::Name::as_str), name)
    });
    if own.is_some() || !alloc::in_trait(db, f) {
        return own;
    }
    let (adt, _) = args.types().next()?.strip_references().as_adt()?;
    let adt = hir::Adt::from(adt);
    let (krate, top) = home(db, adt.module(db))?;
    row_of(krate, top.as_str(), Some(adt.name(db).as_str()), name)
}

/// The type segment of a path expression as it is written: for `Arc::new`, `sync::Arc::new`
/// or `Arc::<T>::new`, how many bytes of `snippet` are the type's path (`Arc`, `sync::Arc`)
/// and the type's name in it. `None` for anything else: a qualified path (`<T as Trait>::f`),
/// a bare function, text that is not a path (a macro call the expression came out of).
fn written_type(snippet: &str) -> Option<(usize, &str)> {
    let bytes = snippet.as_bytes();
    let mut depth = 0u32;
    let mut last = None;
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b'<' => depth += 1,
            b'>' => depth = depth.checked_sub(1)?,
            b':' if depth == 0 && bytes.get(at + 1) == Some(&b':') => {
                last = Some(at);
                at += 1;
            }
            _ => {}
        }
        at += 1;
    }
    // What is before the last `::` outside angle brackets: the type, and perhaps its arguments.
    let qualifier = &snippet[..last?];
    let base = match qualifier.find('<') {
        Some(lt) => qualifier[..lt].strip_suffix("::").unwrap_or(&qualifier[..lt]),
        None => qualifier,
    };
    let of_path = |it: u8| it.is_ascii_alphanumeric() || it == b'_' || it == b':';
    if base.is_empty() || !base.bytes().all(of_path) {
        return None;
    }
    let name = base.rsplit("::").next()?;
    (!name.is_empty()).then_some((base.len(), name))
}

/// Every use of an item of [`TABLE`] in the body `analysis` is of, as the module docs say,
/// in the order the items are first met. `snippet` gives the text at a range
/// [`hir::BodyAnalysis::expr_range`] returns, which only the renamed rows read; a caller with
/// no text gives `""` and finds none of those.
// dyn: called once per body linted, and its one use is cold
pub(crate) fn uses_in<'db, 's>(
    db: &'db Db,
    analysis: &hir::BodyAnalysis<'db>,
    snippet: &dyn Fn(TextRange) -> &'s str,
) -> Vec<Use> {
    let body = analysis.body();
    let infer = analysis.infer();
    // alloc: one body's uses, a few items, dropped when its findings are reported
    let mut uses: Vec<Use> = Vec::new();
    for (id, expr) in body.exprs() {
        let (f, args, is_path) = match expr {
            Expr::Path(_) => {
                let TyKind::FnDef(def, args) = infer.expr_ty(id).kind() else { continue };
                let CallableDefId::FunctionId(f) = def.0 else { continue };
                (f, args.skip_binder(), true)
            }
            Expr::MethodCall { .. } => {
                let Some((f, args)) = infer.method_resolution(id) else { continue };
                (f, args, false)
            }
            _ => continue,
        };
        let Some((row, mut name)) = classify(db, f, args) else { continue };
        let entry = &TABLE[row];
        let range = analysis.expr_range(id);
        let mut rename = None;
        if let Items::Renamed(_, written) = entry.items {
            // One definition under two names: only std's, as written, is a use.
            let Some(range) = range.filter(|_| is_path) else { continue };
            let Some((len, said)) = written_type(snippet(range)) else { continue };
            let Some(&(_, to)) = written.iter().find(|(from, _)| *from == said) else { continue };
            name = format!("{}::{said}", entry.path);
            let len = TextSize::new(u32::try_from(len).unwrap_or(u32::MAX));
            rename = Some((TextRange::at(range.start(), len), to));
        }
        let at = match uses.iter().position(|it| it.name == name) {
            Some(at) => at,
            None => {
                // alloc: the body's uses, above
                uses.push(Use { row, name, count: 0, first: None, renames: Vec::new() });
                uses.len() - 1
            }
        };
        let used = &mut uses[at];
        used.count += 1;
        used.renames.extend(rename);
        // The expressions of a body are not met in the order they are written.
        if let Some(range) = range
            && used.first.is_none_or(|first| range.start() < first.start())
        {
            used.first = Some(range);
        }
    }
    uses
}

/// Whether the crate linted is a build script, whose uses are `build_script_ambient`'s.
fn in_build_script(cx: &Cx<'_, '_>) -> bool {
    matches!(cx.krate().target_kind(cx.db()), TargetKind::BuildScript)
}

/// Whether a path that starts at [`BSTD`] resolves in the crate linted.
fn names_bstd(cx: &Cx<'_, '_>) -> bool {
    cx.krate().dependencies_iter(cx.db()).any(|it| it.name.as_str() == BSTD)
}

/// "once" or "N times".
fn times(count: u32) -> String {
    match count {
        1 => "once".to_owned(), // alloc: a message's part
        n => format!("{n} times"),
    }
}

/// A finding's first line: the item, its kind, how often `item` uses it, why it goes and what
/// is written instead (`to`, bstd's path, for a renamed type).
fn message(entry: &Entry, name: &str, used: &str, to: Option<&str>) -> String {
    let instead = match to {
        Some(to) => format!("write `{to}`"),
        None => entry.instead.to_owned(), // alloc: a message's part
    };
    format!("`{name}` ({}), {used}, {}: {instead}", entry.kind.say(), entry.why)
}

/// What each finding says after its first line.
fn decorate(diagnostic: &mut crate::diag::Diag, entry: &Entry) {
    diagnostic.note(entry.kind.note());
    if !ready::BSTD_IN_TREE {
        diagnostic.note(PLANNED);
    }
}

/// The findings of the body walked: its uses, one per item, each at its first.
pub(super) fn check(cx: &mut Cx<'_, '_>) {
    if !cx.on_blaze(LINT) || in_build_script(cx) || cx.is_in_test(cx.root()) {
        return;
    }
    let db = cx.db();
    let (mut found, item) = {
        let Some(analysis) = cx.analysis() else { return };
        let found = uses_in(db, analysis, &|range| cx.snippet(range));
        (found, super::body_item(db, analysis))
    };
    found.retain(|it| !TABLE[it.row].covered_by.iter().any(|lint| cx.on_blaze(*lint)));
    found.sort_by_key(|it| it.first.map(TextRange::start));
    let fixes = names_bstd(cx);
    for used in found {
        let Some(range) = used.first else { continue };
        let entry = &TABLE[used.row];
        let root = cx.root();
        let at = match root.text_range().contains_range(range) {
            true => match root.covering_element(range) {
                NodeOrToken::Node(it) => it,
                NodeOrToken::Token(it) => it.parent().unwrap_or(root),
            },
            false => root,
        };
        let span = cx.span(range);
        // alloc: a fix's parts, one per place the type is written
        let parts: Vec<_> = match fixes {
            true => used.renames.iter().map(|(at, to)| (cx.span(*at), (*to).to_owned())).collect(),
            false => Vec::new(),
        };
        let to = used.renames.first().map(|it| it.1);
        let how_often = format!("used {} in {item}", times(used.count));
        let diagnostic = cx.blaze(LINT, at, span, message(entry, &used.name, &how_often, to));
        decorate(diagnostic, entry);
        if !parts.is_empty() {
            diagnostic.multipart_suggestion(
                "write it by bstd's name",
                parts,
                Applicability::MachineApplicable,
            );
        }
        diagnostic
            .help(LINT.help())
            .help(format!("for further information visit {}", LINT.docs_url()));
    }
}

/// The finding of `node` when it declares what a row forbids: a `static mut`, or a call of
/// `thread_local!` (by the crate that defines the macro, never by its name alone).
pub(super) fn check_declaration<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    let (row, name, at) = if let Some(item) = ast::Static::cast(node) {
        if item.mut_token().is_none() {
            return;
        }
        let Some(row) = TABLE.iter().position(|it| matches!(it.items, Items::StaticMut)) else {
            return;
        };
        // alloc: a message's part
        (row, "static mut".to_owned(), item.name().map_or(node, |it| it.syntax()))
    } else if let Some(call) = ast::MacroCall::cast(node) {
        let Some(called) = cx.std_macro(call) else { return };
        let Some(row) = TABLE
            .iter()
            .position(|it| matches!(it.items, Items::Macro(name) if name == called.as_str()))
        else {
            return;
        };
        (row, format!("std::{}!", called.as_str()), call.path().map_or(node, |it| it.syntax()))
    } else {
        return;
    };
    if in_build_script(cx) || !cx.is_written(node) || cx.is_in_test(node) {
        return;
    }
    let Some(range) = cx.range(at) else { return };
    let entry = &TABLE[row];
    let span = cx.span(range);
    let diagnostic = cx.blaze(LINT, node, span, message(entry, &name, "declared here", None));
    decorate(diagnostic, entry);
    diagnostic.help(LINT.help()).help(format!("for further information visit {}", LINT.docs_url()));
}

#[cfg(test)]
mod tests {
    use super::{Entry, Items, Kind, TABLE, UNCHANGED, message, row_of, written_type};

    fn row(name: &str) -> &'static Entry {
        let at = TABLE
            .iter()
            .position(|it| it.docs_row().contains(name))
            .unwrap_or_else(|| panic!("a row whose docs line has {name}"));
        &TABLE[at]
    }

    /// A function has the first row that matches it: the named functions of a module before
    /// its catch-all, a type's named functions before the type.
    #[test]
    fn a_function_has_the_first_row_that_names_it() {
        let of = |krate, top, owner, name| {
            row_of(krate, top, owner, name).map(|(at, named)| (TABLE[at].kind, named))
        };
        assert_eq!(
            of("std", "fs", None, "write"),
            Some((Kind::Hidden, "std::fs::write".to_owned()))
        );
        assert_eq!(of("std", "fs", None, "read"), Some((Kind::Ambient, "std::fs::read".to_owned())));
        assert_eq!(
            of("std", "fs", Some("File"), "sync_all"),
            Some((Kind::Hidden, "std::fs::File::sync_all".to_owned()))
        );
        assert_eq!(
            of("std", "fs", Some("File"), "open"),
            Some((Kind::Ambient, "std::fs::File".to_owned()))
        );
        assert_eq!(of("std", "fs", Some("Metadata"), "len"), None, "data already read");
        assert_eq!(of("std", "env", None, "split_paths"), None, "it only computes");
        assert_eq!(
            of("std", "time", Some("Instant"), "now"),
            Some((Kind::Ambient, "std::time::Instant::now".to_owned()))
        );
        assert_eq!(of("std", "time", Some("Instant"), "duration_since"), None);
        assert_eq!(of("std", "io", None, "_print"), Some((Kind::Ambient, "std::println!".to_owned())));
        assert_eq!(
            of("std", "thread", None, "park"),
            Some((Kind::Hidden, "std::thread::park".to_owned()))
        );
        assert_eq!(
            of("std", "thread", None, "spawn"),
            Some((Kind::Ambient, "std::thread::spawn".to_owned()))
        );
        assert_eq!(
            of("alloc", "vec", Some("Vec"), "new"),
            Some((Kind::Alloc, "std::vec::Vec::new".to_owned()))
        );
        assert_eq!(of("alloc", "vec", Some("Vec"), "len"), None, "a use of a value, not a heap");
        assert_eq!(of("alloc", "fmt", None, "format"), Some((Kind::Alloc, "std::format!".to_owned())));
        assert_eq!(
            of("alloc", "sync", Some("Arc"), "new"),
            Some((Kind::Renamed, "std::sync::Arc".to_owned()))
        );
        assert_eq!(of("blaze_core", "fs", None, "read"), None, "only the standard library's");
    }

    /// Plain computation of `core` has no row: the only rows at home in `core` are the cells
    /// and the atomics bstd renames, and nothing [`UNCHANGED`] lists is a row's home.
    #[test]
    fn core_computation_has_no_row() {
        for entry in TABLE {
            for (krate, top) in entry.home {
                assert!(
                    *krate != "core" || matches!(*top, "cell" | "sync"),
                    "{}: `core::{top}` is plain computation",
                    entry.docs_row()
                );
                let home = format!("{krate}::{top}");
                assert!(!UNCHANGED.contains(&home.as_str()), "{home} is re-exported unchanged");
            }
        }
        for (top, name) in [("option", "map"), ("iter", "map"), ("slice", "len"), ("str", "len")] {
            assert_eq!(row_of("core", top, None, name), None);
            assert_eq!(row_of("core", top, Some("Option"), name), None);
        }
        assert_eq!(row_of("core", "mem", None, "swap"), None);
    }

    /// The rows with a fix are the renames, and no other; a row left to another lint names it.
    #[test]
    fn the_rows_with_a_fix_are_the_renames() {
        for entry in TABLE {
            assert_eq!(entry.has_fix(), entry.kind == Kind::Renamed, "{}", entry.docs_row());
            assert_eq!(
                entry.kind == Kind::Alloc,
                entry.covered_by.contains(&super::Lint::HotAlloc),
                "an allocation row is `hot_alloc`'s where that is on: {}",
                entry.docs_row()
            );
            if let Items::Renamed(types, written) = entry.items {
                for (from, to) in written {
                    assert!(types.contains(from), "{from} is a definition's name");
                    assert!(to.starts_with("bstd::"), "{to} starts at bstd");
                }
            }
            for text in [entry.why, entry.instead, entry.path] {
                assert!(!text.contains('|'), "a table cell holds no `|`: {text}");
            }
        }
        assert!(row("`std::sync::{Arc}`").docs_row().contains("| yes |"));
        assert!(row("`std::fs`'s functions").docs_row().contains("| no |"));
    }

    /// The type of a path as written, and how much of the path is its.
    #[test]
    fn the_type_segment_of_a_path_is_read_as_written() {
        assert_eq!(written_type("Arc::new"), Some((3, "Arc")));
        assert_eq!(written_type("std::sync::Arc::new"), Some((14, "Arc")));
        assert_eq!(written_type("Arc::<Vec<u8>>::new"), Some((3, "Arc")));
        assert_eq!(written_type("Shared::new"), Some((6, "Shared")));
        assert_eq!(written_type("<Arc<u8>>::new"), None);
        assert_eq!(written_type("<Arc<u8> as Clone>::clone"), None);
        assert_eq!(written_type("spawn"), None);
        assert_eq!(written_type("vec![Arc::new(1)]"), None);
    }

    /// A finding's first line names the item, its kind, its count and place, why and what
    /// instead: the shape harness/v2/std_uses.py reads.
    #[test]
    fn a_finding_names_the_item_its_kind_its_count_why_and_what_instead() {
        assert_eq!(
            message(row("`std::env::{var, "), "std::env::var", "used 2 times in `fn load`", None),
            "`std::env::var` (ambient authority), used 2 times in `fn load`, reads or changes the \
             process's environment from nowhere: take an `Env` and call \
             `env.get(&var!(\"NAME\"))`; hand on less with `env.only(..)`"
        );
        assert_eq!(
            message(
                row("`std::sync::{Arc}`"),
                "std::sync::Arc",
                "used once in `fn share`",
                Some("bstd::sync::Shared")
            ),
            "`std::sync::Arc` (renamed), used once in `fn share`, has one name in bstd, and this \
             is not it: write `bstd::sync::Shared`"
        );
        assert_eq!(
            message(row("`static mut`"), "static mut", "declared here", None),
            "`static mut` (ambient authority), declared here, is global mutable state, out of \
             every signature: a value someone owns and passes down, or a context"
        );
    }

    /// docs/lints.md's table of rows is this table, row for row.
    #[test]
    fn the_docs_table_is_the_table() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/lints.md");
        let docs = std::fs::read_to_string(path).unwrap_or_else(|error| panic!("{path}: {error}"));
        let mut lines = docs.lines().skip_while(|it| !it.starts_with("| std's item | kind |"));
        assert!(lines.next().is_some(), "docs/lints.md has the table of `std_use`'s rows");
        let said: Vec<&str> = lines.skip(1).take_while(|it| it.starts_with('|')).collect();
        let rows: Vec<String> = TABLE.iter().map(Entry::docs_row).collect();
        assert_eq!(said, rows, "docs/lints.md's table of `std_use`'s rows is `TABLE`");
        for kind in Kind::ALL {
            assert!(docs.contains(&format!("**{}**", kind.say())), "{} is explained", kind.say());
        }
        for unchanged in UNCHANGED {
            assert!(docs.contains(&format!("`{unchanged}`")), "{unchanged} is listed as unchanged");
        }
    }
}
