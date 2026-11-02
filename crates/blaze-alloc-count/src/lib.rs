//! A counting global allocator for allocation-budget tests.
//!
//! A hot crate's tests install it once:
//!
//! ```ignore
//! #[cfg(test)]
//! #[global_allocator]
//! static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;
//! ```
//!
//! and then hold a hot path to a budget:
//!
//! ```ignore
//! let (_, used) = blaze_alloc_count::measure(|| db.get(key));
//! assert_eq!(used.allocations, 0, "a memo hit allocates nothing");
//! ```
//!
//! Counts are per thread, so tests running in parallel don't see each other's allocations.
//! Work a closure hands to other threads is not counted; measure on the thread that does it.
//!
//! With `BLAZE_ALLOC_TRACE=1`, each counted allocation also keeps a backtrace, and a failing
//! [`assert_at_most`] prints where every one of them came from. A test that checks a
//! [`measure`]'s count itself gets them too: a panic on a thread prints the traces of that
//! thread's last measurement after the panic message, so a budget assertion right after its
//! measurement shows where the allocations it counted came from.

use std::{
    alloc::{GlobalAlloc, Layout, System},
    backtrace::Backtrace,
    cell::{Cell, RefCell},
    fmt::Write as _,
    sync::{Once, OnceLock},
};

/// The system allocator, counting each thread's allocations while [`measure`] runs on it.
pub struct Counting;

/// What a measured closure allocated.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Used {
    /// Calls to `alloc`, `alloc_zeroed` and `realloc`.
    pub allocations: u64,
    /// Bytes asked for by those calls (for `realloc`, the new size).
    pub bytes: u64,
    /// The most bytes one of those calls asked for.
    pub largest: u64,
}

thread_local! {
    static ON: Cell<bool> = const { Cell::new(false) };
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
    static BYTES: Cell<u64> = const { Cell::new(0) };
    static LARGEST: Cell<u64> = const { Cell::new(0) };
    static TRACING: Cell<bool> = const { Cell::new(false) };
    // Set while this crate allocates for itself (a backtrace, the trace list, reading the
    // environment): those allocations are neither counted nor traced.
    static OWN: Cell<bool> = const { Cell::new(false) };
    static TRACES: RefCell<Vec<(usize, Backtrace)>> = const { RefCell::new(Vec::new()) };
}

static TRACE: OnceLock<bool> = OnceLock::new();
static PANIC_HOOK: Once = Once::new();

/// Chains a panic hook that prints [`panic_report`] after the previous hook's output.
fn install_panic_hook() {
    PANIC_HOOK.call_once(|| {
        own(|| {
            let previous = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                previous(info);
                if let Some(report) = panic_report() {
                    own(|| eprintln!("{report}"));
                }
            }));
        });
    });
}

/// The traces this thread's last measurement kept, if it kept any. `try_`: a panic may come
/// from inside the trace list's own borrow, or from a thread being torn down.
fn panic_report() -> Option<String> {
    own(|| {
        let report = TRACES.try_with(|traces| {
            let traces = traces.try_borrow().ok()?;
            if traces.is_empty() {
                return None;
            }
            let mut out = format!(
                "this thread's last allocation measurement made {} traced allocations:",
                traces.len()
            );
            for (n, (size, trace)) in traces.iter().enumerate() {
                let _ = write!(out, "\nallocation {} ({size} bytes):\n{trace}", n + 1);
            }
            Some(out)
        });
        report.ok().flatten()
    })
}

#[cfg(test)]
fn forced() -> bool {
    tests::FORCE_TRACE.with(Cell::get)
}

#[cfg(not(test))]
fn forced() -> bool {
    false
}

/// Runs `f` with [`OWN`] set, so what it allocates is this crate's and not the measured code's.
fn own<R>(f: impl FnOnce() -> R) -> R {
    let was = OWN.with(|own| own.replace(true));
    let result = f();
    OWN.with(|own| own.set(was));
    result
}

