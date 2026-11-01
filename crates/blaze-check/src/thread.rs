//! `std::thread`. In a model a spawned thread is a coroutine the explorer runs when the schedule
//! names it, a join waits for it to end (its events happen before the joiner's next), and park
//! and unpark keep a token per thread as `std`'s do. `sleep` and `yield_now` let the others run
//! first. [`thread_local!`](crate::thread_local) gives each model thread its own value, dropped
//! when it ends. Outside a model each is `std`'s.

use std::{
    any::Any,
    cell::{Cell, RefCell},
    error::Error,
    fmt, io,
    num::NonZero,
    panic::Location,
    rc::Rc,
    sync::Arc,
    time::Duration,
};

pub use std::thread::{Result, panicking};

use crate::{clock::Tid, coro, rt};

/// `std::thread::Builder`.
#[derive(Debug, Default)]
#[must_use = "must eventually spawn the thread"]
pub struct Builder {
    name: Option<String>,
    stack_size: Option<usize>,
}

impl Builder {
    pub fn new() -> Builder {
        Builder::default()
    }

    pub fn name(mut self, name: String) -> Builder {
        self.name = Some(name);
        self
    }

    /// Outside a model only: a model thread's stack is the model's `stack_size`.
    pub fn stack_size(mut self, size: usize) -> Builder {
        self.stack_size = Some(size);
        self
    }

    #[track_caller]
    pub fn spawn<F, T>(self, f: F) -> io::Result<JoinHandle<T>>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        if !rt::in_model() {
            let mut builder = std::thread::Builder::new();
            if let Some(name) = self.name {
                builder = builder.name(name);
            }
            if let Some(size) = self.stack_size {
                builder = builder.stack_size(size);
            }
            return builder.spawn(f).map(|handle| {
                // clone: std's `Thread`, a reference count
                let thread = Thread { inner: Inner::Std(handle.thread().clone()) };
                JoinHandle { inner: Handle::Std(handle, thread) }
            });
        }
        let name: Option<Arc<str>> = self.name.map(Arc::from);
        // alloc: the spawned thread's result, once per spawn, as std's `spawn`
        let result = Arc::new(parking_lot::Mutex::new(None));
        let slot = Arc::clone(&result);
        // alloc: the spawned thread's body, once per spawn, as std's `spawn`
        let body = coro::Job::Spawned(Box::new(move || {
            let out = f();
            *slot.lock() = Some(out);
            drop(slot);
            rt::thread_end();
        }));
        // clone: the thread's name, a reference count
        let (tid, epoch) = rt::spawn(Location::caller(), name.clone(), body)
            .expect("blaze-check: a model thread spawns outside its model");
        let thread = Thread { inner: Inner::Model { tid, epoch, name } };
        Ok(JoinHandle { inner: Handle::Model { tid, result, thread } })
    }
}

/// `std::thread::spawn`.
#[track_caller]
pub fn spawn<F, T>(f: F) -> JoinHandle<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    Builder::new().spawn(f).expect("failed to spawn thread")
}

/// `std::thread::JoinHandle`.
pub struct JoinHandle<T> {
    inner: Handle<T>,
}

enum Handle<T> {
    Std(std::thread::JoinHandle<T>, Thread),
    Model { tid: Tid, result: Arc<parking_lot::Mutex<Option<T>>>, thread: Thread },
}

impl<T> JoinHandle<T> {
    /// A model thread that panicked has stopped the model (a finding), so a model join is
    /// always `Ok`.
    #[track_caller]
    pub fn join(self) -> Result<T> {
        match self.inner {
            Handle::Std(handle, _) => handle.join(),
            Handle::Model { tid, result, .. } => {
                rt::join(Location::caller(), tid)
                    .expect("blaze-check: a model thread joined outside its model");
                let out = result.lock().take();
                Ok(out.expect("blaze-check: a joined model thread left its result"))
            }
        }
    }

    pub fn thread(&self) -> &Thread {
        match &self.inner {
            Handle::Std(_, thread) | Handle::Model { thread, .. } => thread,
        }
    }
}

impl<T> fmt::Debug for JoinHandle<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JoinHandle").finish_non_exhaustive()
    }
}

/// `std::thread::Thread`.
#[derive(Clone, Debug)]
pub struct Thread {
    inner: Inner,
}

