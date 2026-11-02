//! How a run process gets its memory entries: a Unix socket can't carry Mach ports, and the
//! daemon can't map into a child (`task_for_pid` needs an entitlement, and a task port a child
//! sends is immovable), so the start hook asks the daemon's rendezvous service for them, as
//! Chromium's `MachPortRendezvousServer` hands ports to its children.
//!
//! The daemon checks in `dev.blaze.inproc.<its pid>` with its bootstrap server once and serves
//! it on a thread of its own. To grant, it makes the entries, files them under the run
//! process's audit token (the channel's `LOCAL_PEERTOKEN`, checked against the process ID it
//! launched), and sends the GRANT frame. The start hook names the daemon by the same option on its
//! end, looks the service up, and sends a request with a send-once reply port; the service
//! takes the entries filed under the request's audit token (the kernel's trailer, which no
//! sender can forge) and moves them into the reply, which the start hook takes only with the
//! daemon's audit token in its trailer. So the ports go only to the process on the channel,
//! and only from the daemon it connected to.
//!
//! A sandboxed run needs `(allow mach-lookup (global-name "dev.blaze.inproc.<pid>"))` in its
//! profile; looking the service up is all it does. `Host::launch` names it in the command's
//! `Sandbox::On::services` ([`super::service`]), which blaze-core's profile allows.
//!
//! The messages are encoded and decoded here as bytes ([`Message`]), by safe functions the
//! tests run over any bytes.

use std::ffi::{CStr, CString, c_char, c_int};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use blaze_bytes::le::{get_u32 as get, get_u64 as get64};
use crossbeam_channel::RecvTimeoutError;
use parking_lot::Mutex;

use super::{
    KERN_SUCCESS, Kern, MACH_PORT_NULL, Object, Port, VM_PROT_EXECUTE, VM_PROT_READ, VM_PROT_WRITE, kern,
    mach_port_deallocate, mach_port_mod_refs, task,
};
use crate::channel::Channel;
use crate::frame::kinds;
use crate::sys::Rights;
use crate::{Access, Error};

const MACH_SEND_MSG: c_int = 0x1;
const MACH_RCV_MSG: c_int = 0x2;
const MACH_SEND_TIMEOUT: c_int = 0x10;
const MACH_RCV_TIMEOUT: c_int = 0x100;
/// `MACH_RCV_TRAILER_TYPE(MACH_MSG_TRAILER_FORMAT_0) |
/// MACH_RCV_TRAILER_ELEMENTS(MACH_RCV_TRAILER_AUDIT)`: the trailer through the sender's audit
/// token.
const MACH_RCV_TRAILER_AUDIT: c_int = 3 << 24;
const MACH_RCV_TOO_LARGE: Kern = 0x1000_4004;
const MACH_RCV_INTERRUPTED: Kern = 0x1000_4005;

const MACH_MSGH_BITS_COMPLEX: u32 = 0x8000_0000;
const MACH_MSGH_BITS_REMOTE_MASK: u32 = 0x1f;
const MACH_MSG_TYPE_MOVE_SEND: u32 = 17;
const MACH_MSG_TYPE_MOVE_SEND_ONCE: u32 = 18;
const MACH_MSG_TYPE_COPY_SEND: u32 = 19;
const MACH_MSG_TYPE_MAKE_SEND_ONCE: u32 = 21;
/// A received send right is `MACH_MSG_TYPE_PORT_SEND`, the value of `MOVE_SEND`.
const MACH_MSG_TYPE_PORT_SEND: u32 = MACH_MSG_TYPE_MOVE_SEND;
const MACH_MSG_TYPE_PORT_SEND_ONCE: u32 = MACH_MSG_TYPE_MOVE_SEND_ONCE;
const MACH_MSG_PORT_DESCRIPTOR: u32 = 0;
const MACH_PORT_DEAD: Port = u32::MAX;
const MACH_PORT_RIGHT_RECEIVE: u32 = 1;

/// `mach_msg_header_t`, `mach_msg_body_t` and `mach_msg_port_descriptor_t` (`#pragma pack(4)`).
const HEADER: usize = 24;
const BODY: usize = 4;
const DESCRIPTOR: usize = 12;
/// `mach_msg_audit_trailer_t`: type, size, sequence number, the security token's two words,
/// then the audit token's eight.
const AUDIT_TRAILER: usize = 52;
const AUDIT_AT: usize = 20;

