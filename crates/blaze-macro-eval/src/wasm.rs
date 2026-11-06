//! Proc macros as WebAssembly: a proc-macro crate built by blaze for `wasm32-wasip1` with the
//! bridge's glue (`guest/entry.rs`), run in process under Wasmtime. Its `proc_macro` client
//! speaks the pinned bridge's protocol to the same [`Server`] as on the evaluator, whose
//! `usize` is eight bytes from every client (`wire`); its only imports are the bridge's shims and the WASI
//! functions std calls ([`abi`]), each answered by the host ([`host`], [`wasi`]) and traced as
//! [`Os`] traces the evaluator's.
//!
//! A module is compiled once a daemon, through the daemon's [`Modules`], and its machine code
//! kept in the daemon's own store, from where alone it is loaded again ([`code`]). Each
//! run is a fresh instance, given the state the calls before it left: the statics of the
//! macro's crates its code can write, which the link lists in the module ([`statics`]), are
//! written after the instance's `_initialize` and before its macro runs, and what the macro
//! changed of them is what it left ([`Ran::statics`]), as the evaluator says it. A static left
//! holding an address of the heap is said to hold one, as the evaluator says it: a fresh
//! instance has no such heap, so the session's earlier calls are then given with each next
//! one, and the run makes the heap again by running them in order in its instance first
//! ([`Library::run`]). An instance runs on an expansion thread, never on a fiber, and the job
//! asking for it answers what it asks of the database ([`thread`]).
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
mod statics;
mod thread;
pub use thread::alive as expansion_threads;
// Wasmtime's embedder-provided TLS, where it uses `sys::custom` (the wasm seed).
#[cfg(not(any(unix, windows)))]
mod tls;
mod wasi;

#[cfg(test)]
mod tests;

use std::{fmt, sync::Arc};

use base_db::{Crate, Db, Env};
use blaze_cas::{Kind, Origin, Reader};
use blaze_ids::ContentId;
use hir_def::StaticId;
use proc_macro_api::kept::{Keeps, Put, put};
use span::{Edition, Span};
use wasmtime::InstancePre;

