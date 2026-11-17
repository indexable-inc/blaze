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
pub(super) use native::{Stack, Stackful, Yielder, give_back_below_here};
#[cfg(target_family = "wasm")]
pub(super) use wasm::{Stack, Stackful, Yielder};

/// wasm: a thread's stack is the host's, outside linear memory, and linear memory gives no
/// page back: nothing is advised, and nothing is said to be.
#[cfg(target_family = "wasm")]
pub(super) fn give_back_below_here() -> std::io::Result<usize> {
    Ok(0)
}

/// The bytes every live fiber's stack was seen to hold ([`resident`]), each counted to the page
/// of the deepest frame it switched out from, and taken off as its stack is unmapped.
static RESIDENT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Bytes the live fibers' stacks hold, each to the deepest page it was seen at as it switched
/// out (native's `Yielder::suspend`): what the memory breakdown's `stack bytes` row says, the
/// part of the footprint past the heaps that fibers hold. A lower bound: a recursion that
/// returned before a suspend is not seen, and the threads' own stacks are not counted. wasm
/// counts nothing (its shadow stacks are linear memory, taken whole). One relaxed load.
pub fn resident() -> u64 {
    RESIDENT.load(std::sync::atomic::Ordering::Relaxed)
}

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

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
#[allow(unsafe_code, reason = "reads the page size from the system (`sysconf`)")]
mod tests {
    /// Recurses `depth` frames of 16 KiB each, writing every one, and returns: the stack is
    /// committed that deep below the caller.
    #[inline(never)]
    fn touch_deep(depth: usize) -> u8 {
        let mut frame = [0u8; 16 << 10];
        std::hint::black_box(&mut frame).fill(depth as u8);
        if depth == 0 {
            return frame[0];
        }
        let below = touch_deep(depth - 1);
        std::hint::black_box(&frame)[100].wrapping_add(below)
    }

    /// The pages of `[from, to)` (page-aligned) resident now, by `mincore`.
    #[cfg(target_os = "linux")]
    fn resident_pages(from: usize, to: usize, page: usize) -> usize {
        let mut held = vec![0u8; (to - from) / page]; // alloc: a test's
        // SAFETY: `[from, to)` is whole pages of this thread's mapped stack; `held` has a byte
        // a page, which the call writes.
        let rc = unsafe {
            libc::mincore(std::ptr::without_provenance_mut(from), to - from, held.as_mut_ptr())
        };
        assert_eq!(rc, 0, "mincore of {from:#x}..{to:#x}: {}", std::io::Error::last_os_error());
        held.iter().filter(|it| **it & 1 != 0).count()
    }

    /// The invariant: a worker that parks after an idle daemon's ask gives back its own
    /// stack's pages below its frame: 4 MiB a deep call left committed under the calling frame
    /// are out of the resident set once `give_back_below_here` returns, and it says it advised
    /// at least them. Linux reads residency with `mincore`; macOS, whose `MADV_FREE_REUSABLE`
    /// leaves a page resident until the kernel takes it, checks the advice is accepted on a
    /// live thread stack (macos-private-apis: the advice exists and works here). The control,
    /// the call removed: the pages stay resident.
    #[test]
    fn a_parked_worker_gives_its_stack_pages_back() {
        const DEEP: usize = 4 << 20;
        // Kept clear of the frames at the two ends: this one's, and the advice's own room.
        const MARGIN: usize = 64 << 10;
        for give in [true, false] {
            let seen = std::thread::Builder::new()
                .name("a parked worker's stack test".to_owned()) // alloc: a test's
                .stack_size(4 * DEEP)
                .spawn(move || {
                    touch_deep(DEEP / (16 << 10));
                    let marker = 0u8;
                    let here = std::ptr::from_ref(std::hint::black_box(&marker)).addr();
                    // SAFETY: reads a constant of the system.
                    let page = usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) })
                        .expect("a page size");
                    let to = (here - MARGIN) & !(page - 1);
                    let from = (here - DEEP + MARGIN) & !(page - 1);
                    let pages = (to - from) / page;
                    #[cfg(target_os = "linux")]
                    let before = resident_pages(from, to, page);
                    let advised = if give {
                        super::give_back_below_here().expect("the advice on this thread's stack")
                    } else {
                        0
                    };
                    #[cfg(target_os = "linux")]
                    let after = resident_pages(from, to, page);
                    // Not read on macOS: the advised pages stay resident until taken.
                    #[cfg(not(target_os = "linux"))]
                    let (before, after) = (pages, 0);
                    (before, after, advised, pages)
                })
                .expect("the test's thread starts")
                .join()
                .expect("the test's thread");
            let (before, after, advised, pages) = seen;
            assert!(
                before * 10 >= pages * 9,
                "the deep call committed the range: {before} of {pages} pages"
            );
            match give {
                true => {
                    assert!(
                        advised >= DEEP - 2 * MARGIN,
                        "advised {advised} bytes, under the {DEEP} touched"
                    );
                    assert!(after <= 2, "{after} of {pages} pages resident after the give-back");
                }
                #[cfg(target_os = "linux")]
                false => assert!(
                    after * 10 >= pages * 9,
                    "control: with no give-back the pages stay, yet {after} of {pages} are resident"
                ),
                #[cfg(not(target_os = "linux"))]
                false => {}
            }
        }
    }
}
