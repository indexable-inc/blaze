//! Proc macros as WebAssembly: a proc-macro crate built by blaze for `wasm32-wasip1` with the
//! bridge's glue (`guest/entry.rs`), run in process under Wasmtime. Its `proc_macro` client
//! speaks the pinned bridge's protocol to the same [`Server`] as on the evaluator, whose
//! `usize` is eight bytes from every client (`wire`); its only imports are the bridge's shims and the WASI
//! functions std calls ([`abi`]), each answered by the host ([`host`], [`wasi`]) and traced as
//! [`Os`] traces the evaluator's.
//!
//! A module is compiled once a daemon, through the daemon's [`Modules`], and its machine code
//! kept in the daemon's own store, from where alone it is loaded again ([`code`]). Each
//! expansion runs in a fresh instance, so no expansion sees what another left: a library of
//! modules is stateless, and its calls are kept and found as any other run of user code. An
//! instance runs on an expansion thread, never on a fiber, and the job asking for it answers
//! what it asks of the database ([`thread`]).
//!
//! Every way a module can't be loaded or run is an error naming the macro ([`LoadError`],
//! [`Stop`]); nothing runs it on the evaluator instead.
//!
//! [`Server`]: crate::server::Server
//! [`Os`]: crate::os::Os

mod abi;
mod code;
mod engine;
mod glue;
mod host;
mod thread;
mod wasi;

#[cfg(test)]
mod tests;

use std::fmt;

use base_db::{Db, Env};
use blaze_cas::{Kind, Origin, Reader};
use blaze_ids::ContentId;
use proc_macro_api::kept::{Keeps, Put, put};
use span::{Edition, Span};
use wasmtime::InstancePre;

pub use self::{
    code::Loads,
    glue::{MODULE as GLUE_MODULE, wasm_glue},
};
use self::{
    abi::Mismatch,
    code::{Codes, module_id},
    engine::Engines,
    host::{Cx, Host},
};
use crate::{
    guest::Stop,
    os::Os,
    run::{Expansion, Ran, Spent},
    server::Server,
    stream::Stream,
};

/// Why a module can't be loaded: each said after "its wasm module".
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum LoadError {
    /// The module's bytes did not compile.
    Compile(String),
    /// Its machine code could not be made into bytes to keep.
    Serialize(String),
    /// What the store keeps of it could not be read or loaded.
    Cached(String),
    /// What the store has under its machine code's key is not machine code this daemon's
    /// compile kept: a value of another `kind` in its own directory (a fetch from a remote
    /// keeps every kind but machine code), or one in a store behind it. Never loaded.
    NotLocal { origin: Origin, kind: Kind },
    /// It imports or lacks what the host does not give or call.
    Mismatch(Mismatch),
    /// Its imports did not link against the host's shims.
    Link(String),
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoadError::Compile(why) => write!(f, "does not compile: {why}"),
            LoadError::Serialize(why) => write!(f, "compiled, but its machine code can't be kept: {why}"),
            LoadError::Cached(why) => write!(f, "has machine code in the store that can't be used: {why}"),
            LoadError::NotLocal { origin: Origin::Own, kind } => write!(
                f,
                "has a {} value in the daemon's store under its machine code's key, where only the \
                 daemon's own compile puts machine code: it came from elsewhere (a remote's fetch), \
                 and is not run",
                kind.name()
            ),
            LoadError::NotLocal { origin: Origin::Behind(dir), kind } => write!(
                f,
                "has a {} value under its machine code's key in {dir}, a store behind the daemon's \
                 own: only machine code the daemon compiled is run",
                kind.name()
            ),
            LoadError::Mismatch(mismatch) => write!(f, "{mismatch}"),
            LoadError::Link(why) => write!(f, "does not link against the host's shims: {why}"),
        }
    }
}

/// A daemon's wasm modules: its Wasmtime engine, and each module it compiled or loaded, by
/// id, with how each load went ([`Modules::loads`]). Made once where the daemon starts (beside
/// its [`Macros`](crate::Macros)) and lent to every [`load_wasm`](crate::load_wasm). `Send`
/// and `Sync`: any worker loads through it at once, and what a load makes (a [`Library`]'s
/// `InstancePre`, a shared handle of the engine) goes to the expansion threads by value.
pub struct Modules {
    engines: Engines,
    codes: Codes,
}

const _: fn() = || {
    fn shared<T: Send + Sync>() {}
    shared::<Modules>();
};

impl Modules {
    /// The engine with its settings (`engine.rs`) and no module yet, or why Wasmtime could
    /// make no engine.
    pub fn new() -> Result<Modules, String> {
        Ok(Modules { engines: Engines::new()?, codes: Codes::default() })
    }

    /// How its loads went so far: what the profile's `macro modules ...` counters plot.
    pub fn loads(&self) -> Loads {
        self.codes.loads()
    }
}

