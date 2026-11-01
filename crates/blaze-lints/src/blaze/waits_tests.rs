//! The lints of waits (`held_across_wait`, `lock_across_query`, `blocking_on_pool`,
//! `unnamed_wait`) over fixtures of their own: a stand-in `corosensei` whose `Yielder::suspend`
//! parks the fiber, an engine whose query (`variance` → `fetch` → `get_or_run` → `suspend`)
//! reaches it, a `std` with the thread-locals, locks, channels and IO they look for, and a `main`
//! that says what it expects. A line of `main` marked `//~ held_across_wait` has that lint's
//! finding; `//~^ held_across_wait` the line above has one.

use crate::tests::lint;

mod blocking;
mod held;
mod lock;
mod unnamed;

const MINICORE: &str = "//- minicore: sized, copy, clone, drop, deref, fn, unsize, coerce_unsized, dispatch_from_dyn, send, sync, option, result, builtin_impls, iterator\n";

/// `corosensei`, and the engine of `hir_ty_suspend`'s own tests: its `fiber` module is the fiber
/// driver (it calls the root), and `spawn` runs a job there.
const ENGINE: &str = r#"
//- /corosensei.rs crate:corosensei
pub struct Yielder {
    stack: *mut u8,
}

impl Yielder {
    pub fn suspend(&self, value: u32) -> u32 {
        let _ = self.stack;
        value
    }
}

//- /engine.rs crate:engine deps:corosensei
pub mod fiber {
    pub struct Db {
        yielder: *const corosensei::Yielder,
    }

    unsafe impl Send for Db {}
    unsafe impl Sync for Db {}

    pub fn get_or_run(db: &Db, key: u32) -> u32 {
        unsafe { (*db.yielder).suspend(key) }
    }

    pub fn spawn<F: FnOnce()>(job: F) {
        job()
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

/// `!Send`, as `Rc` is: the may-suspend check's own error, never this crate's.
pub struct Rc {
    ptr: *const u32,
}

impl Drop for Rc {
    fn drop(&mut self) {}
}

pub fn rc() -> Rc {
    Rc { ptr: 0 as *const u32 }
}
"#;

/// The `std` the lints resolve their items in, by the paths std has them at.
const STD: &str = r#"
//- /std/lib.rs crate:std
pub mod prelude {
    pub mod rust_2015 { pub use core::prelude::rust_2015::*; }
    pub mod rust_2018 { pub use core::prelude::rust_2018::*; }
    pub mod rust_2021 { pub use core::prelude::rust_2021::*; }
    pub mod rust_2024 { pub use core::prelude::rust_2024::*; }
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
    pub struct JoinHandle<T>(*mut T);
    impl<T> JoinHandle<T> {
        pub fn join(self) -> T {
            loop {}
        }
    }
    pub fn sleep(_nanos: u64) {}
    pub fn park() {}
    pub fn park_timeout(_nanos: u64) {}
}
pub mod hint {
    pub fn spin_loop() {}
}
pub mod sync {
    pub struct Mutex<T>(*mut T);
    pub struct MutexGuard<'a, T>(&'a Mutex<T>);
    impl<T> Mutex<T> {
        pub fn lock(&self) -> MutexGuard<'_, T> {
            MutexGuard(self)
        }
    }
    impl<T> Drop for MutexGuard<'_, T> {
        fn drop(&mut self) {}
    }
    pub struct Condvar;
    impl Condvar {
        pub fn wait<'a, T>(&self, guard: MutexGuard<'a, T>) -> MutexGuard<'a, T> {
            guard
        }
        pub fn notify_one(&self) {}
    }
    pub mod mpsc {
        pub struct Receiver<T>(*mut T);
        impl<T> Receiver<T> {
            pub fn recv(&self) -> Option<T> {
                None
            }
            pub fn try_recv(&self) -> Option<T> {
                None
            }
        }
    }
}
pub mod io {
    pub trait Read {
        fn read(&mut self, buf: &mut [u8]) -> usize;
    }
    pub trait Write {
        fn write(&mut self, buf: &[u8]) -> usize;
    }
}
pub mod fs {
    pub struct File(u32);
    impl File {
        pub fn open(_path: &str) -> File {
            File(0)
        }
        pub fn options() -> OpenOptions {
            OpenOptions(0)
        }
    }
    impl crate::io::Read for File {
        fn read(&mut self, _buf: &mut [u8]) -> usize {
            0
        }
    }
    pub struct OpenOptions(u32);
    impl OpenOptions {
        pub fn read(&mut self, _yes: bool) -> &mut OpenOptions {
            self
        }
        pub fn open(&self, _path: &str) -> File {
            File(0)
        }
    }
    pub fn read(_path: &str) -> u32 {
        0
    }
}
pub mod net {
    pub struct TcpStream(u32);
    impl TcpStream {
        pub fn connect(_addr: &str) -> TcpStream {
            TcpStream(0)
        }
        pub fn local_addr(&self) -> u32 {
            0
        }
    }
    pub struct TcpListener(u32);
    pub struct UdpSocket(u32);
}
"#;

