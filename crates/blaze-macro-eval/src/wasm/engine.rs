//! The daemon's Wasmtime engine, and the id of the machine code it makes: what a module's
//! compiled code is kept in the store under, beside the module's own id. Made once by the
//! daemon with its [`Modules`](super::Modules), never per process behind a static.
//!
//! Every setting is named here, none read from the environment (`WASMTIME_BACKTRACE_DETAILS`
//! would otherwise change the code and its errors per process). Traps are explicit checks:
//! without the `signals-based-traps` feature the engine installs no signal handler in the
//! daemon, whose fibers' guard pages are its own.

use std::hash::{Hash as _, Hasher as _};

use blaze_ids::ContentId;
use wasmtime::{Config, Engine, OptLevel, WasmBacktraceDetails};

/// The deepest a module's stack may grow, of a fiber's 16 MiB (`blaze_graph::sched::Config`):
/// a trap past it, not an overflow of the fiber's.
const MAX_WASM_STACK: usize = 4 << 20;

/// The engine, and the id of its code. `Engine` is a shared handle, `Send` and `Sync`: the
/// modules it compiles and the instances made of them may go to an expansion thread.
pub(crate) struct Engines {
    pub(crate) engine: Engine,
    /// What the machine code it makes depends on besides the module: Wasmtime's version and
    /// settings (`precompile_compatibility_hash`), and the host's architecture and system.
    pub(crate) code: ContentId,
}

impl Engines {
    /// The engine with every setting named here, or why Wasmtime could make none.
    pub(crate) fn new() -> Result<Engines, String> {
        make()
    }
}

fn make() -> Result<Engines, String> {
    let mut config = Config::new();
    config
        .cranelift_opt_level(OptLevel::Speed)
        // A NaN's bits are the same on every host, so a macro that prints a float gives the
        // same tokens wherever it ran.
        .cranelift_nan_canonicalization(true)
        // The bounded-progress budget: deterministic, unlike a deadline.
        .consume_fuel(true)
        .max_wasm_stack(MAX_WASM_STACK)
        // Wasmtime refuses a wasm stack past its async stacks' size (2 MiB by default) even
        // when nothing runs async, as here: the fiber's whole stack, which the module's never
        // reaches.
        .async_stack_size(blaze_graph::sched::FIBER_STACK)
        .wasm_backtrace_details(WasmBacktraceDetails::Disable);
    // alloc: error path
    let engine = Engine::new(&config).map_err(|e| format!("Wasmtime could not make its engine: {e:#}"))?;
    let mut bytes = Bytes::default();
    engine.precompile_compatibility_hash().hash(&mut bytes);
    bytes.write(std::env::consts::ARCH.as_bytes());
    bytes.write(std::env::consts::OS.as_bytes());
    let code = ContentId::of("blaze v2 wasm proc macro machine code 1", &bytes.0);
    Ok(Engines { engine, code })
}

/// The bytes a `Hash` writes, in order: hashed whole, so the id is the same in every process
/// of one build of Wasmtime.
#[derive(Default)]
struct Bytes(Vec<u8>);

impl std::hash::Hasher for Bytes {
    /// Never asked by [`make`], which hashes the bytes whole; the first word of their hash.
    fn finish(&self) -> u64 {
        let id = ContentId::of("blaze v2 wasm engine bytes", &self.0);
        let mut word = [0; 8];
        word.copy_from_slice(&id.0[..8]);
        u64::from_le_bytes(word)
    }

    fn write(&mut self, bytes: &[u8]) {
        self.0.extend_from_slice(bytes);
    }
}
