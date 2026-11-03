//! One expansion in a fresh instance of a macro's module: its input written to the module's
//! memory, its macro run, and each request its `proc_macro` client makes answered by
//! [`Server::dispatch`] through the bridge's shims ([`abi::BRIDGE_SHIMS`]); the WASI calls of
//! its std by `blaze_wasi` over the sandbox's [`wasi`].
//!
//! A Wasmtime store's data is `'static`, and what a call answers from borrows the calling
//! crate's environment and the [`Answer`] for the call. So the store holds a pointer to the
//! call's [`Cx`], set for the call and the store dropped before the call returns: this file is
//! the one place it is dereferenced.
//!
//! [`expand`] runs on an expansion thread, never a fiber's ([`super::thread`]): what it asks
//! of the database goes back to the job through the [`Answer`].

use std::ptr::NonNull;

use wasmtime::{Caller, Engine, InstancePre, Linker, Memory, ResourceLimiter, Store, Trap, TypedFunc};

use super::{
    abi::{self, BRIDGE},
    statics::Statics,
    wasi,
};
use crate::{
    guest::Stop,
    os::{HASHMAP_KEYS, Os},
    run::{MEMORY, Over},
    server::{Answer, Server},
    stream::Stream,
};

/// The fuel one expansion may spend, about one unit a wasm instruction: a derive of serde's
/// size runs some hundreds of millions natively, so this is tens of times that.
pub(crate) const FUEL: u64 = 1 << 34;

/// The most elements a module's tables may hold.
const TABLE_MAX: usize = 1 << 20;

/// What a call's shims answer from and write to.
pub(crate) struct Cx<'a> {
    /// What only the database can say of a span: the job's, across the thread's link.
    pub(crate) answer: &'a dyn Answer, // dyn: the link of any expansion's result type, the edge
    pub(crate) server: Server,
    pub(crate) os: Os<'a>,
    /// The reply to the last request, which the module reads with `reply`.
    reply: Vec<u8>,
    /// What the module gave through `output`.
    output: Option<Vec<u8>>,
    /// Why a shim stopped the expansion.
    stop: Option<Stop>,
    /// The bytes of what a WASI call gives the module, reused.
    pub(crate) scratch: Vec<u8>,
}

impl<'a> Cx<'a> {
    pub(crate) fn new(answer: &'a dyn Answer, server: Server, os: Os<'a>) -> Cx<'a> { // dyn: as above
        // alloc: none until the first request, then reused
        Cx { answer, server, os, reply: Vec::new(), output: None, stop: None, scratch: Vec::new() }
    }
}

/// What an expansion's memory grew to, and whether it asked for more than [`MEMORY`].
#[derive(Default)]
pub(crate) struct Limits {
    pub(crate) peak: usize,
    pub(crate) over: bool,
}

impl ResourceLimiter for Limits {
    fn memory_growing(&mut self, _current: usize, desired: usize, _maximum: Option<usize>) -> wasmtime::Result<bool> {
        if desired as u64 > MEMORY {
            self.over = true;
            return Ok(false);
        }
        self.peak = self.peak.max(desired);
        Ok(true)
    }

    fn table_growing(&mut self, _current: usize, desired: usize, _maximum: Option<usize>) -> wasmtime::Result<bool> {
        Ok(desired <= TABLE_MAX)
    }
}

/// A store's data: the call's [`Cx`] while the call runs, and the instance's memory.
pub(crate) struct Host {
    cx: Option<NonNull<Cx<'static>>>,
    memory: Option<Memory>,
    limits: Limits,
}

/// The error a shim's stop ends the instance with; the stop itself is in the [`Cx`].
fn stopped() -> wasmtime::Error {
    wasmtime::Error::msg("a shim of blaze's stopped the expansion")
}

/// Runs `f` on the module's memory and the call's [`Cx`]; a [`Stop`] it returns is kept and
/// ends the instance.
#[allow(unsafe_code)]
fn with<R>(caller: &mut Caller<'_, Host>, f: impl FnOnce(&mut [u8], &mut Cx<'_>) -> Result<R, Stop>) -> wasmtime::Result<R> {
    let Some(memory) = caller.data().memory else {
        return Err(wasmtime::Error::msg("the module called the host before its memory was found"));
    };
    let (mem, host) = memory.data_and_store_mut(caller);
    let Some(mut cx) = host.cx else {
        return Err(wasmtime::Error::msg("the module called the host outside an expansion"));
    };
    // SAFETY: `expand` sets the pointer from a `&mut Cx` it holds for as long as the store
    // lives, the store is dropped before `expand` returns, and the module runs on this thread
    // only: no other reference to the `Cx` is live while a shim runs.
    let cx = unsafe { cx.as_mut() };
    match f(mem, &mut *cx) {
        Ok(it) => Ok(it),
        Err(stop) => {
            cx.stop.get_or_insert(stop);
            Err(stopped())
        }
    }
}

