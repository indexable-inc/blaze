//! wasm: each fiber runs on a stack of the host's, switched by the host. The seed's runner
//! (blaze-seed-run) runs every fiber on one of Wasmtime's own fibers, the native stacks its async
//! calls run on; the calls are blaze's own (`blaze_seed.fiber_*`, defined once in blaze-wasi's
//! `seed`). A fiber's wasm frames (its locals, the engine's return addresses) are on that native
//! stack, and the frames' address-taken memory on its shadow stack, a region of linear memory
//! ([`Stack`]) that the host switches `__stack_pointer` to while it runs and back as it
//! suspends.
//!
//! So a switch costs the same at any depth (a native stack switch and the engine's few fields
//! of per-call state), no function is transformed or instrumented, and resumes nest: a fiber
//! may resume another, each suspend returning to its resumer, as corosensei's on native.
//! Nothing here falls back: a module whose host has no `blaze_seed.fiber_*` calls (any host but
//! the seed's runner, a wasm32 one) is refused at instantiation, naming them.

use std::{
    alloc::{self, Layout},
    io,
    marker::PhantomData,
    mem::ManuallyDrop,
    ptr::NonNull,
};

use super::Resumed;

// The seed's fiber calls (blaze-wasi `seed`): the host runs `entry(arg)` on a fiber of its own
// with `__stack_pointer` at `stack_pointer`; `fiber_resume` answers 0 when the fiber suspended,
// 1 when `entry` returned. Misuse (an unknown fiber, a resume of one running or returned, a
// suspend off any fiber) traps, naming the fiber and the call.
#[link(wasm_import_module = "blaze_seed")]
unsafe extern "C" {
    fn fiber_new(entry: unsafe extern "C" fn(usize), arg: usize, stack_pointer: usize) -> u64;
    fn fiber_resume(fiber: u64) -> u32;
    fn fiber_suspend();
    fn fiber_drop(fiber: u64);
}

/// What `fiber_resume` answers.
const SUSPENDED: u32 = 0;
const RETURNED: u32 = 1;

/// A fiber's shadow stack: `bytes` of linear memory, 16-aligned, owned, taken whole when the
/// fiber is made (wasm reserves nothing). It grows down from its end; wasm has no guard page,
/// so `stdx::ensure_sufficient_stack` checks the room left above [`Stack::limit`].
pub struct Stack {
    at: NonNull<u8>,
    layout: Layout,
}

impl Stack {
    pub fn new(bytes: usize) -> io::Result<Stack> {
        let layout = Layout::from_size_align(bytes.next_multiple_of(16), 16).map_err(|why| {
            // alloc: the error of a fiber not made, once
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("a fiber's stack of {bytes} bytes: {why}"),
            )
        })?;
        if layout.size() == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "a fiber's stack of 0 bytes"));
        }
        // SAFETY: `layout` has a nonzero size, checked above.
        let at = unsafe { alloc::alloc(layout) };
        let at = NonNull::new(at).ok_or_else(|| {
            // alloc: the error of a fiber not made, once
            io::Error::new(io::ErrorKind::OutOfMemory, format!("a fiber's stack of {bytes} bytes"))
        })?;
        Ok(Stack { at, layout })
    }

    /// The lowest usable address of the stack.
    pub fn limit(&self) -> usize {
        self.at.as_ptr().addr()
    }

    /// The stack's top, where a fresh fiber's `__stack_pointer` starts.
    fn top(&self) -> usize {
        self.limit() + self.layout.size()
    }
}

