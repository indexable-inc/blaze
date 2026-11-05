//! A clock that also runs in a browser.

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
pub use std::time::Instant;

/// `std::time::Instant`, where the standard library has no clock: the page provides one as
/// the import `rustd.now`, in milliseconds.
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
#[derive(Clone, Copy, PartialEq, PartialOrd, Debug)]
pub struct Instant(f64);

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
impl Instant {
    pub fn now() -> Instant {
        #[link(wasm_import_module = "rustd")]
        unsafe extern "C" {
            safe fn now() -> f64;
        }
        Instant(now())
    }

    pub fn elapsed(&self) -> std::time::Duration {
        Instant::now() - *self
    }
}

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
impl std::ops::Sub for Instant {
    type Output = std::time::Duration;

    fn sub(self, earlier: Instant) -> std::time::Duration {
        std::time::Duration::from_secs_f64((self.0 - earlier.0).max(0.0) / 1000.0)
    }
}