/// `blaze_graph`'s wait API and `NoWait`, where the crate keeps them.
const BLAZE_GRAPH: &str = r#"
//- /blaze_graph/lib.rs crate:blaze_graph deps:std
pub mod sched {
    pub mod fiber {
        pub struct NoWait(());
        impl NoWait {
            pub fn new() -> NoWait {
                NoWait(())
            }
        }
        impl Drop for NoWait {
            fn drop(&mut self) {}
        }
    }
    pub mod host {
        pub fn wait(_on: super::Wait) -> Result<(), ()> {
            Ok(())
        }
        pub struct EventWait(u32);
        impl EventWait {
            pub fn wait(self) -> Result<(), ()> {
                wait(super::Wait::Key(self.0))
            }
        }
    }
    pub enum Wait {
        Key(u32),
        Until(u64),
    }
    pub use fiber::NoWait;
    pub use host::{EventWait, wait};
}
pub mod pump {
    pub struct Flights(u32);
    impl Flights {
        pub fn get_or_run(&self, key: u32, run: impl FnOnce() -> u32) -> u32 {
            self.get_or_run_named(key, |_| {}, run)
        }
        pub fn get_or_run_named(&self, _key: u32, mut named: impl FnMut(&mut Name), run: impl FnOnce() -> u32) -> u32 {
            let mut name = Name;
            named(&mut name);
            run()
        }
        pub fn hold(&self, _key: u32) -> Option<u32> {
            None
        }
        pub fn hold_named(&self, _key: u32, _named: impl FnMut(&mut Name)) -> Option<u32> {
            None
        }
    }
    pub struct Name;
    impl Name {
        pub fn push_str(&mut self, _text: &str) {}
    }
}
pub mod wait {
    pub enum WaitOn<'a> {
        Key(&'a str),
        Lock(&'static str),
        Channel(&'static str),
        Io(&'a str),
        Child(u32),
    }
    pub fn wait<R>(_on: WaitOn<'_>, _owner: Option<u64>, block: impl FnOnce() -> R) -> R {
        block()
    }
    pub fn lock<'a, T>(lock: &'a std::sync::Mutex<T>, _name: &'static str) -> std::sync::MutexGuard<'a, T> {
        lock.lock()
    }
    pub fn wait_until<'a, T>(
        _condvar: &std::sync::Condvar,
        held: std::sync::MutexGuard<'a, T>,
        _name: &'static str,
        _until: impl FnMut(&mut T) -> bool,
    ) -> std::sync::MutexGuard<'a, T> {
        held
    }
}
"#;

/// Every lint of these tests, by name after `blaze::`.
const LINTS: &[&str] = &["held_across_wait", "lock_across_query", "blocking_on_pool", "unnamed_wait"];

/// The lint a message is of.
fn lint_of(message: &str) -> Option<&'static str> {
    if message.contains("tied to this thread") {
        Some("held_across_wait")
    } else if message.contains("held across the query `") {
        Some("lock_across_query")
    } else if message.contains("blocks the pool thread") {
        Some("blocking_on_pool")
    } else if message.contains("names nothing it waits for") || message.contains("names what it waits for only as") {
        Some("unnamed_wait")
    } else {
        None
    }
}

/// The findings of these lints in `src/lib.rs` of `out`, as (line, lint).
fn found(out: &str) -> Vec<(u32, &'static str)> {
    let mut found = Vec::new();
    let mut lint = None;
    for line in out.lines() {
        if let Some(message) = line.strip_prefix("error: ").or_else(|| line.strip_prefix("warning: ")) {
            lint = lint_of(message);
            continue;
        }
        let Some(at) = line.trim_start().strip_prefix("--> src/lib.rs:") else { continue };
        let Some(name) = lint.take() else { continue };
        let number = at.split(':').next().and_then(|it| it.parse().ok());
        found.push((number.expect("a line number after the path"), name));
    }
    found.sort_unstable();
    found
}

/// What the markers of `source`, the whole of `src/lib.rs`, expect, as (line, lint).
fn expected(source: &str) -> Vec<(u32, &'static str)> {
    let mut expected = Vec::new();
    for (index, line) in source.lines().enumerate() {
        let number = index as u32 + 1;
        let Some((_, marks)) = line.split_once("//~") else { continue };
        let (number, marks) = match marks.strip_prefix('^') {
            Some(marks) => (number - 1, marks),
            None => (number, marks),
        };
        for mark in marks.split_whitespace() {
            let lint = LINTS.iter().find(|&&it| it == mark).unwrap_or_else(|| panic!("`{mark}` is not a lint of these tests"));
            expected.push((number, *lint));
        }
    }
    expected.sort_unstable();
    expected
}

/// The fixture of `main` beside [`ENGINE`], [`STD`] and [`BLAZE_GRAPH`] and `crates`, which
/// ends with the meta line of `main`; and `main`, as `src/lib.rs` has it.
fn fixture(crates: &str, main: &str) -> (String, String) {
    let source = main.trim_start_matches('\n').to_owned();
    (format!("{MINICORE}{ENGINE}{STD}{BLAZE_GRAPH}{crates}{source}"), source)
}

/// Checks that the findings of `out` in `source` are what its markers say.
#[track_caller]
fn assert_marked(out: &str, source: &str, flags: &str) {
    assert_eq!(found(out), expected(source), "with `{flags}`, the findings of:\n{source}\nwere:\n{out}");
}

/// Lints `main` with `flags`, `crates` beside the stand-ins, and checks the findings are what
/// its markers say; returns the output.
#[track_caller]
fn check_in(crates: &str, main: &str, flags: &str) -> String {
    let (fixture, source) = fixture(crates, main);
    let out = lint(&fixture, flags);
    assert_marked(&out, &source, flags);
    out
}