#[inline]
fn record(size: usize) {
    // `try_with`: a thread being torn down may allocate after its locals are gone.
    if OWN.try_with(Cell::get).unwrap_or(true) || !ON.try_with(Cell::get).unwrap_or(false) {
        return;
    }
    let _ = ALLOCATIONS.try_with(|c| c.set(c.get() + 1));
    let _ = BYTES.try_with(|c| c.set(c.get() + size as u64));
    let _ = LARGEST.try_with(|c| c.set(c.get().max(size as u64)));
    if TRACING.try_with(Cell::get).unwrap_or(false) {
        own(|| {
            let trace = Backtrace::force_capture();
            let _ = TRACES.try_with(|traces| traces.borrow_mut().push((size, trace)));
        });
    }
}

// SAFETY: every call forwards to `System` with the same arguments, which upholds
// `GlobalAlloc`'s contract; the counting touches only this thread's `Cell`s and never
// allocates.
unsafe impl GlobalAlloc for Counting {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        // SAFETY: the caller's `layout` obligations pass through unchanged.
        unsafe { System.alloc(layout) }
    }

    #[inline]
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        // SAFETY: as for `alloc`.
        unsafe { System.alloc_zeroed(layout) }
    }

    #[inline]
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` came from this allocator, which is `System`, with `layout`.
        unsafe { System.dealloc(ptr, layout) }
    }

    #[inline]
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        record(new_size);
        // SAFETY: `ptr` came from `System` with `layout`; the caller upholds `new_size`'s rules.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

/// Runs `f` and returns what it allocated on this thread. Measurements nest: an inner one
/// counts only its own closure, and the outer one includes it.
pub fn measure<R>(f: impl FnOnce() -> R) -> (R, Used) {
    let (result, used, _) = measure_traced(f);
    (result, used)
}

/// [`measure`], and where this measurement's traces start in [`TRACES`].
fn measure_traced<R>(f: impl FnOnce() -> R) -> (R, Used, usize) {
    let tracing = own(|| {
        *TRACE.get_or_init(|| std::env::var_os("BLAZE_ALLOC_TRACE").is_some_and(|v| v == "1"))
    }) || forced();
    let was_on = ON.with(|on| on.get());
    if tracing {
        install_panic_hook();
    }
    let start = own(|| {
        TRACES.with(|traces| {
            let mut traces = traces.borrow_mut();
            // An outermost measurement drops the traces the last one kept.
            if !was_on {
                traces.clear();
            }
            traces.len()
        })
    });
    let was_tracing = TRACING.with(|t| t.replace(tracing));
    ON.with(|on| on.set(true));
    let outer_largest = LARGEST.with(|c| c.replace(0));
    let before = (ALLOCATIONS.with(Cell::get), BYTES.with(Cell::get));
    let result = f();
    let after = (ALLOCATIONS.with(Cell::get), BYTES.with(Cell::get));
    let largest = LARGEST.with(|c| c.replace(outer_largest.max(c.get())));
    ON.with(|on| on.set(was_on));
    TRACING.with(|t| t.set(was_tracing));
    (result, Used { allocations: after.0 - before.0, bytes: after.1 - before.1, largest }, start)
}

/// Where each allocation traced since `start` came from, or how to see it.
fn traces_since(start: usize) -> String {
    own(|| {
        TRACES.with(|traces| {
            let traces = traces.borrow();
            if traces.len() <= start {
                return "\n(set BLAZE_ALLOC_TRACE=1 for a backtrace of each allocation)".to_owned();
            }
            let mut out = String::new();
            for (n, (size, trace)) in traces[start..].iter().enumerate() {
                let _ = write!(out, "\nallocation {} ({size} bytes):\n{trace}", n + 1);
            }
            out
        })
    })
}

/// Whether this binary's global allocator is [`Counting`]: without it every measurement is
/// zero, which a budget test must not read as a pass.
pub fn installed() -> bool {
    let (_, used) = measure(|| std::hint::black_box(Box::new(0u8)));
    used.allocations == 1
}

/// Asserts `f` makes at most `max` allocations on this thread, naming `what` when it doesn't.
#[track_caller]
pub fn assert_at_most<R>(max: u64, what: &str, f: impl FnOnce() -> R) -> R {
    let (result, used, start) = measure_traced(f);
    if used.allocations > max {
        let traces = traces_since(start);
        // The message carries them, so the panic hook does not print them again.
        own(|| TRACES.with(|t| t.borrow_mut().truncate(start)));
        panic!(
            "{what}: {} allocations ({} bytes), budget {max}{traces}",
            used.allocations, used.bytes
        );
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[global_allocator]
    static ALLOC: Counting = Counting;

    thread_local! {
        // `BLAZE_ALLOC_TRACE` for this thread alone: setting the variable would reach every test.
        pub(super) static FORCE_TRACE: Cell<bool> = const { Cell::new(false) };
    }

    #[test]
    fn a_traced_failure_names_where_each_allocation_came_from() {
        FORCE_TRACE.with(|f| f.set(true));
        let failed = std::panic::catch_unwind(|| {
            assert_at_most(0, "two boxes", || (Box::new(1u8), Box::new([0u8; 40])));
        });
        FORCE_TRACE.with(|f| f.set(false));
        let message =
            *failed.expect_err("over budget").downcast::<String>().expect("a formatted panic");
        assert!(message.starts_with("two boxes: 2 allocations (41 bytes), budget 0"), "{message}");
        assert!(message.contains("allocation 1 (1 bytes):\n"), "{message}");
        assert!(message.contains("allocation 2 (40 bytes):\n"), "{message}");
        assert!(
            message.contains("a_traced_failure_names_where_each_allocation_came_from"),
            "{message}"
        );
    }

    #[test]
    fn a_panic_after_a_traced_measurement_reports_its_allocations() {
        FORCE_TRACE.with(|f| f.set(true));
        let (_, used) = measure(|| Box::new([0u8; 24]));
        let report = panic_report();
        let _ = std::panic::catch_unwind(|| {
            assert_at_most(0, "a box", || Box::new(1u8));
        });
        let after_assert = panic_report();
        FORCE_TRACE.with(|f| f.set(false));
        assert_eq!(used.allocations, 1);
        let report = report.expect("a traced measurement's report");
        assert!(report.contains("made 1 traced allocations"), "{report}");
        assert!(report.contains("allocation 1 (24 bytes):\n"), "{report}");
        assert_eq!(after_assert, None, "assert_at_most's message has its traces");
    }

    #[test]
    fn tracing_counts_only_what_is_measured() {
        FORCE_TRACE.with(|f| f.set(true));
        let (_, used) = measure(|| Box::new(7u32));
        FORCE_TRACE.with(|f| f.set(false));
        assert_eq!(used.allocations, 1, "the backtrace and the trace list are not counted");
    }

    #[test]
    fn an_untraced_failure_says_how_to_trace() {
        let failed = std::panic::catch_unwind(|| assert_at_most(0, "a box", || Box::new(1u8)));
        if TRACE.get() == Some(&true) {
            return;
        }
        let message =
            *failed.expect_err("over budget").downcast::<String>().expect("a formatted panic");
        assert!(
            message.ends_with("(set BLAZE_ALLOC_TRACE=1 for a backtrace of each allocation)"),
            "{message}"
        );
    }

    #[test]
    fn counts_only_inside_measure() {
        let _outside = vec![1u8; 10];
        let (_, used) = measure(|| ());
        assert_eq!(used, Used::default());
    }

    #[test]
    fn counts_allocations_and_bytes() {
        let (v, used) = measure(|| Vec::<u8>::with_capacity(64));
        assert_eq!(v.capacity(), 64);
        assert_eq!(used.allocations, 1);
        assert_eq!(used.bytes, 64);
        assert_eq!(used.largest, 64);
    }

    #[test]
    fn nests() {
        let ((_, inner), outer) = measure(|| {
            let _a = std::hint::black_box(Box::new([0u8; 100]));
            measure(|| Box::new(2u64))
        });
        assert_eq!(inner.allocations, 1);
        assert_eq!(inner.largest, 8);
        assert_eq!(outer.allocations, 2);
        assert_eq!(outer.largest, 100);
    }

    #[test]
    fn an_inner_measure_keeps_the_outer_largest() {
        let ((), outer) = measure(|| {
            let _a = std::hint::black_box(Vec::<u8>::with_capacity(500));
            let _ = measure(|| Box::new(1u8));
        });
        assert_eq!(outer.largest, 500);
    }

    #[test]
    fn knows_it_is_installed() {
        assert!(installed());
    }

    #[test]
    #[should_panic(expected = "budget 0")]
    fn budget_fails_loudly() {
        assert_at_most(0, "no allocation", || Box::new(3u8));
    }
}
