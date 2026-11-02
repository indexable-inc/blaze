//! Whether a build script is pure, proved from its MIR (docs/v2/plan/lanes/pure-scripts/design.md):
//! its whole reachable call graph, monomorphized from `main` through its build dependencies and
//! std, only reads the variables cargo gives a build script ([`allowed_env`]), reads its
//! package's files and writes its `OUT_DIR` at paths made only from those, and prints. A pure
//! script's result is then a function of its [`Proof::identity`], the variables of
//! [`Proof::env`] and the files it read, which `blaze_usercode::pure` keys it by.
//!
//! - [`walk`](script_purity): the instances from `main`, each summarized bottom-up as a
//!   query over its monomorphized runtime MIR: what it does, the first thing on the way that is
//!   not pure, and the calls leading to it. Function pointers and `dyn` calls are closed over
//!   every function the graph makes a pointer of and every vtable it makes (design section 3).
//! - [`values`]: each local's abstract value ([`Abs`]), for the variable names and paths the
//!   leaves are given (design section 6).
//! - [`models`]: where the walk stops in std (a [`Leaf`]) and the functions whose values it
//!   follows (a [`Transfer`]), by path (design section 5).
//! - [`identity`]: the Merkle hash of the instance graph (design section 4).
//!
//! Anything the walk can't prove is [`Purity::Impure`]: the script runs in the sandbox as any
//! other. That is the design, not a fallback.

#![forbid(unsafe_code)]

mod identity;
pub mod models;
mod values;
mod walk;

#[cfg(test)]
mod tests;

#[cfg(test)]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

use blaze_ids::ContentId;
use intern::Symbol;

pub use walk::{main_of, script_purity};
/// What a pure script's code may do, as its proof says: what the first sandboxed run of it is
/// checked against (`blaze_usercode::pure::check`), and what its key is made of.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Proof {
    /// The Merkle hash of its instance graph (design section 4).
    pub identity: ContentId,
    /// Every variable the code can read, each [`allowed_env`], sorted and each once: the
    /// shape's variables, whether or not a run read them.
    pub env: Box<[Symbol]>,
    pub effects: Effects,
    /// How many instances the walk reached, for the harness.
    pub instances: u32,
}

/// What kinds of effect a pure script's code has, as bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct Effects(pub u8);

impl Effects {
    pub const NONE: Effects = Effects(0);
    /// Prints to stdout or stderr.
    pub const PRINT: Effects = Effects(1);
    /// Reads files inside its package.
    pub const READ: Effects = Effects(1 << 1);
    /// Writes files or directories inside its `OUT_DIR`.
    pub const WRITE: Effects = Effects(1 << 2);
    /// Tests whether a path inside its package exists, or is a file.
    pub const PROBE: Effects = Effects(1 << 3);
    /// Ends the run with `process::exit` or `process::abort`.
    pub const EXIT: Effects = Effects(1 << 4);

    pub const fn union(self, other: Effects) -> Effects {
        Effects(self.0 | other.0)
    }

    pub const fn has(self, other: Effects) -> bool {
        self.0 & other.0 == other.0
    }
}

/// What the walk found of a script's `main`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Purity {
    Pure(Proof),
    Impure(Impure),
}

/// Why a script is not pure: the first effect found, and the calls from `main` to the function
/// that has it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Impure {
    /// What the function that does it does: "calls the foreign function `getpid`".
    pub what: String,
    /// The functions called on the way from `main` to it, by name.
    pub through: Vec<String>,
}

impl Impure {
    /// Said of the script: "`main` → `helper` → `std::process::Command::output`, which calls
    /// the foreign function `posix_spawnp`".
    pub fn said(&self) -> String {
        match self.through.as_slice() {
            // clone: said once, when the harness or a diagnostic reports it
            [] => self.what.clone(),
            // alloc: said once, when the harness or a diagnostic reports it
            names if names.len() <= 4 => format!("{}, which {}", names.join(" → "), self.what),
            // alloc: said once, when the harness or a diagnostic reports it
            names => format!("{} → … → {}, which {}", names[0], names[names.len() - 1], self.what),
        }
    }
}