/// The linker of every shim the host gives, which [`abi::check`] checks a module against.
pub(crate) fn linker(engine: &Engine) -> wasmtime::Result<Linker<Host>> {
    let mut l = Linker::new(engine);
    bridge(&mut l)?;
    blaze_wasi::link::<blaze_wasi::W32, Host>(&mut l)?;
    Ok(l)
}

fn bridge(l: &mut Linker<Host>) -> wasmtime::Result<()> {
    l.func_wrap(BRIDGE, "dispatch", |mut c: Caller<'_, Host>, request: u32, len: u32| {
        with(&mut c, |mem, cx| {
            let request = wasi::bytes(mem, request, len)?;
            cx.server.dispatch(cx.answer, request, &mut cx.reply)?;
            Ok(cx.reply.len() as u32)
        })
    })?;
    l.func_wrap(BRIDGE, "reply", |mut c: Caller<'_, Host>, into: u32| {
        with(&mut c, |mem, cx| {
            wasi::put(mem, into, &cx.reply)?;
            cx.reply.clear();
            Ok(())
        })
    })?;
    l.func_wrap(BRIDGE, "output", |mut c: Caller<'_, Host>, bytes: u32, len: u32| {
        with(&mut c, |mem, cx| {
            if cx.output.is_some() {
                return Err(Stop("gave its output twice".to_owned())); // alloc: error path
            }
            cx.output = Some(wasi::bytes(mem, bytes, len)?.to_vec()); // alloc: once per expansion, its output
            Ok(())
        })
    })?;
    l.func_wrap(BRIDGE, "hashmap_random_keys", |mut c: Caller<'_, Host>, into: u32| {
        with(&mut c, |mem, _| {
            let (k0, k1) = HASHMAP_KEYS;
            wasi::put(mem, into, &k0.to_le_bytes())?;
            wasi::put(mem, into.wrapping_add(8), &k1.to_le_bytes())
        })
    })?;
    l.func_wrap(BRIDGE, "env_var", |mut c: Caller<'_, Host>, name: u32, len: u32, into: u32, capacity: u32| {
        with(&mut c, |mem, cx| env_var(mem, cx, name, len, into, capacity))
    })?;
    l.func_wrap(BRIDGE, "current_dir", |mut c: Caller<'_, Host>, into: u32, capacity: u32| {
        with(&mut c, |mem, cx| current_dir(mem, cx, into, capacity))
    })?;
    Ok(())
}

/// `current_dir(into, capacity) -> len`: the directory cargo runs rustc in for the calling
/// crate ([`Os::current_dir`]), written at `into` when it fits in `capacity`: its length, or -1
/// when the expansion was given none. The glue enters it before the macro runs. Noted as
/// nothing: the expansion's key holds it.
fn current_dir(mem: &mut [u8], cx: &mut Cx<'_>, into: u32, capacity: u32) -> Result<i32, Stop> {
    let Some(dir) = cx.os.current_dir() else { return Ok(-1) };
    let Ok(n) = i32::try_from(dir.len()) else {
        // alloc: error path
        return Err(Stop(format!("was given a working directory of {} bytes, more than a 32-bit memory holds", dir.len())));
    };
    if dir.len() <= capacity as usize {
        wasi::put(mem, into, dir.as_bytes())?;
    }
    Ok(n)
}

/// `env_var(name, len, into, capacity) -> len`: the calling crate's variable of the `len`-byte
/// name at `name`, noted read by name ([`Os::var`]), written at `into` when it fits in
/// `capacity`: its length, or -1 when it is unset. The module's `getenv` asks this one name at a
/// time, so its expansion is keyed by the variables it read alone.
fn env_var(mem: &mut [u8], cx: &mut Cx<'_>, name: u32, len: u32, into: u32, capacity: u32) -> Result<i32, Stop> {
    let name = wasi::bytes(mem, name, len)?;
    let Ok(name) = std::str::from_utf8(name) else {
        // alloc: error path
        return Err(Stop(format!("read the variable {:?}, whose name is not UTF-8", String::from_utf8_lossy(name))));
    };
    let Some(value) = cx.os.var(name) else { return Ok(-1) };
    let Ok(n) = i32::try_from(value.len()) else {
        // alloc: error path
        return Err(Stop(format!("read a variable of {} bytes, more than a 32-bit memory holds", value.len())));
    };
    if value.len() <= capacity as usize {
        wasi::put(mem, into, value.as_bytes())?;
    }
    Ok(n)
}

