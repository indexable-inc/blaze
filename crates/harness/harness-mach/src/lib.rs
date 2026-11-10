//! Mach messages between the harness browser host (crates/harness/harness-browser) and the app (harness-native): the
//! one channel that can carry an IOSurface from one process to another (a Unix socket cannot carry a Mach port).
//!
//! - The host checks in a bootstrap service ([`RecvPort::check_in`], name from `harness_proto::browser::service_name`);
//!   the app looks it up ([`SendRight::look_up`]), makes a receive port of its own ([`RecvPort::new`]) and sends the host
//!   a send right to it in its first message. From then on each side sends to the other's port.
//! - A message is bytes (one CBOR value; the caller's) and up to [`MAX_PORTS`] send rights ([`SendRight::send`]). The
//!   layout is [`codec`]'s, a sans-IO encoder and decoder of the buffer, so the framing is tested without a kernel.
//! - [`iosurface`] turns an IOSurface into a send right and back (`IOSurfaceCreateMachPort`,
//!   `IOSurfaceLookupFromMachPort`).
//! - [`Source`] drains a receive port on the main dispatch queue (a `DISPATCH_SOURCE_TYPE_MACH_RECV` source), so the
//!   app's and the host's messages are handled on their main threads, where AppKit, Metal's textures and CEF's UI
//!   thread are, with no thread of ours blocking in the kernel.
//!
//! Private interfaces (rule macos-private-apis): `bootstrap_check_in` for a name no launchd plist declares is
//! undocumented, and wins over the alternatives: XPC's mach services need a launchd job, global IOSurfaces
//! (`kIOSurfaceIsGlobal`, deprecated) are readable by any process of any user, and passing ports to a spawned child
//! (`posix_spawnattr_setspecialport_np`) only reaches a child, while the app is no child of the host. Chromium's own
//! `MachPortRendezvousServer` checks in a dynamic name the same way. `tests::symbols_resolve` fails naming any symbol an
//! OS update removed.
//!
//! Sends never block: they carry `MACH_SEND_TIMEOUT` 0, so a receiver that stopped draining makes a send fail
//! [`SendError::Full`] (the host drops that frame) instead of stalling the sender's main thread. Each receive port's queue
//! is [`QUEUE_LIMIT`] deep.
#![cfg(target_vendor = "apple")]
use std::ffi::{CString, c_char, c_int, c_void};
use std::fmt;

pub mod codec;
pub mod iosurface;

#[allow(non_camel_case_types)]
type mach_port_t = u32;
#[allow(non_camel_case_types)]
type kern_return_t = c_int;

/// The most port rights one message carries (a ring's slots, with room).
pub const MAX_PORTS: usize = 8;
/// The most bytes one message carries.
pub const MAX_BODY: usize = 256 << 10;
/// Messages a receive port queues before a send to it fails [`SendError::Full`] (`MACH_PORT_QLIMIT_MAX`).
pub const QUEUE_LIMIT: u32 = 1024;
/// The most messages one [`Source`] callback drains before it returns to the queue (dispatch calls it again while more
/// wait), so one busy sender cannot starve the main thread.
const DRAIN_MAX: usize = 256;

const KERN_SUCCESS: kern_return_t = 0;
const MACH_PORT_NULL: mach_port_t = 0;
const MACH_PORT_RIGHT_RECEIVE: c_int = 1;
const MACH_MSG_TYPE_MAKE_SEND: u32 = 20;
const MACH_PORT_LIMITS_INFO: c_int = 1;
const MACH_SEND_MSG: c_int = 0x1;
const MACH_RCV_MSG: c_int = 0x2;
const MACH_SEND_TIMEOUT: c_int = 0x10;
const MACH_RCV_TIMEOUT: c_int = 0x100;
const MACH_SEND_TIMED_OUT: kern_return_t = 0x1000_0004;
const MACH_SEND_INVALID_DEST: kern_return_t = 0x1000_0003;
const MACH_RCV_TIMED_OUT: kern_return_t = 0x1000_4003;
const BOOTSTRAP_NOT_PRIVILEGED: kern_return_t = 1100;
const BOOTSTRAP_UNKNOWN_SERVICE: kern_return_t = 1102;
const BOOTSTRAP_SERVICE_ACTIVE: kern_return_t = 1103;