impl fmt::Debug for Modules {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Modules").field("code", &self.engines.code).field("loads", &self.loads()).finish()
    }
}

/// What one expansion is given, of its [`Expansion`]: a module runs macros by index, so it
/// needs no function.
struct Given<'a> {
    inputs: &'a [Stream],
    /// The def, call and mixed sites.
    sites: [Span; 3],
    edition: Edition,
    forced_keywords: bool,
}

/// A proc-macro crate's module, ready to instantiate once per expansion.
pub(crate) struct Library {
    id: ContentId,
    pre: InstancePre<Host>,
}

impl fmt::Debug for Library {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Library").field("id", &self.id).finish_non_exhaustive()
    }
}

impl Library {
    /// The module `bytes`, of the crate `what` names, loaded with the daemon's `modules` from
    /// the store its expansions are kept in (`keeps`); a module it compiled is left for that
    /// store's owner to publish. Keeping nothing, it compiles and keeps nothing.
    pub(crate) fn load(modules: &Modules, keeps: &Keeps, bytes: &[u8], what: &str) -> Result<Library, LoadError> {
        let store = match keeps {
            Keeps::In(keeping) => Some(&keeping.expansions.store),
            Keeps::Nothing(_) => None,
        };
        let (library, kept) = Library::load_with(modules, bytes, store, what)?;
        if let Some(kept) = kept {
            put(kept);
        }
        Ok(library)
    }

    /// [`Library::load`] with `store`, giving back the put of a module it compiled.
    pub(crate) fn load_with(
        modules: &Modules,
        bytes: &[u8],
        store: Option<&Reader>,
        what: &str,
    ) -> Result<(Library, Option<Put>), LoadError> {
        let Modules { engines, codes } = modules;
        let id = module_id(bytes);
        let loaded = codes.load(engines, id, bytes, store, what)?;
        abi::check(&loaded.module).map_err(LoadError::Mismatch)?;
        // alloc: error path
        let linker = host::linker(&engines.engine).map_err(|e| LoadError::Link(format!("{e:#}")))?;
        // alloc: error path
        let pre = linker.instantiate_pre(&loaded.module).map_err(|e| LoadError::Link(format!("{e:#}")))?;
        Ok((Library { id, pre }, loaded.put))
    }

    /// The module's id, which the keys of its expansions say.
    pub(crate) fn id(&self) -> ContentId {
        self.id
    }

    /// Runs the macro at `index` (of `run::functions`) on the one expansion of `expansions`,
    /// with the calling crate's `env`. A fresh instance has no state, so there are no earlier
    /// calls to run again; given some, it stops.
    pub(crate) fn run(
        &self,
        db: &Db,
        env: &Env,
        index: u32,
        expansions: &[Expansion<'_>],
        spent: &mut Spent,
    ) -> Result<Ran, Stop> {
        spent.evaluated = false;
        let [expansion] = expansions else {
            // alloc: error path
            return Err(Stop(format!(
                "was given {} calls to run in one instance of its wasm module, which runs each in a \
                 fresh one",
                expansions.len()
            )));
        };
        let given = Given {
            inputs: expansion.inputs,
            sites: expansion.sites,
            edition: expansion.edition,
            forced_keywords: expansion.forced_keywords,
        };
        self.expand(db, env, index, &given, spent)
    }

    /// Runs the macro at `index` on `given` in a fresh instance, on an expansion thread
    /// ([`thread`]), what it asks of the database answered from `db` on this one.
    fn expand(&self, db: &Db, env: &Env, index: u32, given: &Given<'_>, spent: &mut Spent) -> Result<Ran, Stop> {
        let mut server = Server::new(given.sites, given.edition, given.forced_keywords);
        // alloc: once per expansion, its input's bytes
        let mut input = Vec::new();
        server.input(given.inputs, &mut input);
        // clone: a handle of the module's code, given to the expansion's thread
        let pre = self.pre.clone();
        // clone: the calling crate's variables, given to the expansion's thread once per expansion
        let env = env.clone();
        let (ran, used) = thread::run(db, move |link: &thread::Link<(Result<Ran, Stop>, host::Used)>| {
            let mut cx = Cx::new(link, server, Os::wasm(&env));
            let (output, used) = host::expand(&pre, &mut cx, index, &input);
            let Cx { server, os, .. } = cx;
            let ran = output.map(|output| Ran {
                output,
                asked: server.asked,
                tracked: server.tracked,
                emitted: server.emitted,
                // alloc: once per expansion, what it read, given to the key
                seen: os.seen().map(|(seen, what)| (seen, what.to_owned())).collect(),
                unkeyable: os.unkeyable().map(str::to_owned),
                statics: Vec::new(),
            });
            (ran, used)
        })?;
        spent.steps = used.fuel;
        spent.peak_bytes = used.peak_bytes;
        spent.over = used.over;
        ran
    }
}