#[derive(Clone, Debug)]
enum Inner {
    Std(std::thread::Thread),
    Model { tid: Tid, epoch: u64, name: Option<Arc<str>> },
}

impl Thread {
    pub fn id(&self) -> ThreadId {
        match &self.inner {
            Inner::Std(thread) => ThreadId(Id::Std(thread.id())),
            Inner::Model { tid, epoch, .. } => ThreadId(Id::Model(*epoch, *tid as u8)),
        }
    }

    pub fn name(&self) -> Option<&str> {
        match &self.inner {
            Inner::Std(thread) => thread.name(),
            Inner::Model { tid: 0, name: None, .. } => Some("main"),
            Inner::Model { name, .. } => name.as_deref(),
        }
    }

    #[track_caller]
    pub fn unpark(&self) {
        match &self.inner {
            Inner::Std(thread) => thread.unpark(),
            Inner::Model { tid, epoch, .. } => {
                rt::unpark(Location::caller(), *tid, *epoch);
            }
        }
    }
}

/// `std::thread::ThreadId`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ThreadId(Id);

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Id {
    Std(std::thread::ThreadId),
    /// A model thread: its execution's epoch and its index.
    Model(u64, u8),
}

/// `std::thread::current`.
pub fn current() -> Thread {
    match rt::current() {
        Some((tid, epoch)) => {
            let name = rt::current_name().flatten();
            Thread { inner: Inner::Model { tid, epoch, name } }
        }
        None => Thread { inner: Inner::Std(std::thread::current()) },
    }
}

/// `std::thread::yield_now`: in a model, every other thread that can run goes first.
#[track_caller]
pub fn yield_now() {
    if rt::yield_now(Location::caller()).is_none() {
        std::thread::yield_now();
    }
}

/// `std::thread::sleep`: in a model, a yield (time is not modelled).
#[track_caller]
pub fn sleep(dur: Duration) {
    if rt::yield_now(Location::caller()).is_none() {
        std::thread::sleep(dur);
    }
}

/// `std::thread::park`.
#[track_caller]
pub fn park() {
    if rt::park(Location::caller(), false).is_none() {
        std::thread::park();
    }
}

/// `std::thread::park_timeout`: in a model it may also return at once with no unpark (a
/// branch), which stands for every time the timeout could fire.
#[track_caller]
pub fn park_timeout(dur: Duration) {
    if rt::park(Location::caller(), true).is_none() {
        std::thread::park_timeout(dur);
    }
}

/// `std::thread::available_parallelism`: in a model, 2 (a structure sized by it gets more than
/// one shard, and a model needs no more).
pub fn available_parallelism() -> io::Result<NonZero<usize>> {
    match rt::in_model() {
        true => Ok(NonZero::new(2).expect("2 is not 0")),
        false => std::thread::available_parallelism(),
    }
}

/// `std::thread::LocalKey`, made by [`thread_local!`](crate::thread_local).
pub struct LocalKey<T: 'static> {
    std: &'static std::thread::LocalKey<T>,
    init: fn() -> T,
}

/// `std::thread::AccessError` (whose constructor is `std`'s own).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub struct AccessError;

impl fmt::Display for AccessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("already destroyed")
    }
}

impl Error for AccessError {}

enum Local<T> {
    Outside,
    Value(Rc<T>),
    Gone,
}

