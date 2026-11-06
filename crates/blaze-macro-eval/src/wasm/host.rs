//! One run in a fresh instance of a macro's module: its calls in order (the session's earlier
//! calls run again for the state they leave, then the one expanded), each one's input written
//! to the module's memory, its macro run, and each request its `proc_macro` client makes answered by
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

use wasmtime::{
    Caller, Engine, InstancePre, Linker, Memory, ResourceLimiter, Store, Trap, TypedFunc,
};

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

/// The fuel one expansion may spend, about one unit a wasm instruction that makes code (locals
/// and constants are free, `engine::fuel_cost`): a derive of serde's size runs some hundreds of
/// millions natively, so this is tens of times that.
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
    pub(crate) fn new(answer: &'a dyn Answer, server: Server, os: Os<'a>) -> Cx<'a> {
        // dyn: as above
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
    fn memory_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        if desired as u64 > MEMORY {
            self.over = true;
            return Ok(false);
        }
        self.peak = self.peak.max(desired);
        Ok(true)
    }

    fn table_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        Ok(desired <= TABLE_MAX)
    }
}

/// A store's data: the call's [`Cx`] while the call runs, and the instance's memory.
pub(crate) struct Host {
    cx: Option<NonNull<Cx<'static>>>,
    memory: Option<Memory>,
    limits: Limits,
    /// The requests its `proc_macro` client sent, which the `macro wasm run` span says.
    requests: u64,
}

/// The error a shim's stop ends the instance with; the stop itself is in the [`Cx`].
fn stopped() -> wasmtime::Error {
    wasmtime::Error::msg("a shim of blaze's stopped the expansion")
}

