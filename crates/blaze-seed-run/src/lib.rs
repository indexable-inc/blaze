//! The bootstrap seed's runner: runs `blaze-seed.wasm`, blaze compiled to wasm64 (memory64),
//! under Wasmtime (docs/future/bootstrap.md).
//!
//! - **WASI** is `blaze_wasi`'s, the one implementation macros also run under, at 64-bit
//!   pointers (`wasi_snapshot_preview1_64`), over the host's files, stdio and clocks
//!   (`system.rs`).
//! - **Memory** is reserved whole for [`Config::memory_max`] ([`MEMORY_MAX`], 8 TiB, by
//!   default) and never moves; growth past it fails. Growth makes pages writable without
//!   charging them, so a page costs memory only once the seed touches it (`memory.rs`). The
//!   store has no limiter, so nothing below the maximum refuses growth. With Wasmtime's
//!   defaults instead, every grow copies the whole memory (21 to 33 GB resident before a
//!   seed died, v2-seedmem).
//! - **Data segments** are copied in at instantiation (`memory_init_cow(false)`): Wasmtime's
//!   copy-on-write image would map over a memory it did not make.
//! - **The seed's own calls** (`blaze_wasi::seed`, module [`RUNNER`], `blaze_seed`): blaze's
//!   ABI past WASI, the file system written at absolute paths and `decommit(ptr, len)`, which
//!   gives pages back (blaze-mem's `os.rs` on wasm64, where it can't unmap; the range reads as
//!   zeros after).
//! - **Stack**: the seed runs on a thread of its own with a [`THREAD_STACK`] stack, of which
//!   wasm may use [`WASM_STACK`] (the plan's "Stack in the seed").
//! - **Fibers** (`blaze_seed.fiber_*`): each of the seed's fibers runs on one of Wasmtime's own
//!   fibers, a native stack of [`FIBER_STACK`] bytes (reserved, committed as touched), of which
//!   wasm may use [`WASM_STACK`] too; the host switches `__stack_pointer` with it. This is
//!   blaze-graph's `sched::stack` on wasm. Fibers still suspended when `_start` returns are
//!   unwound before the store is dropped.
//!
//! A runner is a small crate, not a blaze subcommand: it must run before any blaze exists on
//! the machine, so it links Wasmtime and this host and none of blaze's engine.
//!
//! Linux and macOS only: on Windows [`run`] is [`RunError::Unsupported`].

#[cfg(unix)]
mod memory;
#[cfg(unix)]
mod system;
#[cfg(all(test, unix))]
mod tests;

use std::{ffi::OsString, fmt};

/// The module of the seed's own calls.
pub const RUNNER: &str = blaze_wasi::seed::MODULE;

/// The address space the seed's memory reserves, and so its largest size, by default: 8 TiB.
/// Native blaze held 2,879 GiB of reservations at its committed peak partway through building
/// itself (v2-seedmem), and blaze-mem's wasm64 `os.rs` takes each reservation from linear
/// memory, so the seed needs about that much linear memory and more for a whole self-build.
pub const MEMORY_MAX: u64 = 8 << 40;

/// The deepest the seed's wasm stack may grow (the plan's 192 MiB).
pub const WASM_STACK: usize = 192 << 20;

/// The stack of the thread the seed runs on: its wasm stack and the host's frames under it.
pub const THREAD_STACK: usize = WASM_STACK + (64 << 20);

/// The native stack of each of the seed's fibers: its wasm stack and the host calls' frames
/// above it (Wasmtime needs its async stacks at least as big as the wasm stack).
pub const FIBER_STACK: usize = WASM_STACK + (16 << 20);

/// What a run is given.
pub struct Config {
    /// The seed's arguments, its name first.
    pub args: Vec<OsString>,
    /// Its environment, in order.
    pub env: Vec<(OsString, OsString)>,
    /// The address space its memory reserves, in bytes: the most it may grow to.
    pub memory_max: u64,
}

/// How a run ended.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Exit {
    /// The code the seed exited with: `proc_exit`'s, or 0 when `_start` returned.
    pub code: u32,
    /// Its memory's size at the end.
    pub memory_bytes: u64,
    /// How much of its memory it held at the end: pages in RAM, and on macOS pages the kernel
    /// paged out (compressed or swapped), which the seed still holds ([`memory::held`]).
    pub held_bytes: u64,
}