/// The request: the header, then this protocol's version and how many objects the start hook was
/// told it is granted (the GRANT frame's count).
const REQUEST: usize = HEADER + 8;
const REQUEST_ID: i32 = 0x626c_7a00;
const REPLY_ID: i32 = REQUEST_ID + 100;
const VERSION: u32 = 1;

/// The most objects one grant carries.
const MOST_OBJECTS: usize = 32;
/// Room for a reply of the most objects and the largest trailer (68 bytes).
const MESSAGE: usize = 1024;

/// How long either side waits for the other.
const HANDOFF: Duration = Duration::from_secs(30);
/// How often the daemon looks at the channel while it waits.
const LOOK: Duration = Duration::from_millis(50);

pub(super) type Token = [u32; 8];

unsafe extern "C" {
    static bootstrap_port: Port;
    fn bootstrap_check_in(bp: Port, name: *const c_char, port: *mut Port) -> Kern;
    fn bootstrap_look_up(bp: Port, name: *const c_char, port: *mut Port) -> Kern;
    fn bootstrap_strerror(code: Kern) -> *const c_char;
    fn mach_msg(message: *mut u8, option: c_int, send: u32, receive: u32, name: Port, timeout: u32, notify: Port) -> Kern;
    fn mach_msg_destroy(message: *mut u8);
    fn mach_port_allocate(task: Port, right: u32, name: *mut Port) -> Kern;
}

fn bootstrap_error(what: &str, code: Kern) -> Error {
    // SAFETY: bootstrap_strerror returns a static NUL-terminated string for any code.
    let reason = unsafe { bootstrap_strerror(code) };
    // SAFETY: bootstrap_strerror returns a static NUL-terminated string for any code.
    let reason = unsafe { CStr::from_ptr(reason) };
    Error::new(format!("{what}: {code} ({})", reason.to_string_lossy()))
}

/// What a run process's entry carries for `rights`.
pub(super) fn protection(rights: Rights) -> c_int {
    match rights {
        Rights::Code => VM_PROT_READ | VM_PROT_EXECUTE,
        Rights::Words => VM_PROT_READ | VM_PROT_WRITE,
    }
}

/// The service of the daemon of process `pid`.
pub(super) fn service_name(pid: u32) -> CString {
    CString::new(format!("dev.blaze.inproc.{pid}")).unwrap_or_default()
}

/// A Mach message's bytes, aligned as `mach_msg` wants.
#[repr(C, align(8))]
pub(super) struct Message(pub(super) [u8; MESSAGE]);

impl Message {
    pub(super) fn new() -> Message {
        Message([0; MESSAGE])
    }

    fn as_mut_ptr(&mut self) -> *mut u8 {
        self.0.as_mut_ptr()
    }

    /// Releases the rights a message received into this holds.
    fn destroy(&mut self) {
        // SAFETY: the buffer holds a message `mach_msg` received (or failed to send and gave
        // back), whose rights this process owns and nothing else will release.
        unsafe { mach_msg_destroy(self.as_mut_ptr()) };
    }
}

fn put(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

fn bits(remote: u32, local: u32) -> u32 {
    remote | (local << 8)
}

/// The request to the service at `server`, the reply to come to a send-once right made from
/// the receive right `reply`. Its size.
pub(super) fn encode_request(out: &mut Message, server: Port, reply: Port, count: u32) -> u32 {
    let bytes = &mut out.0;
    bytes[..REQUEST].fill(0);
    put(bytes, 0, bits(MACH_MSG_TYPE_COPY_SEND, MACH_MSG_TYPE_MAKE_SEND_ONCE));
    put(bytes, 4, REQUEST as u32);
    put(bytes, 8, server);
    put(bytes, 12, reply);
    put(bytes, 20, REQUEST_ID as u32);
    put(bytes, HEADER, VERSION);
    put(bytes, HEADER + 4, count);
    REQUEST as u32
}

/// The audit token of the trailer after a received message of `size` bytes.
fn trailer(bytes: &[u8], size: usize) -> Option<Token> {
    let at = size.checked_next_multiple_of(4)?;
    if get(bytes, at)? != 0 || (get(bytes, at + 4)? as usize) < AUDIT_TRAILER {
        return None;
    }
    let mut token = [0u32; 8];
    for (index, word) in token.iter_mut().enumerate() {
        *word = get(bytes, at + AUDIT_AT + index * 4)?;
    }
    Some(token)
}

/// A request as the service received it.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Request {
    pub(super) reply: Port,
    pub(super) count: u32,
    pub(super) token: Token,
}