/// Runs `f` on the module's memory and the call's [`Cx`]; a [`Stop`] it returns is kept and
/// ends the instance.
#[allow(unsafe_code)]
fn with<R>(
    caller: &mut Caller<'_, Host>,
    f: impl FnOnce(&mut [u8], &mut Cx<'_>) -> Result<R, Stop>,
) -> wasmtime::Result<R> {
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
        c.data_mut().requests += 1;
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
    l.func_wrap(
        BRIDGE,
        "env_var",
        |mut c: Caller<'_, Host>, name: u32, len: u32, into: u32, capacity: u32| {
            with(&mut c, |mem, cx| env_var(mem, cx, name, len, into, capacity))
        },
    )?;
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
        return Err(Stop(format!(
            "was given a working directory of {} bytes, more than a 32-bit memory holds",
            dir.len()
        )));
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
fn env_var(
    mem: &mut [u8],
    cx: &mut Cx<'_>,
    name: u32,
    len: u32,
    into: u32,
    capacity: u32,
) -> Result<i32, Stop> {
    let name = wasi::bytes(mem, name, len)?;
    let Ok(name) = std::str::from_utf8(name) else {
        // alloc: error path
        return Err(Stop(format!(
            "read the variable {:?}, whose name is not UTF-8",
            String::from_utf8_lossy(name)
        )));
    };
    let Some(value) = cx.os.var(name) else { return Ok(-1) };
    let Ok(n) = i32::try_from(value.len()) else {
        // alloc: error path
        return Err(Stop(format!(
            "read a variable of {} bytes, more than a 32-bit memory holds",
            value.len()
        )));
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

/// One call of a run ([`expand`]): the macro at `index` of the module's list, on `input` (what
/// `server`'s [`Server::input`] wrote), its requests answered by `server`, as rustc gives each
/// expansion its own handles.
pub(crate) struct Call {
    pub(crate) index: u32,
    pub(crate) server: Server,
    pub(crate) input: Vec<u8>,
}

/// What one run used: the fuel of all its calls, its memory at its largest, and the budget it
/// ran past.
pub(crate) struct Used {
    pub(crate) fuel: u64,
    pub(crate) peak_bytes: u64,
    pub(crate) over: Option<Over>,
}

/// Runs `calls` in order in one instance of the module `pre`, each with [`FUEL`] of its own,
/// their requests answered from `cx` (whose server is each call's while it runs, and the last
/// one's after): what the last gave, or why the run stopped. The calls before the last are run
/// for the state they leave; what they give is not used, and one that stops (a panic aborts the
/// instance the calls after it would run in) stops the run, naming it. On an expansion thread
/// only ([`super::thread::run`]).
pub(crate) fn expand(
    pre: &InstancePre<Host>,
    cx: &mut Cx<'_>,
    calls: &mut [Call],
    watch: &mut Watch,
) -> (Result<Result<Stream, String>, Stop>, Used) {
    // The pointer is valid until the store drops at this function's end (`with`'s SAFETY).
    let pointer = NonNull::from(&mut *cx).cast::<Cx<'static>>();
    let host = Host { cx: Some(pointer), memory: None, limits: Limits::default(), requests: 0 };
    let mut store = Store::new(pre.module().engine(), host);
    store.limiter(|host| &mut host.limits);
    let mut fuel = 0;
    let ran = run(&mut store, pre, calls, watch, &mut fuel);
    let watched = match store.data().memory {
        Some(memory) if watch.started => {
            watch.memory_len = memory.data_size(&store);
            read(&store, memory, &watch.statics, &mut watch.after)
        }
        _ => Ok(()),
    };
    let limits = &store.data().limits;
    let mut spent = Used { fuel, peak_bytes: limits.peak as u64, over: None };
    let over_memory = limits.over;
    drop(store);
    if let Err(error) = watched {
        // alloc: error path
        return (Err(Stop(format!("left its state where it can't be read: {error:#}"))), spent);
    }
    let result = match ran {
        Ok(()) => match cx.output.take() {
            Some(bytes) => cx.server.output(&bytes),
            None => Err(Stop("returned without giving an output".to_owned())), // alloc: error path
        },
        Err((at, error)) => {
            let trap = error.downcast_ref::<Trap>().copied();
            spent.over = match (trap, over_memory) {
                (Some(Trap::OutOfFuel), _) => Some(Over::Steps),
                (_, true) => Some(Over::Memory),
                _ => None,
            };
            let stopped = stopped_by(cx, &error, trap, spent.over);
            match at + 1 < calls.len() {
                true => Err(earlier(at, calls.len() - 1, stopped)),
                false => stopped,
            }
        }
    };
    (result, spent)
}

/// Why a run stopped at its call `at`, one of the `of` calls of the session before the one
/// expanded, which it runs again for the state they leave: what stopped that call, or the panic
/// it gave, which aborts the instance the calls after it would run in.
fn earlier(at: usize, of: usize, stopped: Result<Result<Stream, String>, Stop>) -> Stop {
    let why = match stopped {
        // alloc: error path
        Ok(Err(panic)) => format!("panicked ({panic}), which aborts its wasm instance"),
        Ok(Ok(_)) => "stopped after it gave its output".to_owned(), // alloc: error path
        Err(Stop(why)) => why,
    };
    // alloc: error path
    Stop(format!(
        "could not run again call {} of the {of} its session made before this one, which leave \
         the state it runs from: that call {why}",
        at + 1
    ))
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
        let why =
            match (errno, u16::try_from(errno).ok().and_then(|it| blaze_wasi::Errno(it).name())) {
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
        (Some(Over::Steps), _) => Stop(format!(
            "ran out of its {FUEL} units of fuel in its wasm module: it may loop forever"
        )),
        // alloc: error path
        (Some(Over::Memory), _) => {
            Stop(format!("asked for more than its {MEMORY} bytes of wasm memory: {error:#}"))
        }
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
fn read(
    store: &Store<Host>,
    memory: Memory,
    statics: &Statics,
    into: &mut Vec<u8>,
) -> wasmtime::Result<()> {
    into.clear();
    statics.read(memory.data(store), into).map_err(|why| {
        wasmtime::Error::msg(format!("its `{}` section {why}", super::statics::SECTION))
    })
}

/// Runs `f` on the run's [`Cx`] between two calls into the module.
#[allow(unsafe_code)]
fn between(store: &mut Store<Host>, f: impl FnOnce(&mut Cx<'_>)) {
    if let Some(mut cx) = store.data().cx {
        // SAFETY: as `with`'s: the pointer is from the `&mut Cx` `expand` holds for as long as
        // the store lives, and no module code runs while `store` is borrowed here, so no shim
        // holds another reference to the `Cx`.
        f(unsafe { cx.as_mut() });
    }
}

/// The instance's part of [`expand`]: the fuel each call spent added to `fuel`, and on an
/// error the call it stopped at (the last for the instance's making, which is no earlier
/// call's doing).
fn run(
    store: &mut Store<Host>,
    pre: &InstancePre<Host>,
    calls: &mut [Call],
    watch: &mut Watch,
    fuel: &mut u64,
) -> Result<(), (usize, wasmtime::Error)> {
    let last = calls.len().saturating_sub(1);
    let first = |error| (last, error);
    if calls.is_empty() {
        return Err(first(wasmtime::Error::msg("a run of no calls")));
    }
    store.set_fuel(FUEL).map_err(first)?;
    let instantiating = tracing::info_span!(target: "blaze", "macro instantiate").entered();
    let instance = pre.instantiate(&mut *store).map_err(first)?;
    let Some(memory) = instance.get_memory(&mut *store, abi::MEMORY) else {
        return Err(first(wasmtime::Error::msg("the module has no memory export")));
    };
    store.data_mut().memory = Some(memory);
    // Required (`abi::EXPORTS`, checked before instantiating): a reactor's constructors and
    // wasi-libc's start-up run there, and skipping them would expand with them unrun.
    let init: TypedFunc<(), ()> =
        instance.get_typed_func(&mut *store, abi::INITIALIZE).map_err(first)?;
    init.call(&mut *store, ()).map_err(first)?;
    drop(instantiating);
    // The state the calls before it left, over what `_initialize` made: then what the calls
    // change of it.
    for (at, bytes) in &watch.writes {
        memory.write(&mut *store, *at as usize, bytes).map_err(|e| first(e.into()))?;
    }
    read(store, memory, &watch.statics, &mut watch.before).map_err(first)?;
    watch.started = true;
    let alloc: TypedFunc<u32, u32> =
        instance.get_typed_func(&mut *store, abi::ALLOC).map_err(first)?;
    let expand: TypedFunc<(u32, u32, u32), u32> =
        instance.get_typed_func(&mut *store, abi::EXPAND).map_err(first)?;
    for (at, call) in calls.iter_mut().enumerate() {
        let failed = |error| (at, error);
        between(store, |cx| {
            std::mem::swap(&mut cx.server, &mut call.server);
            cx.output = None;
        });
        if at > 0 {
            store.set_fuel(FUEL).map_err(failed)?;
        }
        store.data_mut().requests = 0;
        let index = call.index;
        let Ok(len) = u32::try_from(call.input.len()) else {
            return Err(failed(wasmtime::Error::msg(
                "its input is more than a 32-bit memory holds",
            )));
        };
        let input = alloc.call(&mut *store, len).map_err(failed)?;
        memory.write(&mut *store, input as usize, &call.input).map_err(|e| failed(e.into()))?;
        // What the macro spent in its module, and how often it asked the server: fuel over the
        // requests is the module's own work, the rest the bridge's.
        let running = tracing::info_span!(target: "blaze", "macro wasm run", fuel = tracing::field::Empty, requests = tracing::field::Empty).entered();
        let called = expand.call(&mut *store, (index, input, len));
        // The engine counts fuel (`engine.rs`), so the store always says what is left.
        let left = store.get_fuel().map_err(|e| {
            failed(wasmtime::Error::msg(format!("ran on an engine that counts no fuel: {e:#}")))
        })?;
        *fuel += FUEL.saturating_sub(left);
        running.record("fuel", FUEL.saturating_sub(left));
        running.record("requests", store.data().requests);
        drop(running);
        match called.map_err(failed)? {
            0 => {}
            1 => {
                return Err(failed(wasmtime::Error::msg(format!(
                    "the module has no macro at index {index}"
                ))));
            }
            status if status & abi::STATUS == abi::UNENTERED => {
                return Err(failed(wasmtime::Error::new(Unentered { errno: status >> 8 })));
            }
            status => {
                return Err(failed(wasmtime::Error::msg(format!(
                    "the module returned status {status}, which the ABI has not"
                ))));
            }
        }
    }
    Ok(())
}
