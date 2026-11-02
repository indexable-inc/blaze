//! The run process's side, linked into the program: the start hook that attaches it to the
//! daemon before `main` (the crate docs' "The start of a run"), its quiescent points, its
//! threads' fence slots, and its asks to the daemon ([`crate::ask`]).
//!
//! The link puts a pointer to [`blaze_inproc_start`] first among the executable's initializers:
//! the ELF `.preinit_array`, the Mach-O `__mod_init_func`, a PE TLS callback in `.CRT$XLAB`. The
//! daemon's own executable never has one. A program whose link has none, or that was started
//! with no `BLAZE_INPROC` (run by hand), runs its file as it is: [`attach`] says why, and
//! [`quiesce`] does nothing.

use std::cell::Cell;
use std::sync::OnceLock;
use std::sync::atomic::AtomicU64;

use crate::channel::Channel;
use crate::fence::{MOST_FRAMES, Thread};
use crate::frame::{Kind, Reader, Writer, kinds};
use crate::handshake::{Grant, HOST_VAR, Hello, VERSION, host_var};
use crate::{Access, Error, Format, ask, bind, image, sys};

pub struct Attached {
    control: &'static [AtomicU64],
    asks: &'static [AtomicU64],
    /// Only rung and looked at once the start is over: nothing reads it here.
    stream: sys::Stream,
    slide: u64,
    pid: u32,
}

/// Set once, by the start hook, before any thread of the program runs.
static ATTACHED: OnceLock<Result<Attached, Error>> = OnceLock::new();

/// This process's side of the fence and channel, as the start hook left it.
pub fn attach() -> Result<&'static Attached, Error> {
    match ATTACHED.get() {
        Some(attached) => attached.as_ref().map_err(Clone::clone),
        None => Err(Error::new(
            "the start hook never ran: the executable's link has no pointer to blaze_inproc_start among its first \
             initializers",
        )),
    }
}

/// A quiescent point of the calling thread, when this process was started by the daemon.
pub fn quiesce() {
    if let Ok(attached) = attach() {
        attached.quiesce();
    }
}

/// The start hook, on Linux: `.preinit_array` entries get `argc`, `argv` and `envp`. libc's
/// `environ` is not set yet (libc's own initializer runs after), so the variable comes from
/// `envp` and leaves it in place: the pointers after it move down one, so libc, the program
/// and its children never see it.
///
/// # Safety
///
/// Called by the dynamic linker as a `.preinit_array` entry, once, before any other code of
/// the program, with the process's own `envp`.
#[cfg(target_os = "linux")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn blaze_inproc_start(
    _: std::ffi::c_int,
    _: *const *const std::ffi::c_char,
    envp: *mut *const std::ffi::c_char,
) {
    // SAFETY: `envp` is the process's environment, null-terminated, which nothing else reads
    // or writes yet (the caller's contract).
    let var = unsafe { take_from(envp) };
    start(var);
}

/// Takes `BLAZE_INPROC=...` out of the null-terminated `envp`, moving the entries after it
/// down: its value.
///
/// # Safety
///
/// `envp` is null or a null-terminated array of NUL-terminated strings, written by nothing
/// else meanwhile.
#[cfg(target_os = "linux")]
pub(crate) unsafe fn take_from(envp: *mut *const std::ffi::c_char) -> Option<Vec<u8>> {
    if envp.is_null() {
        return None;
    }
    let prefix = HOST_VAR.as_bytes();
    let mut index = 0;
    loop {
        // SAFETY: `index` is at or before the terminating null (the loop stops there).
        let entry = unsafe { *envp.add(index) };
        if entry.is_null() {
            return None;
        }
        // SAFETY: a NUL-terminated string of the environment.
        let bytes = unsafe { std::ffi::CStr::from_ptr(entry) }.to_bytes();
        if bytes.len() > prefix.len() && bytes.starts_with(prefix) && bytes[prefix.len()] == b'=' {
            let value = bytes[prefix.len() + 1..].to_vec();
            let mut at = index;
            loop {
                // SAFETY: `at + 1` is at or before the terminating null, and `at` before it:
                // both in the array.
                let next = unsafe { *envp.add(at + 1) };
                // SAFETY: as above; the array is writable, the process's own.
                unsafe { *envp.add(at) = next };
                if next.is_null() {
                    return Some(value);
                }
                at += 1;
            }
        }
        index += 1;
    }
}

/// The start hook, on macOS: the first `__mod_init_func` entry, after libSystem's initializers,
/// so the environment is set up.
#[cfg(target_os = "macos")]
#[unsafe(no_mangle)]
pub extern "C" fn blaze_inproc_start() {
    let var = std::env::var_os(HOST_VAR).map(std::os::unix::ffi::OsStringExt::into_vec);
    if var.is_some() {
        // SAFETY: the program's own code has not run, so none of its threads reads the
        // environment; dyld runs initializers on one thread.
        unsafe { std::env::remove_var(HOST_VAR) };
    }
    start(var);
}