/// The WASI calls' way to a module's memory and the call's [`Cx`], the macro sandbox's
/// [`blaze_wasi::System`] (`wasi.rs`).
impl blaze_wasi::Embed for Host {
    fn with<R>(
        caller: &mut Caller<'_, Host>,
        f: impl FnOnce(&mut [u8], &mut dyn blaze_wasi::System) -> Result<R, Stop>, // dyn: `blaze_wasi`'s calls, shared with the seed runner
    ) -> wasmtime::Result<R> {
        with(caller, |mem, cx| f(mem, cx))
    }
}

/// The statics an expansion is given and watched by: each written after the module's
/// `_initialize` and before its macro runs (`writes`, an address and the bytes), then every
/// watched static's bytes read (`before`), and read again once the macro returned (`after`),
/// with the memory's size then (`memory_len`).
pub(crate) struct Watch {
    pub(crate) statics: std::sync::Arc<Statics>,
    pub(crate) writes: Vec<(u32, Vec<u8>)>,
    pub(crate) before: Vec<u8>,
    pub(crate) after: Vec<u8>,
    pub(crate) memory_len: usize,
    /// Whether `before` was read: then `after` is, however the macro ended (a panic aborts
    /// the instance after its hook gave the output, and what it changed before stays, as it
    /// stays under rustc, where the panic unwinds).
    pub(crate) started: bool,
}

/// What one expansion used: the fuel, its memory at its largest, and the budget it ran past.
pub(crate) struct Used {
    pub(crate) fuel: u64,
    pub(crate) peak_bytes: u64,
    pub(crate) over: Option<Over>,
}

/// Runs macro `index` of the module `pre` instantiates on `input` (what [`Server::input`]
/// wrote), its requests answered from `cx`: what it gave, or why it stopped. On an expansion
/// thread only ([`super::thread::run`]).
pub(crate) fn expand(
    pre: &InstancePre<Host>,
    cx: &mut Cx<'_>,
    index: u32,
    input: &[u8],
    watch: &mut Watch,
) -> (Result<Result<Stream, String>, Stop>, Used) {
    // The pointer is valid until the store drops at this function's end (`with`'s SAFETY).
    let pointer = NonNull::from(&mut *cx).cast::<Cx<'static>>();
    let host = Host { cx: Some(pointer), memory: None, limits: Limits::default() };
    let mut store = Store::new(pre.module().engine(), host);
    store.limiter(|host| &mut host.limits);
    let ran = run(&mut store, pre, index, input, watch);
    let watched = match store.data().memory {
        Some(memory) if watch.started => {
            watch.memory_len = memory.data_size(&store);
            read(&store, memory, &watch.statics, &mut watch.after)
        }
        _ => Ok(()),
    };
    // The engine counts fuel (`engine.rs`), so the store always says what is left.
    let left = store.get_fuel();
    let limits = &store.data().limits;
    let mut spent = Used { fuel: 0, peak_bytes: limits.peak as u64, over: None };
    let over_memory = limits.over;
    drop(store);
    let left = match left {
        Ok(left) => left,
        // alloc: error path
        Err(e) => return (Err(Stop(format!("ran on an engine that counts no fuel: {e:#}"))), spent),
    };
    spent.fuel = FUEL.saturating_sub(left);
    if let Err(error) = watched {
        // alloc: error path
        return (Err(Stop(format!("left its state where it can't be read: {error:#}"))), spent);
    }
    let result = match ran {
        Ok(()) => match cx.output.take() {
            Some(bytes) => cx.server.output(&bytes),
            None => Err(Stop("returned without giving an output".to_owned())), // alloc: error path
        },
        Err(error) => {
            let trap = error.downcast_ref::<Trap>().copied();
            spent.over = match (trap, over_memory) {
                (Some(Trap::OutOfFuel), _) => Some(Over::Steps),
                (_, true) => Some(Over::Memory),
                _ => None,
            };
            stopped_by(cx, &error, trap, spent.over)
        }
    };
    (result, spent)
}