impl<T: 'static> LocalKey<T> {
    #[doc(hidden)]
    pub const fn __new(std: &'static std::thread::LocalKey<T>, init: fn() -> T) -> LocalKey<T> {
        LocalKey { std, init }
    }

    /// The running model thread's value, made on its first use.
    fn local(&'static self) -> Local<T> {
        let key = std::ptr::from_ref(self).addr();
        let found = rt::with(|rt| {
            let th = &rt.threads[rt.current()];
            if th.dropping_locals {
                return None;
            }
            Some(th.locals.iter().find(|(at, _)| *at == key).map(|(_, value)| Rc::clone(value)))
        });
        match found {
            None => Local::Outside,
            Some(None) => Local::Gone,
            Some(Some(Some(value))) => Local::Value(
                value.downcast::<T>().unwrap_or_else(|_| unreachable!("a key's values have its type")),
            ),
            Some(Some(None)) => {
                // alloc: a thread's `thread_local!` value, once per thread and key, as std's
                let value = Rc::new((self.init)());
                // dyn: see `rt::Thread::locals`.
                let shared: Rc<dyn Any> = Rc::clone(&value) as Rc<dyn Any>;
                rt::with(|rt| {
                    let thread = rt.current();
                    rt.threads[thread].locals.push((key, shared));
                });
                Local::Value(value)
            }
        }
    }

    pub fn with<F, R>(&'static self, f: F) -> R
    where
        F: FnOnce(&T) -> R,
    {
        match self.try_with(f) {
            Ok(out) => out,
            Err(_) => panic!(
                "cannot access a Thread Local Storage value during or after destruction: \
                 AccessError"
            ),
        }
    }

    pub fn try_with<F, R>(&'static self, f: F) -> std::result::Result<R, AccessError>
    where
        F: FnOnce(&T) -> R,
    {
        match self.local() {
            Local::Outside => self.std.try_with(f).map_err(|_| AccessError),
            Local::Value(value) => Ok(f(&*value)),
            Local::Gone => Err(AccessError),
        }
    }
}

impl<T: 'static> LocalKey<Cell<T>> {
    pub fn set(&'static self, value: T) {
        self.with(|cell| cell.set(value));
    }

    pub fn get(&'static self) -> T
    where
        T: Copy,
    {
        self.with(Cell::get)
    }

    pub fn take(&'static self) -> T
    where
        T: Default,
    {
        self.with(Cell::take)
    }

    pub fn replace(&'static self, value: T) -> T {
        self.with(|cell| cell.replace(value))
    }
}

impl<T: 'static> LocalKey<RefCell<T>> {
    pub fn with_borrow<F, R>(&'static self, f: F) -> R
    where
        F: FnOnce(&T) -> R,
    {
        self.with(|cell| f(&*cell.borrow()))
    }

    pub fn with_borrow_mut<F, R>(&'static self, f: F) -> R
    where
        F: FnOnce(&mut T) -> R,
    {
        self.with(|cell| f(&mut *cell.borrow_mut()))
    }

    pub fn set(&'static self, value: T) {
        self.with(|cell| *cell.borrow_mut() = value);
    }

    pub fn take(&'static self) -> T
    where
        T: Default,
    {
        self.with(RefCell::take)
    }

    pub fn replace(&'static self, value: T) -> T {
        self.with(|cell| cell.replace(value))
    }
}

impl<T: 'static> fmt::Debug for LocalKey<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalKey").finish_non_exhaustive()
    }
}

/// `std::thread_local!`, std's syntax (`const { }` initializers too), making
/// [`thread::LocalKey`](crate::thread::LocalKey)s: one value per model thread in a model, `std`'s
/// thread-local outside one.
#[macro_export]
macro_rules! thread_local {
    () => {};
    ($(#[$attr:meta])* $vis:vis static $name:ident: $t:ty = const { $($init:tt)* }; $($rest:tt)*) => {
        $crate::__thread_local_inner!($(#[$attr])* $vis $name, $t, { $($init)* });
        $crate::thread_local!($($rest)*);
    };
    ($(#[$attr:meta])* $vis:vis static $name:ident: $t:ty = const { $($init:tt)* }) => {
        $crate::__thread_local_inner!($(#[$attr])* $vis $name, $t, { $($init)* });
    };
    ($(#[$attr:meta])* $vis:vis static $name:ident: $t:ty = $init:expr; $($rest:tt)*) => {
        $crate::__thread_local_inner!($(#[$attr])* $vis $name, $t, $init);
        $crate::thread_local!($($rest)*);
    };
    ($(#[$attr:meta])* $vis:vis static $name:ident: $t:ty = $init:expr) => {
        $crate::__thread_local_inner!($(#[$attr])* $vis $name, $t, $init);
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __thread_local_inner {
    ($(#[$attr:meta])* $vis:vis $name:ident, $t:ty, $init:expr) => {
        $(#[$attr])*
        $vis static $name: $crate::thread::LocalKey<$t> = {
            ::std::thread_local!(static STD: $t = $init);
            fn init() -> $t {
                $init
            }
            $crate::thread::LocalKey::__new(&STD, init)
        };
    };
}