impl Drop for Stack {
    fn drop(&mut self) {
        // SAFETY: allocated in `Stack::new` with this layout, freed once.
        unsafe { alloc::dealloc(self.at.as_ptr(), self.layout) };
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Fresh,
    Running,
    Suspended,
    Done,
}

// dyn: a fiber's body, made once per fiber and called once
type Body<I, O> = Box<dyn FnOnce(&Yielder<I, O>, I)>;

struct Inner<I, O> {
    stack: Stack,
    /// Its number, as the driver's [`super::super::Switch::fiber`] names it.
    fiber: u32,
    /// The host's handle on it (`fiber_new`'s answer).
    handle: u64,
    state: State,
    body: Option<Body<I, O>>,
    input: Option<I>,
    output: Option<O>,
}

/// A fiber's handle on itself, given to its body: [`Yielder::suspend`] switches it out. Its
/// `suspend` is the root of the may-suspend check (hir-ty-suspend finds
/// `blaze_graph::sched::stack::Yielder::suspend`), the same path as on native.
pub struct Yielder<I, O> {
    inner: *mut Inner<I, O>,
    _not_send: PhantomData<*mut ()>,
}

impl<I, O> Yielder<I, O> {
    /// Switches the fiber out with `out` as what its `resume` returns; returns what it is
    /// resumed with next.
    #[inline(never)]
    pub fn suspend(&self, out: O) -> I {
        let inner = self.inner;
        // SAFETY: `inner` is the running fiber's, boxed in its `Stackful`, which outlives every
        // run of its body; only the fiber itself and its resumer touch it, one at a time.
        unsafe {
            (*inner).output = Some(out);
            fiber_suspend();
            (*inner)
                .input
                .take()
                .unwrap_or_else(|| panic!("fiber {} was resumed with no value", (*inner).fiber))
        }
    }
}

/// The first frame of a fiber's body, which the host calls on the fiber's own stack with its
/// `Inner` as `arg`.
unsafe extern "C" fn entry<I, O>(arg: usize) {
    let inner = std::ptr::with_exposed_provenance_mut::<Inner<I, O>>(arg);
    let yielder = Yielder { inner, _not_send: PhantomData };
    // SAFETY: as in `Yielder::suspend`; a fresh fiber holds its body and its first input.
    let (body, input) = unsafe {
        let fiber = (*inner).fiber;
        (
            (*inner)
                .body
                .take()
                .unwrap_or_else(|| panic!("fiber {fiber} was started with no body")),
            (*inner)
                .input
                .take()
                .unwrap_or_else(|| panic!("fiber {fiber} was started with no value")),
        )
    };
    body(&yielder, input);
}

/// A fiber: its body runs on its own stack until it suspends or returns.
pub struct Stackful<I, O>(ManuallyDrop<Box<Inner<I, O>>>);

impl<I, O> Stackful<I, O> {
    /// `fiber` is its number, named in this layer's panics.
    pub fn new(
        stack: Stack,
        fiber: u32,
        body: impl FnOnce(&Yielder<I, O>, I) + 'static,
    ) -> Stackful<I, O>
    where
        I: 'static,
        O: 'static,
    {
        let top = stack.top();
        // alloc: a fiber's own, once per fiber made, kept for its life
        let mut inner = Box::new(Inner {
            stack,
            fiber,
            handle: 0,
            state: State::Fresh,
            body: Some(Box::new(body)), // alloc: as above
            input: None,
            output: None,
        });
        let arg = std::ptr::from_mut::<Inner<I, O>>(&mut *inner).expose_provenance();
        // SAFETY: the host only records `entry`, `arg` and `top` here; it calls `entry(arg)`
        // at the first resume, while `inner` (boxed, never moved) is alive.
        inner.handle = unsafe { fiber_new(entry::<I, O>, arg, top) };
        Stackful(ManuallyDrop::new(inner))
    }

    /// Runs the fiber from where it was (its start, or its last suspend, which returns
    /// `input`) until it suspends again or its body returns.
    ///
    /// # Panics
    ///
    /// For a fiber running or one whose body returned.
    #[inline(never)]
    pub fn resume(&mut self, input: I) -> Resumed<O> {
        let inner: *mut Inner<I, O> = &mut **self.0;
        // SAFETY: `inner` is this fiber's, which nothing else touches while it is not running;
        // the host switches to its stack and back around `fiber_resume`.
        unsafe {
            let fiber = (*inner).fiber;
            match (*inner).state {
                State::Fresh | State::Suspended => {}
                State::Running => panic!("fiber {fiber} resumed while it runs"),
                State::Done => panic!("fiber {fiber} resumed after its body returned"),
            }
            (*inner).input = Some(input);
            (*inner).state = State::Running;
            match fiber_resume((*inner).handle) {
                SUSPENDED => {
                    (*inner).state = State::Suspended;
                    Resumed::Yield(
                        (*inner)
                            .output
                            .take()
                            .unwrap_or_else(|| panic!("fiber {fiber} suspended with no value")),
                    )
                }
                RETURNED => {
                    (*inner).state = State::Done;
                    Resumed::Return
                }
                other => panic!(
                    "fiber {fiber}: the host's `fiber_resume` answered {other}, neither 0 nor 1"
                ),
            }
        }
    }
}

impl<I, O> Drop for Stackful<I, O> {
    /// The host's fiber is ended either way; a suspended one is unwound there with no guest
    /// code run. Its `Inner` and shadow stack are leaked when it was suspended mid-body: its
    /// frames there may be linked from elsewhere (a waiter's node on its stack), and running
    /// their drops would run the body.
    fn drop(&mut self) {
        // SAFETY: the handle is the host's, dropped once, here; the fiber is not running (a
        // running fiber's `Stackful` is borrowed by its `resume`).
        unsafe { fiber_drop(self.0.handle) };
        if matches!(self.0.state, State::Fresh | State::Done) {
            // SAFETY: dropped once, here, and never used after.
            unsafe { ManuallyDrop::drop(&mut self.0) };
        }
    }
}
