//! The scheduler's one stack-switching layer: every fiber of [`super::fiber`]'s driver and of
//! [`super::host`]'s pool jobs is made, resumed and suspended through it, and the target picks
//! its implementation here, in one place (docs/future/bootstrap.md, "Fibers in the seed"):
//!
//! - native targets: corosensei, a machine stack per fiber with a guard page below it;
//! - wasm ([`wasm`]): the host's fibers, through the seed's own calls (`blaze_seed.fiber_*`):
//!   the seed's runner runs each fiber on a native stack of Wasmtime's, and switches
//!   `__stack_pointer` to the fiber's shadow stack, a region of linear memory, while it runs. A
//!   host without those calls refuses the module at instantiation, naming them.
//!
//! Above this layer nothing differs by target: one scheduler, one pool, one wait graph. Its
//! `Yielder::suspend` is the one call that parks a fiber on every target, and the root
//! hir-ty-suspend finds by this path (`blaze_graph::sched::stack::Yielder::suspend`).

#[cfg(not(target_family = "wasm"))]
#[allow(unsafe_code)]
mod native;
#[cfg(target_family = "wasm")]
#[allow(unsafe_code)]
mod wasm;

#[cfg(not(target_family = "wasm"))]
pub(super) use native::{Stack, Stackful, Yielder};
#[cfg(target_family = "wasm")]
pub(super) use wasm::{Stack, Stackful, Yielder};

/// The bytes of each fiber's stack, both drivers' ([`super::host`]'s pool fibers and
/// [`super::fiber`]'s `Config::for_cores`), chosen here per target.
///
/// Native: 16 MiB above a guard page, reserved and committed as touched: the front end's
/// deepest recursion runs on it, and `stdx::ensure_sufficient_stack` grows past it.
#[cfg(not(target_family = "wasm"))]
pub const FIBER_STACK: usize = 16 << 20;
/// wasm: the fiber's shadow stack, 4 MiB of linear memory taken whole when the fiber is made
/// (wasm reserves nothing). It holds only a frame's address-taken memory: every other local is a
/// wasm local, on the host's native stack for the fiber (the runner's async stack size).
/// docs/future/bootstrap.md's "Stack in the seed" puts the trait solver's deepest recursion (512
/// levels) at about 3 MiB of whole native frames, an upper bound for its shadow-stack share, and
/// the debug red zone of `ensure_sufficient_stack` is 1 MiB, so 4 MiB holds both; past it that
/// check stops with its error (which aborts the seed: panic=abort), never an overflow.
#[cfg(target_family = "wasm")]
pub const FIBER_STACK: usize = 4 << 20;

/// What a [`Stackful::resume`] came to: the fiber suspended with a value, or its body returned.
pub(super) enum Resumed<O> {
    Yield(O),
    Return,
}
