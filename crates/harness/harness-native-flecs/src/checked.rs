//! Failures flecs reports instead of aborting. The vendored flecs is built with `FLECS_SOFT_ASSERT`: a refused call (a bad
//! argument, a pair that breaks `OneOf`, a trait added after a query) logs the error and does nothing, and
//! `ecs_log_last_error()` returns its code once. [`checked`] runs a step and returns that error as a value, so a feature
//! whose registration fails is marked degraded and skipped rather than taking the process down. The internal sanity checks
//! (`ecs_assert`) still abort: those are bugs in flecs or in the bindings, not in a feature.
use hyperion_ecs::sys;
use core::sync::atomic::{AtomicUsize, Ordering};

/// A flecs call that was refused: flecs's error code and the log line that says why.
#[derive(Clone, Debug)]
pub struct FlecsError {
    pub code: i32,
    pub message: String,
}

impl core::fmt::Display for FlecsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{} (flecs error {})", self.message, self.code)
    }
}

impl std::error::Error for FlecsError {}

static MESSAGE: AtomicUsize = AtomicUsize::new(0);

/// Where [`FlecsError::message`] comes from: the host's last flecs log line (harness-native-ecs's `last_error`). Without it the message
/// only names the code, which is all an image without the host's log hook can know.
pub fn set_message_source(source: fn() -> String) {
    MESSAGE.store(source as usize, Ordering::Relaxed);
}

fn message() -> String {
    let f = MESSAGE.load(Ordering::Relaxed);
    if f == 0 {
        return "flecs refused the call (see the log)".to_owned();
    }
    // SAFETY: only `set_message_source` stores here, always a `fn() -> String`.
    let f: fn() -> String = unsafe { core::mem::transmute::<usize, fn() -> String>(f) };
    f()
}

/// Forget any error flecs reported before this point.
pub fn clear_error() {
    // SAFETY: reads and resets flecs's last-error code.
    unsafe { sys::ecs_log_last_error() };
}

/// The error flecs reported since the last [`clear_error`] or `take_error`, once.
pub fn take_error() -> Option<FlecsError> {
    // SAFETY: reads and resets flecs's last-error code.
    let code = unsafe { sys::ecs_log_last_error() };
    (code != 0).then(|| FlecsError { code, message: message() })
}

/// Run `f` and return the first error flecs reported while it ran. What `f` did before the error stays done: a refused call
/// changes nothing, but the calls around it ran.
pub fn checked<T>(f: impl FnOnce() -> T) -> Result<T, FlecsError> {
    clear_error();
    let v = f();
    take_error().map_or(Ok(v), Err)
}