/// Whether a pure script may read the variable `name`: one cargo gives every build script from
/// the unit alone. `CARGO_MAKEFLAGS` is the jobserver, of one run alone; `RUSTC`, `RUSTDOC`,
/// `RUSTC_WRAPPER`, `RUSTC_WORKSPACE_WRAPPER`, `RUSTC_LINKER` and `CARGO` name programs a pure
/// script never runs; `DEP_*` come from other scripts' runs; anything else is passed through
/// from the shell.
pub fn allowed_env(name: &str) -> bool {
    match name.strip_prefix("CARGO_") {
        Some(rest) => !rest.is_empty() && rest != "MAKEFLAGS",
        None => matches!(name, "TARGET" | "HOST" | "PROFILE" | "OPT_LEVEL" | "DEBUG" | "OUT_DIR" | "NUM_JOBS"),
    }
}

/// Where a path the path analysis proves is made from is rooted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Root {
    /// `CARGO_MANIFEST_DIR`, or a safe relative path (the script runs in its package).
    Package,
    /// `OUT_DIR`.
    OutDir,
}

/// A local's abstract value (design section 6). Flow-insensitive: what every assignment of the
/// local gives, joined ([`Abs::join`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Abs {
    /// Not assigned yet: the join's identity.
    Unset,
    /// A constant string (a `&str`, or a `String`, `OsString` or `PathBuf` made from one).
    Const(Symbol),
    /// The value of an environment variable cargo gives, by name, before it is known what it is
    /// used as: `OUT_DIR`'s is a path rooted at [`Root::OutDir`].
    Env(Symbol),
    /// A path rooted at `root` joined only with safe relative constants.
    Root(Root),
    /// The instance's parameter `i` as it was given: only in summaries, substituted at each call.
    Param(u8),
    /// The instance's parameter `i` joined only with safe relative constants: only in summaries;
    /// at a call it is `Root(r)` where the argument is a path rooted at `r`, else `Unknown`.
    Under(u8),
    Unknown,
}

impl Abs {
    /// The join of two values a local is assigned: equal values stay, `Unset` gives the other,
    /// two paths under one root are a path under it (`Env("OUT_DIR")` and `Root(OutDir)` are
    /// `Root(OutDir)`), parameter `i` and a path under it are a path under it; anything else is
    /// `Unknown`.
    pub fn join(self, other: Abs) -> Abs {
        match (self, other) {
            (Abs::Unset, it) | (it, Abs::Unset) => it,
            (a, b) if a == b => a,
            (Abs::Param(i), Abs::Under(j)) | (Abs::Under(i), Abs::Param(j)) if i == j => Abs::Under(i),
            (a, b) => match (a.as_path(), b.as_path()) {
                (Some(a), Some(b)) if a == b => Abs::Root(a),
                _ => Abs::Unknown,
            },
        }
    }

    /// The value at a call: `Param(i)` and `Under(i)` of the callee's summary given `args`, the
    /// caller's values of its arguments; any other value is its own.
    pub fn substitute(self, args: &[Abs]) -> Abs {
        match self {
            Abs::Param(i) => args.get(usize::from(i)).copied().unwrap_or(Abs::Unknown),
            Abs::Under(i) => match args.get(usize::from(i)).copied() {
                Some(Abs::Param(j)) => Abs::Under(j),
                Some(Abs::Under(j)) => Abs::Under(j),
                Some(arg) => arg.as_path().map_or(Abs::Unknown, Abs::Root),
                None => Abs::Unknown,
            },
            it => it,
        }
    }

    /// The value as a path: a constant that is a safe relative path is a package path, the
    /// manifest directory's and `OUT_DIR`'s values are their roots; anything else is `None`.
    pub fn as_path(self) -> Option<Root> {
        match self {
            Abs::Root(root) => Some(root),
            Abs::Const(text) => safe_relative(text.as_str()).then_some(Root::Package),
            Abs::Env(name) => match name.as_str() {
                "CARGO_MANIFEST_DIR" => Some(Root::Package),
                "OUT_DIR" => Some(Root::OutDir),
                _ => None,
            },
            Abs::Unset | Abs::Param(_) | Abs::Under(_) | Abs::Unknown => None,
        }
    }
}

/// Whether `path`, joined to a directory, stays inside it on every platform a script runs on:
/// not empty, no root, no drive or UNC prefix, no `..`, no `:` (a drive or an alternate stream
/// on Windows) and no NUL.
pub fn safe_relative(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with(['/', '\\'])
        && !path.contains([':', '\0'])
        && path.split(['/', '\\']).all(|part| part != "..")
}