pub(super) fn decode_request(bytes: &[u8]) -> Option<Request> {
    let header = get(bytes, 0)?;
    if header & MACH_MSGH_BITS_COMPLEX != 0
        || header & MACH_MSGH_BITS_REMOTE_MASK != MACH_MSG_TYPE_PORT_SEND_ONCE
        || get(bytes, 4)? as usize != REQUEST
        || get(bytes, 20)? != REQUEST_ID as u32
        || get(bytes, HEADER)? != VERSION
    {
        return None;
    }
    let reply = get(bytes, 8)?;
    if reply == MACH_PORT_NULL || reply == MACH_PORT_DEAD {
        return None;
    }
    Some(Request { reply, count: get(bytes, HEADER + 4)?, token: trailer(bytes, REQUEST)? })
}

fn reply_size(count: usize) -> usize {
    HEADER + BODY + count * (DESCRIPTOR + 8)
}

/// The reply moving `objects` (send rights and their lengths) to the send-once right `to`.
/// Its size.
pub(super) fn encode_reply(out: &mut Message, to: Port, objects: &[(Port, u64)]) -> u32 {
    let size = reply_size(objects.len());
    let bytes = &mut out.0;
    bytes[..size].fill(0);
    put(bytes, 0, MACH_MSG_TYPE_MOVE_SEND_ONCE | MACH_MSGH_BITS_COMPLEX);
    put(bytes, 4, size as u32);
    put(bytes, 8, to);
    put(bytes, 20, REPLY_ID as u32);
    put(bytes, HEADER, objects.len() as u32);
    let lens = HEADER + BODY + objects.len() * DESCRIPTOR;
    for (index, &(port, len)) in objects.iter().enumerate() {
        let at = HEADER + BODY + index * DESCRIPTOR;
        put(bytes, at, port);
        put(bytes, at + 8, (MACH_MSG_TYPE_MOVE_SEND << 16) | (MACH_MSG_PORT_DESCRIPTOR << 24));
        bytes[lens + index * 8..lens + index * 8 + 8].copy_from_slice(&len.to_le_bytes());
    }
    size as u32
}

/// The objects of a reply as the start hook received it, and its sender's audit token.
pub(super) fn decode_reply(bytes: &[u8], count: usize) -> Result<(Vec<(Port, u64)>, Token), String> {
    let size = reply_size(count);
    let header = get(bytes, 0).ok_or("a reply shorter than its header")?;
    if header & MACH_MSGH_BITS_COMPLEX == 0 || get(bytes, 20) != Some(REPLY_ID as u32) {
        return Err(format!("a reply of bits {header:#x} and ID {:?}, not the service's", get(bytes, 20)));
    }
    if get(bytes, 4).map(|it| it as usize) != Some(size) || get(bytes, HEADER).map(|it| it as usize) != Some(count) {
        return Err(format!(
            "a reply of {:?} bytes and {:?} descriptors, not {size} and {count}",
            get(bytes, 4),
            get(bytes, HEADER)
        ));
    }
    let lens = HEADER + BODY + count * DESCRIPTOR;
    let mut out = Vec::with_capacity(count);
    for index in 0..count {
        let at = HEADER + BODY + index * DESCRIPTOR;
        let port = get(bytes, at).ok_or("a short reply")?;
        let word = get(bytes, at + 8).ok_or("a short reply")?;
        let (disposition, kind) = ((word >> 16) & 0xff, word >> 24);
        if kind != MACH_MSG_PORT_DESCRIPTOR || disposition != MACH_MSG_TYPE_PORT_SEND {
            return Err(format!("descriptor {index} of the reply is of type {kind} and disposition {disposition}, not a send right"));
        }
        if port == MACH_PORT_NULL || port == MACH_PORT_DEAD {
            return Err(format!("descriptor {index} of the reply names no port"));
        }
        let len = get64(bytes, lens + index * 8).ok_or("a short reply")?;
        out.push((port, len));
    }
    let token = trailer(bytes, size).ok_or("a reply with no audit trailer")?;
    Ok((out, token))
}