/// Why a run failed.
#[derive(Debug)]
pub enum RunError {
    /// The runner does not run on this system.
    Unsupported { os: &'static str },
    /// An argument or variable can't be given to the seed.
    Config(String),
    /// Wasmtime could not make the engine, compile the module or instantiate it.
    Load { step: &'static str, why: String },
    /// A WASI call stopped the seed: what it did.
    Stopped(String),
    /// The seed trapped.
    Trapped(String),
    /// The seed's thread could not start or panicked.
    Thread(String),
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RunError::Unsupported { os } => {
                write!(
                    f,
                    "the seed runner runs on Linux and macOS, not {os}: the seed bootstraps there and nowhere else yet"
                )
            }
            RunError::Config(why) => write!(f, "the seed's arguments or environment: {why}"),
            RunError::Load { step, why } => write!(f, "{step}: {why}"),
            RunError::Stopped(why) => write!(f, "the seed {why}"),
            RunError::Trapped(why) => write!(f, "the seed trapped: {why}"),
            RunError::Thread(why) => write!(f, "the seed's thread: {why}"),
        }
    }
}

impl std::error::Error for RunError {}

/// Runs the seed `module` (its binary) with `config`, on a thread of its own: how it ended.
pub fn run(module: &[u8], config: &Config) -> Result<Exit, RunError> {
    #[cfg(unix)]
    {
        std::thread::scope(|scope| {
            let seed = std::thread::Builder::new()
                .name("blaze-seed".to_owned()) // alloc: once per run
                .stack_size(THREAD_STACK)
                .spawn_scoped(scope, || unix::run(module, config))
                .map_err(|why| {
                    RunError::Thread(format!(
                        "could not start it with a {THREAD_STACK}-byte stack: {why}"
                    ))
                })?; // alloc: error path
            seed.join().map_err(|payload| {
                // A panic's message is a `&str` or a `String`; anything else says only that.
                let message = payload
                    .downcast_ref::<&str>()
                    .copied()
                    .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                    .unwrap_or("a payload that is not a message");
                RunError::Thread(format!("panicked: {message}")) // alloc: error path
            })?
        })
    }
    #[cfg(not(unix))]
    {
        let _ = (module, config);
        Err(RunError::Unsupported { os: std::env::consts::OS })
    }
}

#[cfg(unix)]
mod unix {
    use std::sync::Arc;

    use blaze_wasi::{
        Embed, Stop, System, W64,
        seed::{self, EmbedSeed, Fibers},
    };
    use wasmtime::{Caller, Engine, Linker, Memory, Module, Store, TypedFunc};

    use crate::{Config, Exit, FIBER_STACK, RunError, WASM_STACK, memory, system::Host};

    /// A store's data: the seed's machine, its memory once found, why a call stopped it, and
    /// its fibers.
    pub(crate) struct Seed {
        host: Host,
        memory: Option<Memory>,
        stop: Option<Stop>,
        fibers: Fibers,
    }

    impl Seed {
        /// Runs `f` on the seed's memory and its host, keeping a [`Stop`] it returns.
        fn serve<R>(
            caller: &mut Caller<'_, Seed>,
            f: impl FnOnce(&mut [u8], &mut Host) -> Result<R, Stop>,
        ) -> wasmtime::Result<R> {
            let Some(memory) = caller.data().memory else {
                return Err(wasmtime::Error::msg(
                    "the seed called the runner before its memory was found",
                ));
            };
            let (mem, seed) = memory.data_and_store_mut(caller);
            match f(mem, &mut seed.host) {
                Ok(it) => Ok(it),
                Err(stop) => {
                    seed.stop.get_or_insert(stop);
                    Err(wasmtime::Error::msg("a WASI call of the runner stopped the seed"))
                }
            }
        }
    }

    impl Embed for Seed {
        fn with<R>(
            caller: &mut Caller<'_, Seed>,
            f: impl FnOnce(&mut [u8], &mut dyn System) -> Result<R, Stop>, // dyn: `blaze_wasi`'s calls, shared with the macro host
        ) -> wasmtime::Result<R> {
            Seed::serve(caller, |mem, host| f(mem, host))
        }
    }

