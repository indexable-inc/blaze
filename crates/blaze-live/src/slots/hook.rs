//! What a panic under [`super::Live::guard`] ran through: a panic hook that walks the stack
//! of a guarded thread when it panics, before it unwinds.

use std::{
    any::Any,
    cell::{Cell, RefCell},
    panic::{self, AssertUnwindSafe, PanicHookInfo},
    sync::Once,
};

/// How many frames of a panicking stack are looked at.
pub(crate) const FRAMES: usize = 256;

/// A panic of a guarded thread.
pub(crate) struct Panic {
    /// The code addresses of its frames, newest first: each a return address less one, so it
    /// is in the function that made the call.
    pub(crate) pcs: Vec<u64>,
    /// `panicked at <file>:<line>:<column>: <message>`.
    pub(crate) why: String,
}

thread_local! {
    static GUARDED: Cell<u32> = const { Cell::new(0) };
    static CAUGHT: RefCell<Option<Panic>> = const { RefCell::new(None) };
}

/// Installs the hook once in the process, in front of the one there was, which still runs.
pub(crate) fn install() {
    static HOOK: Once = Once::new();
    HOOK.call_once(|| {
        let previous = panic::take_hook();
        // alloc: setup, the panic hook, installed once per process
        panic::set_hook(Box::new(move |info| {
            if GUARDED.try_with(Cell::get).unwrap_or(0) != 0 {
                let mut pcs = Vec::with_capacity(FRAMES);
                super::backtrace(&mut pcs);
                let panic = Panic { pcs, why: describe(info) };
                _ = CAUGHT.try_with(|caught| {
                    if let Ok(mut caught) = caught.try_borrow_mut() {
                        *caught = Some(panic);
                    }
                });
            }
            previous(info);
        }));
    });
}

/// Runs `f`, catching a panic, with what the hook saw of it when it saw it.
#[allow(clippy::type_complexity)]
pub(crate) fn catch<R>(
    f: impl FnOnce() -> R,
    // dyn: std's panic payload, as `catch_unwind` hands it
) -> Result<R, (Box<dyn Any + Send + 'static>, Option<Panic>)> {
    GUARDED.with(|it| it.set(it.get() + 1));
    // The guard is where the program says what `f` touched may be dropped half done.
    let result = panic::catch_unwind(AssertUnwindSafe(f));
    GUARDED.with(|it| it.set(it.get() - 1));
    let caught = CAUGHT.with(|it| it.borrow_mut().take());
    result.map_err(|payload| (payload, caught))
}

fn describe(info: &PanicHookInfo<'_>) -> String {
    let payload = info.payload();
    let message = payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        // dyn: none: the text a payload of no known type is described as
        .unwrap_or("Box<dyn Any>");
    match info.location() {
        // alloc: once per panic under the guard: the reason the program and the linker get
        Some(at) => format!("panicked at {}:{}:{}: {message}", at.file(), at.line(), at.column()),
        // alloc: as above
        None => format!("panicked: {message}"),
    }
}