/// The audit token of the process at the other end of the Unix socket `fd`, as it was when it
/// connected.
pub(super) fn peer_token(fd: i64) -> Result<Token, Error> {
    let fd = c_int::try_from(fd).map_err(|_| Error::new(format!("a channel fd of {fd}")))?;
    let mut token: Token = [0; 8];
    let mut len = size_of::<Token>() as libc::socklen_t;
    // SAFETY: `token` is 32 writable bytes and `len` says so; a bad `fd` fails with an error.
    let got =
        unsafe { libc::getsockopt(fd, libc::SOL_LOCAL, libc::LOCAL_PEERTOKEN, (&raw mut token).cast(), &raw mut len) };
    if got != 0 || len as usize != size_of::<Token>() {
        return Err(Error::new(format!("LOCAL_PEERTOKEN of the channel: {}", std::io::Error::last_os_error())));
    }
    Ok(token)
}

/// The process ID in an audit token (`audit_token_to_pid`).
pub(super) fn token_pid(token: &Token) -> u32 {
    token[5]
}

/// Send rights this process holds, released when dropped unless taken out.
struct Ports(Vec<Port>);

impl Drop for Ports {
    fn drop(&mut self) {
        for &port in &self.0 {
            // SAFETY: a send right this `Ports` holds one reference of.
            unsafe { mach_port_deallocate(task(), port) };
        }
    }
}

/// Objects filed for a run process, until its start hook asks.
struct Pending {
    token: Token,
    ports: Ports,
    lens: Vec<u64>,
    done: crossbeam_channel::Sender<Result<(), Error>>,
}

/// What is filed for each run process being started. Locked only to file or take an entry.
static PENDING: Mutex<Vec<Pending>> = Mutex::new(Vec::new());

fn take(token: &Token) -> Option<Pending> {
    let mut pending = PENDING.lock();
    let index = pending.iter().position(|it| it.token == *token)?;
    Some(pending.swap_remove(index))
}

/// The daemon's service, checked in and served from the first grant on.
fn service() -> Result<(), Error> {
    static STARTED: OnceLock<Result<(), Error>> = OnceLock::new();
    STARTED
        .get_or_init(|| {
            let name = service_name(std::process::id());
            let mut port = MACH_PORT_NULL;
            // SAFETY: `bootstrap_port` is this process's bootstrap send right, set before
            // `main` and never changed; `name` is NUL-terminated; `port` is a live out-pointer.
            let code = unsafe { bootstrap_check_in(bootstrap_port, name.as_ptr(), &raw mut port) };
            if code != KERN_SUCCESS {
                return Err(bootstrap_error(&format!("bootstrap_check_in of {}", name.to_string_lossy()), code));
            }
            std::thread::Builder::new()
                .name("blaze-inproc-rendezvous".to_owned())
                .spawn(move || serve(port))
                .map(drop)
                .map_err(|it| Error::new(format!("starting the rendezvous thread: {it}")))
        })
        .clone()
}

