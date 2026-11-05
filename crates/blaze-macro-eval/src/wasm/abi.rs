//! What a proc macro's module and the host agree on: the names it exports, the shims it may
//! import, and the check that it imports nothing else. `guest/entry.rs` is the module's side
//! of the same names.

use std::fmt;

use wasmtime::{ExternType, Module};

/// The module of the bridge's shims.
pub(crate) const BRIDGE: &str = "blaze_proc_macro";
/// The module of WASI preview 1, which std for `wasm32-wasip1` calls.
pub(crate) const WASI: &str = <blaze_wasi::W32 as blaze_wasi::Width>::MODULE;

/// The bridge's shims, each in `host.rs`:
///
/// - `dispatch(request, len) -> reply_len`: a request of the client's `Closure::call`,
///   answered by [`Server::dispatch`](crate::server::Server::dispatch);
/// - `reply(into)`: the reply's bytes, written where the module reserved `reply_len`;
/// - `output(bytes, len)`: what `run` gave, `Result<_, PanicMessage>` as the bridge encodes
///   it, or the same encoding of a panic the module's hook saw before it aborted;
/// - `hashmap_random_keys(into)`: the 16 bytes of std's `RandomState` keys, the evaluator's
///   fixed ones (`os/random.rs`), so a macro that builds a `HashMap` reads no randomness;
/// - `env_var(name, len, into, capacity) -> len`: one of the calling crate's variables, which
///   the glue's `getenv` asks in place of wasi-libc's (which reads them all), noted read by name;
/// - `current_dir(into, capacity) -> len`: the directory cargo runs rustc in for the calling
///   crate (`Os::current_dir`), which the glue enters before its macro runs, as wasi-libc
///   resolves a relative path against its own working directory (`/` until then).
pub(crate) const BRIDGE_SHIMS: &[&str] =
    &["dispatch", "reply", "output", "hashmap_random_keys", "env_var", "current_dir"];

/// The WASI preview 1 functions std for `wasm32-wasip1` reaches, each `blaze_wasi`'s over the
/// sandbox's answers in `wasi.rs`.
pub(crate) const WASI_SHIMS: &[&str] = blaze_wasi::FUNCTIONS;

/// The module's linear memory.
pub(crate) const MEMORY: &str = "memory";
/// `blaze_proc_macro_alloc(len) -> ptr`: `len` bytes the host writes a call's input to, which
/// [`EXPAND`] takes back.
pub(crate) const ALLOC: &str = "blaze_proc_macro_alloc";
/// `blaze_proc_macro_expand(index, input, len) -> status`: runs the macro `index` (its place in
/// the crate's `__blaze_proc_macros::listed`, sorted by name) on the input at
/// `input`. 0 once it called `output`; 1 for no such macro; [`UNENTERED`] when it could not
/// enter the directory `current_dir` named.
pub(crate) const EXPAND: &str = "blaze_proc_macro_expand";
/// [`EXPAND`]'s status when the glue could not enter the working directory the host named, in
/// its low byte ([`STATUS`]); the bits above are the `errno` `set_current_dir` failed with
/// (WASI's number, as wasi-libc's `errno` is), 0 when there is none.
pub(crate) const UNENTERED: u32 = 2;
/// The bits of [`EXPAND`]'s status that say what it is; the rest is what goes with it.
pub(crate) const STATUS: u32 = 0xFF;
/// A WASI reactor's constructors, run once per instance before anything else.
pub(crate) const INITIALIZE: &str = "_initialize";

/// The exports every module has, and what each is.
const EXPORTS: &[(&str, Kind)] =
    &[(MEMORY, Kind::Memory), (ALLOC, Kind::Func), (EXPAND, Kind::Func), (INITIALIZE, Kind::Func)];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Func,
    Memory,
}

impl Kind {
    fn of(ty: &ExternType) -> Option<Kind> {
        match ty {
            ExternType::Func(_) => Some(Kind::Func),
            ExternType::Memory(_) => Some(Kind::Memory),
            _ => None,
        }
    }
}

/// What makes a module one the host does not run: an import that is no shim, or an export it
/// lacks.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Mismatch {
    /// It imports `module::name`, which is no function the host gives.
    Import { module: String, name: String },
    /// It has no export `name` of the kind the host calls it as.
    Export { name: &'static str },
}

impl fmt::Display for Mismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Mismatch::Import { module, name } => write!(
                f,
                "imports `{module}::{name}`, which is none of the `proc_macro` bridge's shims \
                 (`{BRIDGE}`) or the WASI preview 1 functions the host answers (`{WASI}`): a \
                 macro reaches the system only through them"
            ),
            Mismatch::Export { name } => write!(f, "has no export `{name}`, which the host calls"),
        }
    }
}

/// The first import of `module` that is not a shim's function, or the first export it lacks.
pub(crate) fn check(module: &Module) -> Result<(), Mismatch> {
    for import in module.imports() {
        let shims = match import.module() {
            BRIDGE => BRIDGE_SHIMS,
            WASI => WASI_SHIMS,
            _ => &[],
        };
        let func = matches!(import.ty(), ExternType::Func(_));
        if !func || !shims.contains(&import.name()) {
            return Err(Mismatch::Import {
                module: import.module().to_owned(), // alloc: error path
                name: import.name().to_owned(),     // alloc: error path
            });
        }
    }
    for &(name, kind) in EXPORTS {
        if module.get_export(name).as_ref().and_then(Kind::of) != Some(kind) {
            return Err(Mismatch::Export { name });
        }
    }
    Ok(())
}
