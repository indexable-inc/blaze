//! A module's machine code: compiled once a daemon, and kept in the daemon's store as a
//! [`Kind::Machine`] under [`code_key`] (the module's id and the engine's code id), so the next
//! daemon loads it without compiling.
//!
//! What the store keeps here is machine code the daemon runs, and `Module::deserialize`
//! checks only the engine's version and settings, never that the code is what the module
//! compiles to. So it is loaded only from machine code this daemon's own compile kept: a
//! [`Kind::Machine`] in the store's own directory ([`Origin::Own`]). Anything else under the
//! key is [`LoadError::NotLocal`], and never deserialized: a value of another kind (what a
//! fetch from a remote keeps, since `blaze_cas`'s fetch refuses a `Kind::Machine`) and a
//! value in a store behind the daemon's (a layer, another writer of a shared directory).
//!
//! Each load is a `macro module` span ([`MacroModule`]) saying where the code came from and
//! how many bytes it is, and counts in its source's [`Plot`].

use std::sync::atomic::{AtomicU64, Ordering};

use blaze_cas::{Guard, Key, Kind, Origin, Reader};
use blaze_hash::SharedMap;
use blaze_ids::{ContentId, Hasher};
use blaze_profile::{
    Plot,
    span::{MacroModule, MacroModuleLoaded, ModuleFrom, Name},
};
use proc_macro_api::kept::{Keep, Put};
use wasmtime::Module;

use super::{LoadError, engine::Engines};

/// The id of a module, by its bytes.
pub(crate) fn module_id(bytes: &[u8]) -> ContentId {
    ContentId::of("blaze v2 wasm proc macro module 1", bytes)
}

/// The store key the machine code of module `module` from `engines` is kept under.
pub(crate) fn code_key(module: &ContentId, engines: &Engines) -> Key {
    Key(Hasher::new("blaze v2 wasm proc macro machine code 2")
        .id(module)
        .id(&engines.code)
        .finish())
}

/// How many loads each way went: what the `macro modules ...` counters plot.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Loads {
    /// Found in the daemon's memo.
    pub memo: u64,
    /// Loaded from machine code in the daemon's own store.
    pub store: u64,
    /// Compiled.
    pub compiled: u64,
}

/// The modules compiled or loaded by a daemon, by id, and how each load went.
#[derive(Default)]
pub(crate) struct Codes {
    memo: SharedMap<ContentId, Module>,
    from_memo: AtomicU64,
    from_store: AtomicU64,
    compiled: AtomicU64,
}

/// A module made ready to instantiate.
pub(crate) struct Loaded {
    pub(crate) module: Module,
    /// Its machine code, for the store's owner to publish, when this compiled it and a store is
    /// installed.
    pub(crate) put: Option<Put>,
}

/// Bytes the daemon's own compile kept as machine code: what [`local`] found, and the only
/// thing [`deserialize`] takes.
struct Local<'g>(&'g [u8]);

impl Codes {
    pub(crate) fn loads(&self) -> Loads {
        Loads {
            memo: self.from_memo.load(Ordering::Relaxed),
            store: self.from_store.load(Ordering::Relaxed),
            compiled: self.compiled.load(Ordering::Relaxed),
        }
    }

    /// The module `bytes` (of id `id`, named `what` in errors, its span and its put): from
    /// this daemon's memo, else its machine code in `store`, else compiled.
    pub(crate) fn load(
        &self,
        engines: &Engines,
        id: ContentId,
        bytes: &[u8],
        store: Option<&Reader>,
        what: &str,
    ) -> Result<Loaded, LoadError> {
        // A load that fails drops the span, which reads as one that did not end.
        let span = MacroModule { module: Name::of(what) }.open();
        let (loaded, from, code_bytes) = self.ready(engines, id, bytes, store, what)?;
        let (counter, plot) = match from {
            ModuleFrom::Memo => (&self.from_memo, Plot::MacroModuleMemo),
            ModuleFrom::Store => (&self.from_store, Plot::MacroModuleStore),
            ModuleFrom::Compiled => (&self.compiled, Plot::MacroModuleCompiled),
        };
        blaze_profile::plot(plot, counter.fetch_add(1, Ordering::Relaxed) + 1);
        span.ended(MacroModuleLoaded { from, code_bytes });
        Ok(loaded)
    }

