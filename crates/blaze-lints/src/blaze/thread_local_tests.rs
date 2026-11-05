//! `thread_local_in_waiting_fn` over a fixture of its own: a stand-in `blaze_graph` whose
//! `sched::stack::Yielder::suspend` parks the fiber, an engine whose query (`variance` →
//! `fetch` → `get_or_run` → `suspend`) reaches it, a `std` with `thread::LocalKey`, a crate
//! `tls` with readers of its own thread-local, and a `main` that says what it expects. A line
//! of `main` marked `//~ thread_local` has the lint's finding.

use crate::tests::lint;

const MINICORE: &str = "//- minicore: sized, copy, clone, drop, deref, fn, unsize, coerce_unsized, dispatch_from_dyn, send, sync, option, result, builtin_impls, iterator\n";

/// The fiber layer, the engine whose query parks, `std`'s thread-locals and a crate of readers.
const CRATES: &str = r#"
//- /blaze_graph/lib.rs crate:blaze_graph deps:std
pub mod sched {
    pub mod stack {
        pub struct Yielder {
            stack: *mut u8,
        }
        impl Yielder {
            pub fn suspend(&self, value: u32) -> u32 {
                let _ = self.stack;
                value
            }
        }
    }
}
//- /engine.rs crate:engine deps:blaze_graph
pub mod fiber {
    pub struct Db {
        yielder: *const blaze_graph::sched::stack::Yielder,
    }

    unsafe impl Send for Db {}
    unsafe impl Sync for Db {}

    pub fn get_or_run(db: &Db, key: u32) -> u32 {
        unsafe { (*db.yielder).suspend(key) }
    }
}

pub use fiber::Db;

pub fn fetch(db: &Db, key: u32) -> u32 {
    fiber::get_or_run(db, key)
}

impl Db {
    pub fn variance(&self, def: u32) -> u32 {
        fetch(self, def)
    }
}
//- /std/lib.rs crate:std
pub mod prelude {
    pub mod rust_2015 { pub use core::prelude::rust_2015::*; }
    pub mod rust_2018 { pub use core::prelude::rust_2018::*; }
    pub mod rust_2021 { pub use core::prelude::rust_2021::*; }
    pub mod rust_2024 { pub use core::prelude::rust_2024::*; }
}
pub mod cell {
    pub struct Cell<T> {
        value: T,
    }
    impl<T: Copy> Cell<T> {
        pub fn get(&self) -> T {
            self.value
        }
    }
}
pub mod thread {
    pub struct LocalKey<T: 'static> {
        inner: fn() -> *const T,
    }
    impl<T: 'static> LocalKey<T> {
        pub const fn new(inner: fn() -> *const T) -> LocalKey<T> {
            LocalKey { inner }
        }
        pub fn with<F: FnOnce(&T) -> R, R>(&'static self, f: F) -> R {
            f(unsafe { &*(self.inner)() })
        }
    }
    impl<T: Copy + 'static> LocalKey<crate::cell::Cell<T>> {
        pub fn get(&'static self) -> T {
            self.with(|it| it.get())
        }
    }
}
//- /tls.rs crate:tls deps:std
use std::thread::LocalKey;

pub static SHARED: LocalKey<u32> = LocalKey::new(shared);

fn shared() -> *const u32 {
    0 as *const u32
}

#[inline]
pub fn inline_reader() -> u32 {
    SHARED.with(|it| *it)
}

pub fn plain_reader() -> u32 {
    SHARED.with(|it| *it)
}

pub fn generic_reader<T>(_value: T) -> u32 {
    SHARED.with(|it| *it)
}

#[inline(never)]
pub fn never_reader<T>(_value: T) -> u32 {
    SHARED.with(|it| *it)
}
"#;

const MAIN: &str = r#"//- /src/lib.rs crate:main deps:engine,std,blaze_graph,tls
use engine::Db;
use std::cell::Cell;
use std::thread::LocalKey;

static SLOT: LocalKey<u32> = LocalKey::new(slot);
static COUNT: LocalKey<Cell<u32>> = LocalKey::new(count);

fn slot() -> *const u32 {
    0 as *const u32
}

fn count() -> *const Cell<u32> {
    0 as *const Cell<u32>
}
"#;

const FLAGS: &str =
    "-A clippy::all --allow=blaze::general --deny=blaze::thread_local_in_waiting_fn";

/// The lines of `src/lib.rs` with a finding of the lint in `out`, in order.
fn found(out: &str) -> Vec<u32> {
    let mut found = Vec::new();
    let mut ours = false;
    for line in out.lines() {
        if let Some(message) =
            line.strip_prefix("error: ").or_else(|| line.strip_prefix("warning: "))
        {
            ours = message.contains("which may park the fiber at `");
            continue;
        }
        let Some(at) = line.trim_start().strip_prefix("--> src/lib.rs:") else { continue };
        if !std::mem::take(&mut ours) {
            continue;
        }
        let number = at.split(':').next().and_then(|it| it.parse().ok());
        found.push(number.expect("a line number after the path"));
    }
    found.sort_unstable();
    found
}

