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
use wasmtime::{Config, Engine, OperatorCost, OptLevel, WasmBacktraceDetails};

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
        .operator_cost(fuel_cost())
        .max_wasm_stack(MAX_WASM_STACK)
        // Wasmtime refuses a wasm stack past its async stacks' size (2 MiB by default) even
        // when nothing runs async, as here: the fiber's whole stack, which the module's never
        // reaches.
        .async_stack_size(blaze_graph::sched::FIBER_STACK)
        .wasm_backtrace_details(WasmBacktraceDetails::Disable);
    // alloc: error path
    let engine =
        Engine::new(&config).map_err(|e| format!("Wasmtime could not make its engine: {e:#}"))?;
    let mut bytes = Bytes::default();
    engine.precompile_compatibility_hash().hash(&mut bytes);
    bytes.write(std::env::consts::ARCH.as_bytes());
    bytes.write(std::env::consts::OS.as_bytes());
    let code = ContentId::of("blaze v2 wasm proc macro machine code 1", &bytes.0);
    Ok(Engines { engine, code })
}

/// What each operator costs of an expansion's fuel: Wasmtime's table (one an operator, none
/// for those that make no code), with locals and constants free as well. blaze's lowering of
/// IR to wasm (`hir-ty-codegen`'s `wasm.rs`) passes every IR value through a local of its own
/// (`local.get`, the operation, `local.set`), and Wasmtime's register allocator makes those
/// moves and immediates, mostly none: charged, they were most of what a macro spent, a
/// measure of the producer's encoding and not of work, and LLVM's code of the same function
/// (rustc's std in the same module) spent several times less. derive_generic_visitor's
/// `visitable_group` over 80 types spent 12.5G units, 72% of them locals and constants, and
/// over charon's 170 ran out (#1661). Every loop still costs: a backward edge is a `br`,
/// `br_if` or `br_table`, and a call costs one.
pub(crate) fn fuel_cost() -> OperatorCost {
    let mut cost = OperatorCost::new();
    cost.LocalGet = 0;
    cost.LocalSet = 0;
    cost.LocalTee = 0;
    cost.I32Const = 0;
    cost.I64Const = 0;
    cost.F32Const = 0;
    cost.F64Const = 0;
    cost
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

#[cfg(test)]
mod tests {
    use wasmtime::{Module, Store, Trap};

    use super::Engines;

    /// A loop whose body moves its values through locals, as blaze's lowering writes every
    /// function, costs what its operations do (a `sub` and a `br_if` a turn), not its moves
    /// (#1661); a loop with no end still runs out of fuel.
    #[test]
    fn fuel_counts_operations_not_moves_through_locals() {
        let engines = Engines::new().unwrap();
        let module = Module::new(
            &engines.engine,
            r#"(module
                (func (export "count") (param $n i32) (local $a i32) (local $b i32)
                  (loop $top
                    (local.set $a (local.get $n)) (local.set $b (local.get $a))
                    (local.set $a (local.get $b)) (local.set $b (local.get $a))
                    (local.set $n (i32.sub (local.get $b) (i32.const 1)))
                    (br_if $top (local.get $n))))
                (func (export "spin") (loop $top (br $top))))"#,
        )
        .unwrap();
        let mut store = Store::new(&engines.engine, ());
        store.set_fuel(1 << 30).unwrap();
        let instance = wasmtime::Instance::new(&mut store, &module, &[]).unwrap();
        let count = instance.get_typed_func::<i32, ()>(&mut store, "count").unwrap();
        count.call(&mut store, 1000).unwrap();
        let spent = (1 << 30) - store.get_fuel().unwrap();
        assert!(
            (2000..2100).contains(&spent),
            "1000 turns of a `sub` and a `br_if` spent {spent} fuel"
        );

        store.set_fuel(1 << 20).unwrap();
        let spin = instance.get_typed_func::<(), ()>(&mut store, "spin").unwrap();
        let error = spin.call(&mut store, ()).unwrap_err();
        assert_eq!(error.downcast_ref::<Trap>(), Some(&Trap::OutOfFuel), "{error:#}");
    }
}