/// The start hook, on Windows: a TLS callback, run before the CRT's start, on the process's
/// first thread at `DLL_PROCESS_ATTACH`.
///
/// # Safety
///
/// Called by the system loader as a TLS callback.
#[cfg(windows)]
#[unsafe(no_mangle)]
pub unsafe extern "system" fn blaze_inproc_start(_: *mut std::ffi::c_void, reason: u32, _: *mut std::ffi::c_void) {
    const DLL_PROCESS_ATTACH: u32 = 1;
    if reason != DLL_PROCESS_ATTACH {
        return;
    }
    let var = std::env::var(HOST_VAR).ok().map(String::into_bytes);
    if var.is_some() {
        // SAFETY: the program's own code has not run (TLS callbacks come before the CRT's
        // start), so none of its threads reads the environment.
        unsafe { std::env::remove_var(HOST_VAR) };
    }
    start(var);
}

/// Attaches, or stops the process: a run whose code memory can't be laid over its image must
/// not run the file's code in its place.
#[cfg_attr(not(any(target_os = "linux", target_os = "macos", windows)), allow(dead_code, reason = "no start hook here"))]
fn start(var: Option<Vec<u8>>) {
    if ATTACHED.get().is_some() {
        return;
    }
    let Some(var) = var else {
        let _ = ATTACHED.set(Err(Error::new(format!("{HOST_VAR} is not set: this program was not started by blaze"))));
        return;
    };
    let mut channel = None;
    match open(&var, &mut channel) {
        Ok(attached) => {
            let _ = ATTACHED.set(Ok(attached));
        }
        Err(error) => {
            if let Some(channel) = &mut channel {
                let mut body = Vec::new();
                Writer::new(&mut body).str(&error.to_string());
                let _ = channel.send(kinds::FAILED, 0, &body);
            }
            eprintln!("{error}");
            std::process::exit(127);
        }
    }
}

#[cfg_attr(not(any(target_os = "linux", target_os = "macos", windows)), allow(dead_code, reason = "no start hook here"))]
fn open(var: &[u8], out: &mut Option<Channel>) -> Result<Attached, Error> {
    let (address, token) = host_var(var)?;
    let format = Format::host().ok_or_else(|| Error::new("no executable format on this platform"))?;
    let page = sys::page();
    let (header, headers) = sys::own_image()?;
    let own = image::layout(format, headers, page)?;
    let channel = out.insert(Channel::new(sys::connect(address)?));

    let mut hello = Vec::with_capacity(40);
    Hello { token, pid: std::process::id(), version: VERSION, page, header }.encode(&mut hello);
    channel.send(kinds::HELLO, 0, &hello)?;
    let grant = {
        let frame = channel.recv()?;
        match frame.kind {
            kinds::GRANT => Grant::decode(frame.body)?,
            kinds::REFUSED => {
                let why = Reader::new(frame.body).str().unwrap_or("(not UTF-8)").to_owned();
                return Err(Error::new(format!("the daemon refused this run: {why}")));
            }
            other => return Err(Error::new(format!("the daemon answered hello with a frame of kind {:#06x}", other.0))),
        }
    };
    grant.check(&own, page)?;
    let slide = header.wrapping_sub(grant.header);
    bind::check_slide(&grant, slide)?;
    let objects = sys::recv_objects(channel, grant.objects.len())?;
    for (index, (object, (len, _))) in objects.iter().zip(&grant.objects).enumerate() {
        if object.len() != *len {
            return Err(Error::new(format!("object {index} is {:#x} bytes, where the grant says {len:#x}", object.len())));
        }
    }
    let control = &objects[1];
    let at = control.map_anywhere(channel, Access::Shared)?;
    let words = sys::adopt_words(at, control.len(), crate::fence::CONTROL_BYTES)?;
    if !Thread::ready(&words) {
        return Err(Error::new(format!("the control words at {at:#x} are not set up")));
    }
    let asks = &objects[2];
    let at = asks.map_anywhere(channel, Access::Shared)?;
    let asks = sys::adopt_words(at, asks.len(), ask::BYTES)?;
    if !ask::ready(&asks) {
        return Err(Error::new(format!("the asks at {at:#x} are not set up")));
    }
    sys::overlay(channel, &objects, &grant.overlays, slide)?;
    let mut scope = sys::Scope::new();
    bind::bind(
        &grant,
        slide,
        |name| scope.find(name),
        |at, value| {
            // SAFETY: `bind` stores only at a place plus `slide` that `Grant::decode` proved
            // is 8 bytes inside an overlay of `Access::Copy`, which `sys::overlay` just mapped
            // at its linked address plus `slide`, readable, writable and private to this
            // process (`MAP_PRIVATE`, an anonymous copy on macOS, a copy-on-write view on
            // Windows): never the memory object. No other thread of the program runs yet.
            unsafe { sys::store_private(at, value) }
        },
    )?;

    let mut started = Vec::with_capacity(8);
    Writer::new(&mut started).u64(sys::thread_id());
    channel.send(kinds::STARTED, 0, &started)?;
    let channel = out.take().ok_or_else(|| Error::new("the channel went away while starting"))?;
    Ok(Attached { control: words, asks, stream: channel.into_stream(), slide, pid: std::process::id() })
}