    /// The seed, unlike a macro, has calls of its own: it writes the file system.
    impl EmbedSeed for Seed {
        fn with_seed<R>(
            caller: &mut Caller<'_, Seed>,
            f: impl FnOnce(&mut [u8], &mut dyn seed::Seed) -> Result<R, Stop>, // dyn: as `with`
        ) -> wasmtime::Result<R> {
            Seed::serve(caller, |mem, host| f(mem, host))
        }

        fn fibers(&mut self) -> &mut Fibers {
            &mut self.fibers
        }
    }

    fn load(step: &'static str) -> impl FnOnce(wasmtime::Error) -> RunError {
        move |why| RunError::Load { step, why: format!("{why:#}") } // alloc: error path
    }

    /// The engine: memory64, memories reserved whole by [`memory::Lazy`] and never moved, no
    /// guard region (every access is bounds-checked), data segments copied rather than mapped
    /// (Wasmtime's copy-on-write image assumes memory it made), the seed's stack.
    fn engine(config: &Config) -> Result<Engine, RunError> {
        let mut c = wasmtime::Config::new();
        c.wasm_memory64(true)
            .memory_may_move(false)
            .memory_reservation(config.memory_max)
            .memory_reservation_for_growth(0)
            .memory_guard_size(0)
            .with_host_memory(Arc::new(memory::Lazy))
            .memory_init_cow(false)
            .cranelift_opt_level(wasmtime::OptLevel::Speed)
            .max_wasm_stack(WASM_STACK)
            // Each fiber's native stack (`blaze_seed.fiber_new`).
            .async_stack_size(FIBER_STACK);
        Engine::new(&c).map_err(load("making the seed runner's Wasmtime engine"))
    }

    /// The linker: WASI at 64-bit pointers, and the seed's own calls.
    fn linker(engine: &Engine) -> wasmtime::Result<Linker<Seed>> {
        let mut l = Linker::new(engine);
        blaze_wasi::link::<W64, Seed>(&mut l)?;
        seed::link::<Seed>(&mut l)?;
        Ok(l)
    }

    pub(crate) fn run(bytes: &[u8], config: &Config) -> Result<Exit, RunError> {
        let host = Host::new(&config.args, &config.env).map_err(RunError::Config)?;
        let engine = engine(config)?;
        let module = Module::new(&engine, bytes).map_err(load("compiling the seed"))?;
        let linker = linker(&engine).map_err(load("linking the seed runner's imports"))?;
        let mut store =
            Store::new(&engine, Seed { host, memory: None, stop: None, fibers: Fibers::default() });
        let instance =
            linker.instantiate(&mut store, &module).map_err(load("instantiating the seed"))?;
        let Some(memory) = instance.get_memory(&mut store, "memory") else {
            return Err(RunError::Load {
                step: "instantiating the seed",
                why: "it exports no memory `memory`".to_owned(),
            }); // alloc: error path
        };
        store.data_mut().memory = Some(memory);
        let start: TypedFunc<(), ()> = instance
            .get_typed_func(&mut store, "_start")
            .map_err(load("finding the seed's `_start`"))?;
        let ran = start.call(&mut store, ());
        Fibers::dispose_all(&mut store);
        let seed = store.data();
        let code = match (ran, seed.host.exit, &seed.stop) {
            (Ok(()), _, _) => 0,
            (Err(_), Some(code), _) => code,
            (Err(_), None, Some(stop)) => return Err(RunError::Stopped(stop.0.clone())), // alloc: error path
            (Err(trap), None, None) => return Err(RunError::Trapped(format!("{trap:#}"))), // alloc: error path
        };
        let size = memory.data_size(&store);
        // SAFETY: the memory's first `size` bytes are mapped, its base page-aligned.
        let held = unsafe { memory::held(memory.data_ptr(&store), size) }.map_err(|why| {
            RunError::Load {
                step: "measuring the memory the seed holds (mincore)",
                why: why.to_string(),
            }
        })?; // alloc: error path
        Ok(Exit { code, memory_bytes: size as u64, held_bytes: held })
    }
}