/// The lines of `source`, the whole of `src/lib.rs`, marked `//~ thread_local`.
fn expected(source: &str) -> Vec<u32> {
    let marked = source.lines().enumerate().filter(|(_, line)| line.contains("//~ thread_local"));
    marked.map(|(index, _)| index as u32 + 1).collect()
}

/// Lints [`MAIN`] followed by `items` and checks the findings are on the marked lines; returns
/// the output.
#[track_caller]
fn check(items: &str) -> String {
    let source = format!(
        "{}{}",
        MAIN.split_once('\n').map_or("", |(_, rest)| rest),
        items.trim_start_matches('\n')
    );
    let fixture = format!("{MINICORE}{CRATES}{MAIN}{}", items.trim_start_matches('\n'));
    let out = lint(&fixture, FLAGS);
    assert_eq!(found(&out), expected(&source), "the findings of:\n{source}\nwere:\n{out}");
    out
}

/// A thread-local read in a function that may park the fiber, before or after its wait, through
/// `with` or `get`, is found naming the static, the function and the call that parks; so is
/// one in an `#[inline(never)]` function that waits itself.
#[test]
fn a_read_in_a_function_that_may_park_is_found_naming_the_thread_local() {
    let out = check(
        r#"
pub fn reads_then_waits(db: &Db) -> u32 {
    let first = SLOT.with(|it| *it); //~ thread_local
    db.variance(first)
}

pub fn gets_after_waiting(db: &Db) -> u32 {
    let _ = db.variance(1);
    COUNT.get() //~ thread_local
}

#[inline(never)]
pub fn waits_itself(db: &Db) -> u32 {
    let v = db.variance(1);
    v + SLOT.with(|it| *it) //~ thread_local
}
"#,
    );
    assert!(
        out.contains(
            "the thread-local `main::SLOT` is read through `LocalKey::with` in `fn reads_then_waits`, which may park the fiber at `variance` → `fetch` → `get_or_run` → `suspend`"
        ),
        "the finding names the static, the function and the call that parks:\n{out}"
    );
    assert!(
        out.contains("the thread-local `main::COUNT` is read through `LocalKey::get`"),
        "{out}"
    );
    assert!(out.contains("`#[inline(never)]` on `fn waits_itself` does not help"), "{out}");
}

/// The same read moved into an `#[inline(never)]` reader that does not wait, and called from
/// the function that waits, is the fix: no finding, in the reader or at its call.
#[test]
fn a_read_in_an_inline_never_reader_that_does_not_wait_is_not_found() {
    check(
        r#"
#[inline(never)]
fn reader() -> u32 {
    SLOT.with(|it| *it)
}

pub fn through_a_reader(db: &Db) -> u32 {
    let first = reader();
    db.variance(first)
}

pub fn through_another_crates_reader(db: &Db) -> u32 {
    let first = tls::never_reader(1u32);
    db.variance(first)
}
"#,
    );
}

/// A call of a reader that can be inlined into the function that waits is found at the call,
/// naming the reader and its thread-local: `#[inline]` or plain in this crate, `#[inline]` or
/// generic in another. Another crate's plain reader can't be inlined here and is not found.
#[test]
fn a_call_of_an_inlinable_reader_from_a_function_that_may_park_is_found() {
    let out = check(
        r#"
#[inline]
fn inline_reader() -> u32 {
    SLOT.with(|it| *it)
}

fn plain_reader() -> u32 {
    COUNT.get()
}

pub fn through_an_inline_reader(db: &Db) -> u32 {
    let first = inline_reader(); //~ thread_local
    db.variance(first)
}

pub fn through_a_plain_reader(db: &Db) -> u32 {
    let v = db.variance(1);
    v + plain_reader() //~ thread_local
}

pub fn through_other_crates(db: &Db) -> u32 {
    let a = tls::inline_reader(); //~ thread_local
    let b = tls::plain_reader();
    let c = tls::generic_reader(1u32); //~ thread_local
    db.variance(a + b + c)
}
"#,
    );
    assert!(
        out.contains(
            "`fn inline_reader` reads the thread-local `main::SLOT` through `LocalKey::with` and can be inlined into `fn through_an_inline_reader`"
        ),
        "the finding names the reader and its thread-local:\n{out}"
    );
    assert!(out.contains("`fn inline_reader` is `#[inline]`"), "{out}");
    assert!(out.contains("`fn plain_reader` is of this crate and not `#[inline(never)]`"), "{out}");
    assert!(out.contains("`fn generic_reader` reads the thread-local `tls::SHARED`"), "{out}");
    assert!(out.contains("`fn generic_reader` is generic, so built in this crate"), "{out}");
}

/// A function that reads a thread-local and never parks the fiber is not found, nor is its
/// call of an `#[inline]` reader.
#[test]
fn a_read_in_a_function_that_does_not_park_is_not_found() {
    check(
        r#"
#[inline]
fn inline_reader() -> u32 {
    SLOT.with(|it| *it)
}

pub fn no_wait() -> u32 {
    SLOT.with(|it| *it) + COUNT.get() + inline_reader() + tls::inline_reader()
}
"#,
    );
}