/// The calling thread's slot and the epoch it last reported, and its release when it ends.
struct Local {
    slot: Cell<Option<usize>>,
    reported: Cell<u64>,
    /// `sys::thread_id()`, read once (a system call on Linux); 0 before.
    tid: Cell<u64>,
}

impl Local {
    fn tid(&self) -> u64 {
        match self.tid.get() {
            0 => {
                let tid = sys::thread_id();
                self.tid.set(tid);
                tid
            }
            tid => tid,
        }
    }
}

impl Drop for Local {
    fn drop(&mut self) {
        let Some(index) = self.slot.get() else { return };
        let Some(Ok(attached)) = ATTACHED.get() else { return };
        let mut frames = [0u64; MOST_FRAMES];
        let walked = attached.walk(&mut frames);
        let epoch = Thread::epoch(&attached.control);
        let seen = Thread::seen(&attached.control, epoch, walked);
        if seen == epoch {
            Thread::release(&attached.control, index);
        } else {
            // A frame returns into retired code: the slot holds its fence until the daemon
            // finds the thread gone.
            Thread::report(&attached.control, index, seen);
        }
    }
}

thread_local! {
    static LOCAL: Local = const { Local { slot: Cell::new(None), reported: Cell::new(0), tid: Cell::new(0) } };
}

impl Attached {
    /// What this process adds to a linked address: where its image's header is minus where the
    /// link put it.
    pub fn slide(&self) -> u64 {
        self.slide
    }

    /// What the daemon last announced ([`crate::CodeMemory::announce`]): one load.
    pub fn announced(&self) -> u64 {
        Thread::announced(&self.control)
    }

    /// Sleeps until the daemon announces other than `seen`, or `timeout` passes: what is
    /// announced then. On Linux and macOS the daemon's announce wakes it (a futex on the
    /// shared word); on Windows a wait on an address stays within its process, so it wakes at
    /// `timeout` and the caller looks again.
    pub fn wait_announced(&self, seen: u64, timeout: std::time::Duration) -> u64 {
        Thread::wait_announced(&self.control, seen, timeout)
    }

    /// The calling thread's return addresses as linked addresses, the fence's.
    fn walk<'a>(&self, frames: &'a mut [u64; MOST_FRAMES]) -> Option<&'a [u64]> {
        let len = sys::backtrace(frames)?;
        for frame in &mut frames[..len] {
            *frame = frame.wrapping_sub(self.slide);
        }
        Some(&frames[..len])
    }

    /// A quiescent point of the calling thread: it is in no code it could leave and return
    /// to except what its stack shows. While the epoch has not changed since the thread last
    /// reported, one atomic load of the epoch and two of the thread's own slot.
    pub fn quiesce(&self) {
        let epoch = Thread::epoch(&self.control);
        let _ = LOCAL.try_with(|local| {
            let tid = local.tid();
            if let Some(index) = local.slot.get()
                && local.reported.get() == epoch
            {
                if Thread::holds(&self.control, index, self.pid, tid) {
                    return;
                }
                local.slot.set(None);
            }
            sys::serialize();
            let mut frames = [0u64; MOST_FRAMES];
            let walked = self.walk(&mut frames);
            let seen = Thread::seen(&self.control, epoch, walked);
            match local.slot.get() {
                Some(index) if Thread::holds(&self.control, index, self.pid, tid) => {
                    Thread::report(&self.control, index, seen);
                }
                _ => local.slot.set(Thread::claim(&self.control, self.pid, tid, seen)),
            }
            local.reported.set(if seen == epoch { epoch } else { 0 });
        });
    }

    /// Asks the daemon `kind` with `body` (at most [`ask::MOST_BODY`] bytes) and waits for its
    /// answer: the answer's kind and whole length, with as much of its body as fits in
    /// `answer` (the rest is lost: a longer answer than the caller can take is its error).
    ///
    /// From any thread, any number at once, and from a signal handler, also one that
    /// interrupted its own thread inside this call: it takes no lock, allocates nothing, and
    /// leaves `errno` as it found it. Errors are made only on failure, and allocate.
    pub fn request_into(&self, kind: Kind, body: &[u8], answer: &mut [u8]) -> Result<(Kind, usize), Error> {
        ask::request(&self.asks, &self.stream, kind, body, answer)
    }
}