/// The service's loop over the receive right `port`, for the life of the process.
fn serve(port: Port) {
    let mut message = Message::new();
    loop {
        // SAFETY: receives into `message`, `MESSAGE` writable bytes this thread owns, from the
        // receive right `port` this thread holds for the life of the process.
        let code = unsafe {
            mach_msg(message.as_mut_ptr(), MACH_RCV_MSG | MACH_RCV_TRAILER_AUDIT, 0, MESSAGE as u32, port, 0, MACH_PORT_NULL)
        };
        match code {
            KERN_SUCCESS => {}
            MACH_RCV_TOO_LARGE | MACH_RCV_INTERRUPTED => continue,
            _ => {
                eprintln!("{}", kern("blaze-inproc's rendezvous service stopped: mach_msg", code));
                return;
            }
        }
        let Some(request) = decode_request(&message.0) else {
            message.destroy();
            continue;
        };
        let Some(mut pending) = take(&request.token) else {
            message.destroy();
            continue;
        };
        if request.count as usize != pending.lens.len() {
            message.destroy();
            let _ = pending.done.send(Err(Error::new(format!(
                "the run process asked for {} objects, not the {} it was granted",
                request.count,
                pending.lens.len()
            ))));
            continue;
        }
        let objects: Vec<(Port, u64)> = pending.ports.0.iter().copied().zip(pending.lens.iter().copied()).collect();
        let mut reply = Message::new();
        let size = encode_reply(&mut reply, request.reply, &objects);
        // The rights go into the reply: sent, they are the start hook's; not sent, the kernel
        // gives them back in it, and `destroy` releases them.
        pending.ports.0.clear();
        // SAFETY: sends `size` bytes of `reply`, which moves the send-once right of the request
        // and the send rights `objects`, all this process's.
        let sent = unsafe {
            mach_msg(
                reply.as_mut_ptr(),
                MACH_SEND_MSG | MACH_SEND_TIMEOUT,
                size,
                0,
                MACH_PORT_NULL,
                HANDOFF.as_millis() as u32,
                MACH_PORT_NULL,
            )
        };
        let result = if sent == KERN_SUCCESS {
            Ok(())
        } else {
            reply.destroy();
            Err(kern("sending a run process its objects: mach_msg", sent))
        };
        let _ = pending.done.send(result);
    }
}

/// Whether the process at the other end of `fd` wrote or hung up.
fn spoke(fd: i64) -> bool {
    let Ok(fd) = c_int::try_from(fd) else { return true };
    let mut poll = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
    // SAFETY: one live `pollfd`; a zero timeout returns at once.
    let got = unsafe { libc::poll(&raw mut poll, 1, 0) };
    got != 0
}

/// The daemon's side of a run process: its process ID, since the start hook maps what it is
/// given. macOS has no descriptor for a process, so a kill names it by ID: one sent after the
/// process was reaped and its ID reused would reach another process, and its launcher must
/// not reap it while it may still be killed.
#[derive(Debug)]
pub(crate) struct Remote {
    pid: u32,
}

impl Remote {
    pub(crate) fn kill(&self) -> Result<(), Error> {
        let Ok(pid) = libc::pid_t::try_from(self.pid) else {
            return Err(Error::new(format!("process ID {} past pid_t", self.pid)));
        };
        // SAFETY: kill takes a process ID and a signal and touches no memory of ours.
        if unsafe { libc::kill(pid, libc::SIGKILL) } != 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) {
            return Err(Error::new(format!("kill of process {pid}: {}", std::io::Error::last_os_error())));
        }
        Ok(())
    }

    pub(crate) fn map(&self, object: u32, _: Access) -> Result<u64, Error> {
        Err(Error::new(format!("a start hook asked the daemon to map object {object}: on macOS the hook maps it")))
    }

    pub(crate) fn overlay(&self, _: u64, _: &crate::handshake::Grant) -> Result<(), Error> {
        Err(Error::new("a start hook asked the daemon to lay its overlays: on macOS the hook lays them"))
    }
}

/// Files the objects for the run process on `channel`, sends the GRANT frame, and waits until
/// the service gave them to its start hook. A hook that writes to the channel before it asked
/// (its FAILED frame) is left for the host to read.
pub(crate) fn send_objects(channel: &mut Channel, pid: u32, grant: &[u8], objects: &[(&Object, Rights)]) -> Result<Remote, Error> {
    if objects.len() > MOST_OBJECTS {
        return Err(Error::new(format!("{} objects to grant, past the {MOST_OBJECTS} one reply carries", objects.len())));
    }
    service()?;
    let token = peer_token(channel.raw())?;
    if token_pid(&token) != pid {
        return Err(Error::new(format!(
            "the channel's peer is process {}, not the run process {pid}",
            token_pid(&token)
        )));
    }
    let mut ports = Ports(Vec::with_capacity(objects.len()));
    let mut lens = Vec::with_capacity(objects.len());
    for &(object, rights) in objects {
        ports.0.push(object.entry(protection(rights))?);
        lens.push(object.len());
    }
    let (done, finished) = crossbeam_channel::unbounded();
    {
        let mut pending = PENDING.lock();
        pending.retain(|it| it.token != token);
        pending.push(Pending { token, ports, lens, done });
    }
    if let Err(error) = channel.send(kinds::GRANT, 0, grant) {
        drop(take(&token));
        return Err(error);
    }
    let deadline = Instant::now() + HANDOFF;
    loop {
        match finished.recv_timeout(LOOK) {
            Ok(result) => return result.map(|()| Remote { pid }),
            Err(RecvTimeoutError::Disconnected) => return Err(Error::new("the rendezvous service dropped a grant")),
            Err(RecvTimeoutError::Timeout) => {}
        }
        let late = Instant::now() >= deadline;
        if !late && !spoke(channel.raw()) {
            continue;
        }
        if take(&token).is_some() {
            if late {
                return Err(Error::new(format!(
                    "run process {pid} did not ask for its objects in {HANDOFF:?}: its sandbox must allow mach-lookup \
                     of {}",
                    service_name(std::process::id()).to_string_lossy()
                )));
            }
            return Ok(Remote { pid });
        }
        return match finished.recv_timeout(HANDOFF) {
            Ok(result) => result.map(|()| Remote { pid }),
            Err(_) => Err(Error::new("the rendezvous service took a grant and never said how it went")),
        };
    }
}

