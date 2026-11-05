//! Native targets: corosensei's fibers, each on a machine stack with a guard page below it.

use std::{io, mem::ManuallyDrop};

use corosensei::{
    Coroutine, CoroutineResult,
    stack::{DefaultStack, Stack as _},
};

use super::Resumed;

/// A fiber's stack: `bytes` reserved (committed as touched), with a guard page below.
pub struct Stack {
    stack: DefaultStack,
    bytes: usize,
}

impl Stack {
    /// The stack is kept on base pages: a fiber touches its top few pages, and under the host
    /// setting `transparent_hugepage=always` each fiber would otherwise hold 2 MiB from its
    /// first call.
    pub fn new(bytes: usize) -> io::Result<Stack> {
        let stack = Stack { stack: DefaultStack::new(bytes)?, bytes };
        let limit = std::ptr::NonNull::new(std::ptr::without_provenance_mut::<u8>(stack.limit()))
            .ok_or_else(|| io::Error::other("corosensei gave a stack at address 0"))?;
        // SAFETY: `[limit, base)` is the stack's usable range, mapped by `DefaultStack` and
        // owned by it, alive while `stack` is; advice changes no contents.
        unsafe { blaze_mem::huge::keep_base_pages(limit, bytes) };
        Ok(stack)
    }

    /// The lowest usable address: the stack has at least `bytes` above its guard page, which
    /// corosensei's `Stack::limit` counts in.
    pub fn limit(&self) -> usize {
        self.stack.base().get() - self.bytes
    }
}

/// A fiber's handle on itself, given to its body: [`Yielder::suspend`] switches it out. Its
/// `suspend` is the root of the may-suspend check (hir-ty-suspend finds
/// `blaze_graph::sched::stack::Yielder::suspend`), the same path as on wasm.
#[repr(transparent)]
pub struct Yielder<I, O>(corosensei::Yielder<I, O>);

impl<I, O> Yielder<I, O> {
    fn of(yielder: &corosensei::Yielder<I, O>) -> &Yielder<I, O> {
        // SAFETY: `Yielder` is `repr(transparent)` over corosensei's.
        unsafe { &*std::ptr::from_ref(yielder).cast::<Yielder<I, O>>() }
    }

    /// Switches the fiber out with `out` as what its `resume` returns; returns what it is
    /// resumed with next.
    #[inline]
    pub fn suspend(&self, out: O) -> I {
        self.0.suspend(out)
    }
}

/// A coroutine that is never dropped suspended mid-job: such a one is leaked, stack and all.
/// Dropping it would force-unwind the job, whose own `catch_unwind` swallows corosensei's
/// unwind and goes on into code that panics, and corosensei aborts the process on that panic.
pub struct Stackful<I, O>(ManuallyDrop<Coroutine<I, O, (), DefaultStack>>);

impl<I, O> Stackful<I, O> {
    /// `_fiber` is the fiber's number, which wasm's layer names in its panics; corosensei's
    /// own checks need none.
    pub fn new(
        stack: Stack,
        _fiber: u32,
        body: impl FnOnce(&Yielder<I, O>, I) + 'static,
    ) -> Stackful<I, O>
    where
        I: 'static,
        O: 'static,
    {
        Stackful(ManuallyDrop::new(Coroutine::with_stack(
            stack.stack,
            move |yielder: &corosensei::Yielder<I, O>, first| body(Yielder::of(yielder), first),
        )))
    }

    /// Runs the fiber from its start or its last suspend (which returns `input`) until it
    /// suspends again or its body returns.
    #[inline]
    pub fn resume(&mut self, input: I) -> Resumed<O> {
        match self.0.resume(input) {
            CoroutineResult::Yield(out) => Resumed::Yield(out),
            CoroutineResult::Return(()) => Resumed::Return,
        }
    }
}

impl<I, O> Drop for Stackful<I, O> {
    fn drop(&mut self) {
        if !self.0.started() || self.0.done() {
            // SAFETY: dropped once, here, and never used after.
            unsafe { ManuallyDrop::drop(&mut self.0) };
        }
    }
}