/// What an instance that ended in `error` gave: the stop a shim kept, the panic its hook gave
/// before it aborted, the budget it ran past, or the trap.
fn stopped_by(
    cx: &mut Cx<'_>,
    error: &wasmtime::Error,
    trap: Option<Trap>,
    over: Option<Over>,
) -> Result<Result<Stream, String>, Stop> {
    if let Some(stop) = cx.stop.take() {
        return Err(stop);
    }
    if let Some(&Unentered { errno }) = error.downcast_ref::<Unentered>() {
        let dir = cx.os.current_dir().unwrap_or("<none>");
        let why = match (errno, u16::try_from(errno).ok().and_then(|it| blaze_wasi::Errno(it).name())) {
            (0, _) => "no errno".to_owned(), // alloc: error path
            (_, Some(name)) => format!("{name} (WASI errno {errno})"), // alloc: error path
            (_, None) => format!("WASI errno {errno}"), // alloc: error path
        };
        // alloc: error path
        return Err(Stop(format!(
            "could not enter `{dir}`, the directory cargo runs rustc in for the calling crate, in its \
             wasm module: `std::env::set_current_dir` in blaze's glue failed with {why}"
        )));
    }
    if let Some(bytes) = cx.output.take() {
        return match cx.server.output(&bytes)? {
            Err(panic) => Ok(Err(panic)),
            // alloc: error path
            Ok(_) => Err(Stop(format!("trapped after it gave its output: {error:#}"))),
        };
    }
    Err(match (over, trap) {
        // alloc: error path
        (Some(Over::Steps), _) => Stop(format!("ran out of its {FUEL} units of fuel in its wasm module: it may loop forever")),
        // alloc: error path
        (Some(Over::Memory), _) => Stop(format!("asked for more than its {MEMORY} bytes of wasm memory: {error:#}")),
        // alloc: error path
        (None, Some(_)) => Stop(format!("trapped in its wasm module: {error:#}")),
        // alloc: error path
        (None, None) => Stop(format!("failed in its wasm module: {error:#}")),
    })
}

/// The glue's [`abi::UNENTERED`]: it could not enter the working directory `current_dir`
/// named, failing with `errno` (0 for none), which [`stopped_by`] names.
#[derive(Debug)]
struct Unentered {
    errno: u32,
}

impl std::fmt::Display for Unentered {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the module could not enter its working directory (errno {})", self.errno)
    }
}

impl std::error::Error for Unentered {}

/// The watched statics' bytes in `memory` of the instance `store` holds, into `into`.
fn read(store: &Store<Host>, memory: Memory, statics: &Statics, into: &mut Vec<u8>) -> wasmtime::Result<()> {
    into.clear();
    statics.read(memory.data(store), into).map_err(|why| wasmtime::Error::msg(format!("its `{}` section {why}", super::statics::SECTION)))
}

/// The instance's part of [`expand`].
fn run(store: &mut Store<Host>, pre: &InstancePre<Host>, index: u32, input: &[u8], watch: &mut Watch) -> wasmtime::Result<()> {
    store.set_fuel(FUEL)?;
    let instantiating = tracing::info_span!(target: "blaze", "macro instantiate").entered();
    let instance = pre.instantiate(&mut *store)?;
    let Some(memory) = instance.get_memory(&mut *store, abi::MEMORY) else {
        return Err(wasmtime::Error::msg("the module has no memory export"));
    };
    store.data_mut().memory = Some(memory);
    // Required (`abi::EXPORTS`, checked before instantiating): a reactor's constructors and
    // wasi-libc's start-up run there, and skipping them would expand with them unrun.
    let init: TypedFunc<(), ()> = instance.get_typed_func(&mut *store, abi::INITIALIZE)?;
    init.call(&mut *store, ())?;
    drop(instantiating);
    // The state the calls before it left, over what `_initialize` made: then what the macro
    // changes of it.
    for (at, bytes) in &watch.writes {
        memory.write(&mut *store, *at as usize, bytes)?;
    }
    read(store, memory, &watch.statics, &mut watch.before)?;
    watch.started = true;
    let alloc: TypedFunc<u32, u32> = instance.get_typed_func(&mut *store, abi::ALLOC)?;
    let expand: TypedFunc<(u32, u32, u32), u32> = instance.get_typed_func(&mut *store, abi::EXPAND)?;
    let Ok(len) = u32::try_from(input.len()) else {
        return Err(wasmtime::Error::msg("its input is more than a 32-bit memory holds"));
    };
    let at = alloc.call(&mut *store, len)?;
    memory.write(&mut *store, at as usize, input)?;
    match expand.call(&mut *store, (index, at, len))? {
        0 => Ok(()),
        1 => Err(wasmtime::Error::msg(format!("the module has no macro at index {index}"))),
        status if status & abi::STATUS == abi::UNENTERED => Err(wasmtime::Error::new(Unentered { errno: status >> 8 })),
        status => Err(wasmtime::Error::msg(format!("the module returned status {status}, which the ABI has not"))),
    }
}