unsafe extern "C" {
    static mach_task_self_: mach_port_t;
    static bootstrap_port: mach_port_t;
    fn mach_port_allocate(task: mach_port_t, right: c_int, name: *mut mach_port_t) -> kern_return_t;
    fn mach_port_insert_right(task: mach_port_t, name: mach_port_t, poly: mach_port_t, poly_type: u32) -> kern_return_t;
    fn mach_port_deallocate(task: mach_port_t, name: mach_port_t) -> kern_return_t;
    fn mach_port_mod_refs(task: mach_port_t, name: mach_port_t, right: c_int, delta: c_int) -> kern_return_t;
    fn mach_port_set_attributes(task: mach_port_t, name: mach_port_t, flavor: c_int, info: *mut c_int, count: u32) -> kern_return_t;
    fn mach_msg(msg: *mut c_void, option: c_int, send_size: u32, rcv_size: u32, rcv_name: mach_port_t, timeout: u32, notify: mach_port_t) -> kern_return_t;
    // servers/bootstrap.h: undocumented for names launchd does not know (see the module doc).
    fn bootstrap_check_in(bp: mach_port_t, service_name: *const c_char, sp: *mut mach_port_t) -> kern_return_t;
    fn bootstrap_look_up(bp: mach_port_t, service_name: *const c_char, sp: *mut mach_port_t) -> kern_return_t;
    fn mach_error_string(kr: kern_return_t) -> *const c_char;
}

fn task() -> mach_port_t {
    // SAFETY: a process-lifetime constant libsystem sets before main.
    unsafe { mach_task_self_ }
}

fn kr_text(kr: kern_return_t) -> String {
    // SAFETY: mach_error_string returns a static C string for any code.
    let s = unsafe { std::ffi::CStr::from_ptr(mach_error_string(kr)) };
    format!("{} (0x{kr:x})", s.to_string_lossy())
}

/// Why a Mach call failed, naming the call, the service or port, and the kernel's words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

/// Why a send failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendError {
    /// The receiver's queue is full (it stopped draining): nothing was sent.
    Full,
    /// The receiver is gone (its receive right was destroyed): nothing was sent, and nothing will be.
    Dead,
    /// The message is too big ([`MAX_BODY`], [`MAX_PORTS`]), or the kernel refused it: the words name which.
    Other(String),
}

impl fmt::Display for SendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full => f.write_str("the receiver's Mach queue is full (it is not draining)"),
            Self::Dead => f.write_str("the receiver's Mach port is dead (its process ended or dropped it)"),
            Self::Other(s) => f.write_str(s),
        }
    }
}

impl std::error::Error for SendError {}

/// A receive right this process owns (destroyed on drop).
#[derive(Debug)]
pub struct RecvPort(mach_port_t);

/// A send right this process owns (its reference released on drop).
#[derive(Debug)]
pub struct SendRight(mach_port_t);

/// One message received: its bytes and the send rights it carried, in order.
#[derive(Debug)]
pub struct Msg {
    pub body: Vec<u8>,
    pub ports: Vec<SendRight>,
}

fn set_qlimit(port: mach_port_t) -> Result<(), Error> {
    let mut limit = QUEUE_LIMIT as c_int;
    // SAFETY: MACH_PORT_LIMITS_INFO takes one natural_t (mach_port_limits_t { mpl_qlimit }).
    let kr = unsafe { mach_port_set_attributes(task(), port, MACH_PORT_LIMITS_INFO, &mut limit, 1) };
    if kr != KERN_SUCCESS {
        return Err(Error(format!("mach_port_set_attributes(qlimit {QUEUE_LIMIT}) on port {port}: {}", kr_text(kr))));
    }
    Ok(())
}