/// A receive right this process holds, destroyed when dropped.
struct Receive(Port);

impl Drop for Receive {
    fn drop(&mut self) {
        // SAFETY: the receive right this `Receive` holds; dropping its one reference destroys
        // the port.
        unsafe { mach_port_mod_refs(task(), self.0, MACH_PORT_RIGHT_RECEIVE, -1) };
    }
}

pub(crate) fn recv_objects(channel: &mut Channel, count: usize) -> Result<Vec<super::Granted>, Error> {
    if count > MOST_OBJECTS {
        return Err(Error::new(format!("a grant of {count} objects, past the {MOST_OBJECTS} one reply carries")));
    }
    let daemon = peer_token(channel.raw())?;
    let name = service_name(token_pid(&daemon));
    let mut server = MACH_PORT_NULL;
    // SAFETY: as in `service`; `server` is a live out-pointer.
    let code = unsafe { bootstrap_look_up(bootstrap_port, name.as_ptr(), &raw mut server) };
    if code != KERN_SUCCESS {
        return Err(bootstrap_error(
            &format!(
                "bootstrap_look_up of the daemon's {} (a sandbox must allow mach-lookup of it)",
                name.to_string_lossy()
            ),
            code,
        ));
    }
    let server = Ports(vec![server]);
    let mut reply = MACH_PORT_NULL;
    // SAFETY: `reply` is a live out-pointer for the new receive right's name.
    let code = unsafe { mach_port_allocate(task(), MACH_PORT_RIGHT_RECEIVE, &raw mut reply) };
    if code != KERN_SUCCESS {
        return Err(kern("mach_port_allocate of a reply port", code));
    }
    let reply = Receive(reply);
    let mut message = Message::new();
    let size = encode_request(&mut message, server.0[0], reply.0, count as u32);
    // SAFETY: sends `size` bytes of `message`, naming this process's send right to the service
    // (copied) and its receive right `reply` (a send-once right made from it), then receives
    // the reply on `reply` into the same `MESSAGE` bytes.
    let code = unsafe {
        mach_msg(
            message.as_mut_ptr(),
            MACH_SEND_MSG | MACH_RCV_MSG | MACH_SEND_TIMEOUT | MACH_RCV_TIMEOUT | MACH_RCV_TRAILER_AUDIT,
            size,
            MESSAGE as u32,
            reply.0,
            HANDOFF.as_millis() as u32,
            MACH_PORT_NULL,
        )
    };
    if code != KERN_SUCCESS {
        return Err(kern(&format!("asking {} for the run's objects: mach_msg", name.to_string_lossy()), code));
    }
    match decode_reply(&message.0, count) {
        Ok((objects, token)) if token == daemon => {
            Ok(objects.into_iter().map(|(port, len)| super::Granted { port, len }).collect())
        }
        Ok((_, token)) => {
            message.destroy();
            Err(Error::new(format!(
                "objects from process {}, not the daemon {} on the channel",
                token_pid(&token),
                token_pid(&daemon)
            )))
        }
        Err(why) => {
            message.destroy();
            Err(Error::new(format!("the rendezvous reply: {why}")))
        }
    }
}