    /// [`Codes::load`]'s module, where it came from, and its machine code's bytes when it read
    /// or made them.
    fn ready(
        &self,
        engines: &Engines,
        id: ContentId,
        bytes: &[u8],
        store: Option<&Reader>,
        what: &str,
    ) -> Result<(Loaded, ModuleFrom, Option<u64>), LoadError> {
        if let Some(module) = self.memo.pin().get(&id) {
            // clone: a module shares its code, so this copies a pointer
            return Ok((Loaded { module: module.clone(), put: None }, ModuleFrom::Memo, None));
        }
        let key = code_key(&id, engines);
        if let Some(store) = store {
            let guard = blaze_cas::enter();
            if let Some(code) = local(store, &guard, &key)? {
                let len = code.0.len() as u64;
                let module = deserialize(engines, &code)?;
                // clone: as above
                self.memo.pin().insert(id, module.clone());
                name_code(&module, what);
                return Ok((Loaded { module, put: None }, ModuleFrom::Store, Some(len)));
            }
        }
        // alloc: error path
        let module = Module::new(&engines.engine, bytes)
            .map_err(|e| LoadError::Compile(format!("{e:#}")))?;
        let put = match store {
            Some(_) => Some(Put {
                keep: Keep::Machine(key),
                // alloc: error path
                value: module.serialize().map_err(|e| LoadError::Serialize(format!("{e:#}")))?,
                // alloc: once per module compiled, what its put says it is
                what: format!("the machine code of {what}"),
            }),
            None => None,
        };
        let code_bytes = put.as_ref().map(|it| it.value.len() as u64);
        // clone: as above
        self.memo.pin().insert(id, module.clone());
        name_code(&module, what);
        Ok((Loaded { module, put }, ModuleFrom::Compiled, code_bytes))
    }
}

/// Registers `module`'s machine code (its `.text`, where Wasmtime runs it) and its functions'
/// places in it with the profiler's code maps (`ix_prof::jit`), so a CPU sample inside a proc
/// macro names the macro's function, not `[code in no image]`. The map keeps the module, as
/// the memo does for the daemon's life, so its range never names freed code. A module two
/// threads compiled at once is registered for each copy; the one the memo dropped stays alive
/// in the map.
fn name_code(module: &Module, what: &str) {
    let text = module.text();
    let functions = module
        .functions()
        .map(|it| ix_prof::jit::Function {
            offset: it.offset as u64,
            len: it.len as u64,
            name: it.name.as_deref().map(ix_prof::symbols::demangle),
            index: it.index.as_u32(),
        })
        // alloc: once per module loaded, kept for the daemon's life as the module is
        .collect();
    ix_prof::jit::register(
        ix_prof::jit::CodeMap {
            start: text.as_ptr().addr() as u64,
            len: text.len() as u64,
            // alloc: as above
            module: format!("wasm module {what}"),
            functions,
        },
        // clone: a module shares its code, so this copies a pointer
        // alloc: as above
        Box::new(module.clone()),
    );
}

/// The machine code `store` keeps under `key`, when its own directory keeps it as a
/// [`Kind::Machine`]; any other value under the key is refused, unread past its kind.
fn local<'g>(
    store: &'g Reader,
    guard: &'g Guard,
    key: &Key,
) -> Result<Option<Local<'g>>, LoadError> {
    match store.get_from(guard, key) {
        Ok(None) => Ok(None),
        Ok(Some((Origin::Own, entry))) if entry.kind == Kind::Machine => {
            Ok(Some(Local(entry.bytes)))
        }
        Ok(Some((origin, entry))) => Err(LoadError::NotLocal { origin, kind: entry.kind }),
        // alloc: error path
        Err(corrupt) => Err(LoadError::Cached(corrupt.to_string())),
    }
}

/// `code`, machine code `Module::serialize` made for this engine, as a module.
#[allow(unsafe_code)]
fn deserialize(engines: &Engines, code: &Local<'_>) -> Result<Module, LoadError> {
    // SAFETY: `Module::deserialize` runs what it is given as machine code, checking only the
    // engine's version and settings. `code` is a `Local`, which only `local` makes: a
    // `Kind::Machine` in the daemon's own store directory, verified against its content hash
    // by the store's read. Only the daemon's own compile puts that kind there (`Codes::ready`
    // through `Keep::Machine`); `blaze_cas::Store::take_fetched` refuses a fetched
    // `Kind::Machine`, and a layer's or another writer's value reads as `Origin::Behind`,
    // which `local` refuses. The key holds the engine's code id, so the code is of this
    // Wasmtime and these settings, which Wasmtime checks again. The directory's own files
    // are trusted as the daemon's binary is: only the daemon writes them.
    let module = unsafe { Module::deserialize(&engines.engine, code.0) };
    // alloc: error path
    module.map_err(|e| LoadError::Cached(format!("its kept machine code does not load: {e:#}")))
}