impl RecvPort {
    /// A new receive port, [`QUEUE_LIMIT`] deep.
    pub fn new() -> Result<Self, Error> {
        let mut name = MACH_PORT_NULL;
        // SAFETY: out-pointer to a local.
        let kr = unsafe { mach_port_allocate(task(), MACH_PORT_RIGHT_RECEIVE, &mut name) };
        if kr != KERN_SUCCESS {
            return Err(Error(format!("mach_port_allocate(receive): {}", kr_text(kr))));
        }
        let port = Self(name);
        set_qlimit(name)?;
        Ok(port)
    }

    /// Check in bootstrap service `service`: its receive right, which only this process then holds. Fails naming the
    /// service when another process already serves it (another host for the same `HARNESS_HOME`) or launchd refuses.
    pub fn check_in(service: &str) -> Result<Self, Error> {
        let c = CString::new(service).map_err(|_| Error(format!("bootstrap service name {service:?} has a NUL")))?;
        let mut name = MACH_PORT_NULL;
        // SAFETY: a NUL-terminated name and an out-pointer to a local; bootstrap_port is libsystem's, set before main.
        let kr = unsafe { bootstrap_check_in(bootstrap_port, c.as_ptr(), &mut name) };
        match kr {
            KERN_SUCCESS => {
                let port = Self(name);
                set_qlimit(name)?;
                Ok(port)
            }
            BOOTSTRAP_SERVICE_ACTIVE => Err(Error(format!("bootstrap service {service} is already served by another process (another harness browser host for this HARNESS_HOME is running)"))),
            BOOTSTRAP_NOT_PRIVILEGED => Err(Error(format!("bootstrap_check_in({service}): not privileged (this process is in another bootstrap namespace than its launchd session; start the daemon from the logged-in session, not over ssh)"))),
            kr => Err(Error(format!("bootstrap_check_in({service}): {}", kr_text(kr)))),
        }
    }

    /// A send right to this port, to hand to the other side.
    pub fn make_send(&self) -> Result<SendRight, Error> {
        // SAFETY: inserting a send right for a receive right we hold, under the same name.
        let kr = unsafe { mach_port_insert_right(task(), self.0, self.0, MACH_MSG_TYPE_MAKE_SEND) };
        if kr != KERN_SUCCESS {
            return Err(Error(format!("mach_port_insert_right(make send) on port {}: {}", self.0, kr_text(kr))));
        }
        Ok(SendRight(self.0))
    }

    /// The next message, waiting at most `timeout_ms` (0: only what is queued now). `Ok(None)`: none came by then.
    pub fn recv(&self, timeout_ms: u32) -> Result<Option<Msg>, Error> {
        self.recv_with(&mut codec::RecvBuf::new(), timeout_ms)
    }

    /// [`Self::recv`] into a buffer the caller keeps between receives (a drain loop's).
    pub fn recv_with(&self, buf: &mut codec::RecvBuf, timeout_ms: u32) -> Result<Option<Msg>, Error> {
        // SAFETY: buf is RecvBuf::LEN bytes, 8-aligned, and the kernel writes at most that (it refuses a larger message).
        let kr = unsafe { mach_msg(buf.as_mut_ptr(), MACH_RCV_MSG | MACH_RCV_TIMEOUT, 0, codec::RecvBuf::LEN as u32, self.0, timeout_ms, MACH_PORT_NULL) };
        match kr {
            KERN_SUCCESS => {}
            MACH_RCV_TIMED_OUT => return Ok(None),
            kr => return Err(Error(format!("mach_msg(receive) on port {}: {}", self.0, kr_text(kr)))),
        }
        let d = buf.decode().map_err(|e| Error(format!("a message on port {}: {e}", self.0)))?;
        Ok(Some(Msg { body: d.body, ports: d.ports.into_iter().map(SendRight).collect() }))
    }

    #[must_use]
    pub fn raw(&self) -> u32 {
        self.0
    }
}

impl Drop for RecvPort {
    fn drop(&mut self) {
        // SAFETY: we own one receive right under this name; dropping it makes senders' rights dead names.
        unsafe { mach_port_mod_refs(task(), self.0, MACH_PORT_RIGHT_RECEIVE, -1) };
    }
}