use self::{
    abi::Mismatch,
    code::{Codes, module_id},
    engine::Engines,
    host::{Cx, Host, Watch},
    statics::{List, Statics},
};
pub use self::{
    code::Loads,
    glue::{MODULE as GLUE_MODULE, wasm_glue},
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
    /// Its list of the statics its code can write is missing or wrong, or names a static of
    /// none of its crates (`statics.rs`).
    Statics(String),
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoadError::Compile(why) => write!(f, "does not compile: {why}"),
            LoadError::Serialize(why) => {
                write!(f, "compiled, but its machine code can't be kept: {why}")
            }
            LoadError::Cached(why) => {
                write!(f, "has machine code in the store that can't be used: {why}")
            }
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
            LoadError::Statics(why) => f.write_str(why),
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
        f.debug_struct("Modules")
            .field("code", &self.engines.code)
            .field("loads", &self.loads())
            .finish()
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

/// A proc-macro crate's module, ready to instantiate once per expansion, and the statics its
/// expansions watch and carry ([`Library::watch`]).
pub(crate) struct Library {
    id: ContentId,
    pre: InstancePre<Host>,
    list: List,
    statics: Arc<Statics>,
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
    pub(crate) fn load(
        modules: &Modules,
        keeps: &Keeps,
        bytes: &[u8],
        what: &str,
    ) -> Result<Library, LoadError> {
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
        let linker =
            host::linker(&engines.engine).map_err(|e| LoadError::Link(format!("{e:#}")))?;
        // alloc: error path
        let pre = linker
            .instantiate_pre(&loaded.module)
            .map_err(|e| LoadError::Link(format!("{e:#}")))?;
        let list = List::parse(bytes).map_err(LoadError::Statics)?;
        // alloc: once a module loaded
        let statics = Arc::new(Statics::unresolved(&list));
        Ok((Library { id, pre, list, statics }, loaded.put))
    }

    /// Finds each static its list names among the crates `krate`, the proc-macro crate it was
    /// built from, reaches: the ones its expansions watch and carry, the standard library's
    /// aside. Until then, a module whose list names any runs nothing.
    pub(crate) fn watch(&mut self, db: &Db, krate: Crate) -> Result<(), LoadError> {
        // alloc: once a module loaded
        self.statics =
            Arc::new(Statics::resolve(db, krate, &self.list).map_err(LoadError::Statics)?);
        Ok(())
    }

    /// The module's id, which the keys of its expansions say.
    pub(crate) fn id(&self) -> ContentId {
        self.id
    }

    /// What the macros of the proc-macro crate whose code is `code` run in this module: the
    /// crate's code and the module's id, so a module the code generator made otherwise from the
    /// same source (a fix of a backend, a stencil or LLVM) runs other code.
    pub(crate) fn runs(&self, code: ContentId) -> ContentId {
        blaze_ids::Hasher::new("the code a proc-macro crate's macros run in its wasm module 1")
            .part(&code.0)
            .part(&self.id.0)
            .finish()
    }

    /// Runs `expansions` in order in one fresh instance, each the macro at its
    /// [`Expansion::index`], with the calling crate's `env`, from the statics `preset` (what the
    /// calls before them left, as [`Ran::statics`] said each), in the working directory `cwd`
    /// (the call's, which the module's glue enters): what the last gave, and what all of them
    /// did. The ones before the last are the session's earlier calls, run again for the state
    /// they leave on the heap, as on the evaluator ([`run::run`](crate::run::run)).
    pub(crate) fn run(
        &self,
        db: &Db,
        env: &Env,
        expansions: &[Expansion<'_>],
        preset: &[(StaticId, Vec<u8>)],
        cwd: &str,
        spent: &mut Spent,
    ) -> Result<Ran, Stop> {
        spent.evaluated = false;
        // alloc: once per run, what each of its calls is given
        let calls: Vec<(u32, Given<'_>)> = expansions
            .iter()
            .map(|it| {
                let given = Given {
                    inputs: it.inputs,
                    sites: it.sites,
                    edition: it.edition,
                    forced_keywords: it.forced_keywords,
                };
                (it.index, given)
            })
            .collect();
        self.expand(db, env, &calls, preset, cwd, spent)
    }

    /// Runs `calls` (each a macro's index and what it is given) in order in one fresh instance
    /// from the statics `preset`, on an expansion thread ([`thread`]), what they ask of the
    /// database answered from `db` on this one: what the last gave, and what all of them did.
    fn expand(
        &self,
        db: &Db,
        env: &Env,
        calls: &[(u32, Given<'_>)],
        preset: &[(StaticId, Vec<u8>)],
        cwd: &str,
        spent: &mut Spent,
    ) -> Result<Ran, Stop> {
        let Some((_, last)) = calls.last() else {
            return Err(Stop("was given nothing to expand".to_owned())); // alloc: error path
        };
        let statics = &*self.statics;
        if !statics.resolved {
            return Err(Stop(format!(
                "ran from a module whose `{}` list was not resolved against its crates, so the state \
                 it keeps between calls can't be named",
                statics::SECTION
            )));
        }
        // alloc: once per run, where the state it is given is written
        let mut writes = Vec::with_capacity(preset.len());
        for (id, bytes) in preset {
            let Some(places) = statics.places(*id, bytes) else {
                // alloc: error path
                return Err(Stop(format!(
                    "was given {} bytes of state for a static its module does not list as {:?}",
                    bytes.len(),
                    id
                )));
            };
            writes.extend(places.map(|(at, part)| (at, part.to_vec()))); // alloc: as above
        }
        let mut watch = Watch {
            statics: Arc::clone(&self.statics), // clone: a refcount, given to the expansion's thread
            writes,
            before: Vec::new(),
            after: Vec::new(),
            memory_len: 0,
            started: false,
        };
        // alloc: once per run, each call's server and input bytes, given to the expansion's thread
        let mut runs: Vec<host::Call> = calls
            .iter()
            .map(|(index, given)| {
                let mut server = Server::new(given.sites, given.edition, given.forced_keywords);
                let mut input = Vec::new(); // alloc: as above
                server.input(given.inputs, &mut input);
                host::Call { index: *index, server, input }
            })
            .collect();
        // The run's server until its first call's takes its place ([`host::expand`]).
        let server = Server::new(last.sites, last.edition, last.forced_keywords);
        // clone: a handle of the module's code, given to the expansion's thread
        let pre = self.pre.clone();
        // clone: the calling crate's variables, given to the expansion's thread once per run
        let env = env.clone();
        // alloc: the call's directory, given to the expansion's thread once per run
        let cwd = cwd.to_owned();
        let (ran, used, watch) =
            thread::run(db, move |link: &thread::Link<(Result<Ran, Stop>, host::Used, Watch)>| {
                let mut cx = Cx::new(link, server, Os::wasm_in(&env, &cwd));
                let (output, used) = host::expand(&pre, &mut cx, &mut runs, &mut watch);
                let Cx { server, os, .. } = cx;
                let ran = output.map(|output| Ran {
                    output,
                    asked: server.asked,
                    tracked: server.tracked,
                    emitted: server.emitted,
                    // alloc: once per run, what its calls read, given to the key
                    seen: os.seen().map(|(seen, what)| (seen, what.to_owned())).collect(),
                    unkeyable: os.unkeyable().map(str::to_owned),
                    statics: Vec::new(),
                });
                (ran, used, watch)
            })?;
        spent.steps = used.fuel;
        spent.peak_bytes = used.peak_bytes;
        spent.over = used.over;
        let mut ran = ran?;
        ran.statics = self.left(&watch);
        Ok(ran)
    }

    /// The statics `watch` saw the run change, each with its bytes as the run left them and
    /// whether they hold an address of the instance's heap, which a next instance has only
    /// when the calls that made it run again in it first ([`Library::run`]).
    fn left(&self, watch: &Watch) -> Vec<(StaticId, Vec<u8>, bool)> {
        let statics = &*self.statics;
        // alloc: once per run, what it left (nothing for most)
        let mut left = Vec::new();
        if !watch.started {
            return left;
        }
        for (at, watched) in statics.watched.iter().enumerate() {
            let span = statics.span(at);
            let (before, after) = (&watch.before[span.clone()], &watch.after[span]);
            if before == after {
                continue;
            }
            let holds = statics.holds_heap_address(at, after, watch.memory_len);
            left.push((watched.id, after.to_vec(), holds)); // alloc: as above
        }
        left
    }
}
