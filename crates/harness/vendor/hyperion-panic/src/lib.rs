//! Per-image panic capture (docs/host-diet.md 10.4 item 6). A Rust panic cannot unwind into another image (each image links its own std,
//! and unwinding out of `extern "C"` aborts), so every entry another image can call runs under [`catch`] INSIDE its own image: the panic
//! message lands in this image's fixed buffer, the image is quarantined (every later entry returns its fault value at once), and the host
//! pulls the message through the image's `hyperion_take_panic` export ([`take`]). This crate is linked statically into every image, so these
//! statics are per image by construction.
use core::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

const MSG_CAP: usize = 1024;
const LOC_CAP: usize = 256;

struct Slot {
    msg: [u8; MSG_CAP],
    len: usize,
    loc: [u8; LOC_CAP],
    loc_len: usize,
}

// ALLOW-STATE: the image's panic mailbox, read by the host; fixed size, no heap.
static SLOT: Mutex<Slot> = Mutex::new(Slot { msg: [0; MSG_CAP], len: 0, loc: [0; LOC_CAP], loc_len: 0 });
static PENDING: AtomicBool = AtomicBool::new(false);
static QUARANTINED: AtomicBool = AtomicBool::new(false);
/// Set by a module image in `hyperion_set_host`: only module images quarantine themselves. The host image records its panics (pulled by
/// `Manager::poll`) but keeps running its other systems.
static QUARANTINE_ON_PANIC: AtomicBool = AtomicBool::new(false);

/// Module images call this once (`hyperion_set_host`): from now on a recorded fault quarantines the image.
pub fn enable_quarantine() {
    QUARANTINE_ON_PANIC.store(true, Ordering::Relaxed);
}

fn slot() -> std::sync::MutexGuard<'static, Slot> {
    SLOT.lock().unwrap_or_else(|p| p.into_inner())
}

/// Bytes appended to a fixed buffer, cut at the end (never allocates).
struct Buf<'a> {
    b: &'a mut [u8],
    n: usize,
}

impl core::fmt::Write for Buf<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let room = self.b.len() - self.n;
        let mut k = s.len().min(room);
        while k > 0 && !s.is_char_boundary(k) {
            k -= 1;
        }
        self.b[self.n..self.n + k].copy_from_slice(&s.as_bytes()[..k]);
        self.n += k;
        Ok(())
    }
}

/// True once this image caught a panic: its entries return their fault value until the image is unloaded.
#[inline]
pub fn quarantined() -> bool {
    QUARANTINED.load(Ordering::Relaxed)
}

/// Called by the image's panic hook: remember where the newest panic happened (the `catch_unwind` payload carries only the message).
pub fn note_location(l: &core::panic::Location<'_>) {
    let mut g = slot();
    let s = &mut *g;
    let mut w = Buf { b: &mut s.loc, n: 0 };
    let _ = core::fmt::write(&mut w, format_args!("{}:{}:{}", l.file(), l.line(), l.column()));
    s.loc_len = w.n;
}

/// Record a caught panic of entry `what` (first one wins until [`take`]) and quarantine the image (module images only).
pub fn record(what: &str, payload: &(dyn core::any::Any + Send)) {
    let msg = if let Some(s) = payload.downcast_ref::<&str>() {
        s
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.as_str()
    } else {
        "<non-string panic payload>"
    };
    write(what, msg, true);
}

/// A fault that is not a panic (a layout mismatch found at run time): recorded and quarantined like one.
pub fn record_fault(what: &str, msg: &str) {
    write(what, msg, false);
}

fn write(what: &str, msg: &str, with_loc: bool) {
    if QUARANTINE_ON_PANIC.load(Ordering::Relaxed) {
        QUARANTINED.store(true, Ordering::Relaxed);
    }
    if PENDING.load(Ordering::Acquire) {
        return;
    }
    let mut g = slot();
    let s = &mut *g;
    let loc_len = s.loc_len;
    let mut w = Buf { b: &mut s.msg, n: 0 };
    let loc = core::str::from_utf8(&s.loc[..loc_len]).unwrap_or("?");
    let _ = if with_loc { core::fmt::write(&mut w, format_args!("{what}: {msg} at {loc}")) } else { core::fmt::write(&mut w, format_args!("{what}: {msg}")) };
    s.len = w.n;
    PENDING.store(true, Ordering::Release);
}

/// Run `f`; on a panic record it and return `fault()`. Returns `fault()` without running `f` when the image is quarantined.
#[inline]
pub fn catch<R>(what: &str, fault: impl FnOnce() -> R, f: impl FnOnce() -> R) -> R {
    if quarantined() {
        return fault();
    }
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(p) => {
            record(what, &*p);
            fault()
        }
    }
}

/// Body of `hyperion_take_panic`: 0 when nothing is pending (one load), else the full length, `min(len, buf.len())` bytes copied, cleared.
pub fn take(buf: &mut [u8]) -> usize {
    if !PENDING.load(Ordering::Acquire) {
        return 0;
    }
    let mut g = slot();
    let n = g.len;
    let k = n.min(buf.len());
    buf[..k].copy_from_slice(&g.msg[..k]);
    g.len = 0;
    PENDING.store(false, Ordering::Release);
    n
}