impl SendRight {
    /// Look up bootstrap service `service`. Fails naming it when no process serves it (the host is not running).
    pub fn look_up(service: &str) -> Result<Self, Error> {
        let c = CString::new(service).map_err(|_| Error(format!("bootstrap service name {service:?} has a NUL")))?;
        let mut name = MACH_PORT_NULL;
        // SAFETY: as in check_in.
        let kr = unsafe { bootstrap_look_up(bootstrap_port, c.as_ptr(), &mut name) };
        match kr {
            KERN_SUCCESS => Ok(Self(name)),
            BOOTSTRAP_UNKNOWN_SERVICE => Err(Error(format!("bootstrap service {service} is not served (the harness browser host is not running, or runs in another bootstrap namespace)"))),
            kr => Err(Error(format!("bootstrap_look_up({service}): {}", kr_text(kr)))),
        }
    }

    /// Send `body` and `ports` (each send right copied: the caller keeps its own, dropped as usual). Never blocks.
    pub fn send(&self, body: &[u8], ports: &[&SendRight]) -> Result<(), SendError> {
        if body.len() > MAX_BODY || ports.len() > MAX_PORTS {
            return Err(SendError::Other(format!("a Mach message of {} bytes and {} ports is over the limit ({MAX_BODY} bytes, {MAX_PORTS} ports)", body.len(), ports.len())));
        }
        let names: Vec<u32> = ports.iter().map(|p| p.0).collect(); // alloc: at most MAX_PORTS names, once per message
        let mut buf = codec::encode(self.0, &names, body);
        // The size mach_msg takes is in bytes; the buffer is words.
        let size = std::mem::size_of_val(buf.as_slice()) as u32;
        // SAFETY: buf is a whole message codec::encode laid out (header, body count, descriptors, bytes), 4-aligned.
        let kr = unsafe { mach_msg(buf.as_mut_ptr().cast(), MACH_SEND_MSG | MACH_SEND_TIMEOUT, size, 0, MACH_PORT_NULL, 0, MACH_PORT_NULL) };
        match kr {
            KERN_SUCCESS => Ok(()),
            MACH_SEND_TIMED_OUT => Err(SendError::Full),
            MACH_SEND_INVALID_DEST => Err(SendError::Dead),
            kr => Err(SendError::Other(format!("mach_msg(send) to port {}: {}", self.0, kr_text(kr)))),
        }
    }

    /// Take ownership of a send right named `raw` (one reference).
    ///
    /// # Safety
    /// `raw` names a send right of this task whose reference the caller gives up.
    #[must_use]
    pub unsafe fn from_raw(raw: u32) -> Self {
        Self(raw)
    }

    #[must_use]
    pub fn raw(&self) -> u32 {
        self.0
    }
}

impl Drop for SendRight {
    fn drop(&mut self) {
        if self.0 != MACH_PORT_NULL {
            // SAFETY: we own one user reference to this send right (or dead name).
            unsafe { mach_port_deallocate(task(), self.0) };
        }
    }
}

// ---------------------------------------------------------------- a receive port on the main queue

#[allow(non_upper_case_globals)]
unsafe extern "C" {
    static _dispatch_source_type_mach_recv: c_void;
    static _dispatch_main_q: c_void;
    fn dispatch_source_create(kind: *const c_void, handle: usize, mask: usize, queue: *const c_void) -> *mut c_void;
    fn dispatch_set_context(object: *mut c_void, context: *mut c_void);
    fn dispatch_source_set_event_handler_f(source: *mut c_void, handler: extern "C" fn(*mut c_void));
    fn dispatch_source_set_cancel_handler_f(source: *mut c_void, handler: extern "C" fn(*mut c_void));
    fn dispatch_resume(object: *mut c_void);
    fn dispatch_source_cancel(source: *mut c_void);
    fn dispatch_release(object: *mut c_void);
}

/// What a [`Source`] hands each message to, or the error that ended its port.
pub type Handler = Box<dyn FnMut(Result<Msg, Error>)>; // alloc: one per source, for its life

struct Ctx {
    port: RecvPort,
    buf: codec::RecvBuf,
    handler: Handler,
}

extern "C" fn on_event(ctx: *mut c_void) {
    // SAFETY: ctx is the Box<Ctx> `Source::on_main_queue` leaked; dispatch calls this on the main queue only, never
    // after the cancel handler has run, and never twice at once.
    let ctx = unsafe { &mut *ctx.cast::<Ctx>() };
    for _ in 0..DRAIN_MAX {
        match ctx.port.recv_with(&mut ctx.buf, 0) {
            Ok(Some(m)) => (ctx.handler)(Ok(m)),
            Ok(None) => return,
            Err(e) => return (ctx.handler)(Err(e)),
        }
    }
}

extern "C" fn on_cancel(ctx: *mut c_void) {
    // SAFETY: the Box<Ctx> leaked in on_main_queue; dispatch runs the cancel handler once, after the last event.
    drop(unsafe { Box::from_raw(ctx.cast::<Ctx>()) });
}

/// A receive port drained on the main dispatch queue: `handler` gets each message there. Dropping it cancels the
/// source, and the port is destroyed once dispatch has run its last callback.
pub struct Source(*mut c_void);

impl Source {
    /// Drain `port` on the main queue. The handler runs on the main thread, so it may touch AppKit, Metal objects made
    /// there, and CEF's UI-thread objects.
    pub fn on_main_queue(port: RecvPort, handler: Handler) -> Result<Self, Error> {
        let raw = port.raw();
        // SAFETY: dispatch's mach-receive source type and main queue are libdispatch statics; `raw` is our receive right.
        let src = unsafe { dispatch_source_create(&raw const _dispatch_source_type_mach_recv, raw as usize, 0, &raw const _dispatch_main_q) };
        if src.is_null() {
            return Err(Error(format!("dispatch_source_create(mach recv) for port {raw} returned NULL")));
        }
        let ctx = Box::into_raw(Box::new(Ctx { port, buf: codec::RecvBuf::new(), handler })); // alloc: the source's context, freed in on_cancel
        // SAFETY: src is a fresh, suspended source; ctx lives until on_cancel frees it.
        unsafe {
            dispatch_set_context(src, ctx.cast());
            dispatch_source_set_event_handler_f(src, on_event);
            dispatch_source_set_cancel_handler_f(src, on_cancel);
            dispatch_resume(src);
        }
        Ok(Self(src))
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        // SAFETY: our one reference to a resumed source; cancel then release (the context is freed by on_cancel).
        unsafe {
            dispatch_source_cancel(self.0);
            dispatch_release(self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every private or undocumented symbol this crate calls still resolves (rule macos-private-apis).
    #[test]
    fn symbols_resolve() {
        unsafe extern "C" {
            fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
        }
        const RTLD_DEFAULT: *mut c_void = -2isize as *mut c_void;
        for sym in ["bootstrap_check_in", "bootstrap_look_up", "_dispatch_source_type_mach_recv", "IOSurfaceCreateMachPort", "IOSurfaceLookupFromMachPort"] {
            let c = CString::new(sym).unwrap();
            // SAFETY: dlsym with RTLD_DEFAULT and a NUL-terminated name.
            let p = unsafe { dlsym(RTLD_DEFAULT, c.as_ptr()) };
            assert!(!p.is_null(), "harness-mach: the symbol {sym} no longer resolves on this macOS");
        }
    }

    #[test]
    fn a_message_crosses_a_port_with_a_right() {
        let a = RecvPort::new().unwrap();
        let b = RecvPort::new().unwrap();
        let to_a = a.make_send().unwrap();
        let to_b = b.make_send().unwrap();
        to_a.send(b"hello", &[&to_b]).unwrap();
        let m = a.recv(1000).unwrap().expect("the message");
        assert_eq!(m.body, b"hello");
        assert_eq!(m.ports.len(), 1);
        m.ports[0].send(b"back", &[]).unwrap();
        assert_eq!(b.recv(1000).unwrap().unwrap().body, b"back");
        assert!(a.recv(0).unwrap().is_none());
    }

    #[test]
    fn a_dead_receiver_is_dead() {
        let a = RecvPort::new().unwrap();
        let to_a = a.make_send().unwrap();
        drop(a);
        assert_eq!(to_a.send(b"x", &[]), Err(SendError::Dead));
    }
}
