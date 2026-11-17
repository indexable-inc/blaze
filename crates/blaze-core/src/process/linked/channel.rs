//! A connected channel between a parent and the child it starts linked: a Unix stream socket
//! pair ([`pair`]), the child's end given as its descriptor [`CHILD_FD`] (through
//! [`super::RunLink::inherit`]) and the parent's end read and written by deadline. Messages
//! are length-prefixed: four bytes of the body's length, big-endian, then the body (the
//! launcher's run wire, `blaze_launch::wire::split`, frames the same way).
//!
//! - [`Channel::recv_message`] reads until the caller's inbox starts with one whole message,
//!   or the deadline passes (the partial bytes stay in the inbox for the next call), or the
//!   child's end is closed: at a message boundary that is [`Message::Closed`]; inside a message
//!   it is [`ChannelError::Torn`] (a child that died mid-message), never a short message.
//! - [`Channel::send`] writes bytes that are whole messages, all of them, by the deadline;
//!   bytes that do not split into whole messages are refused before any is written
//!   ([`ChannelError::NotWhole`]), and a peer gone is [`ChannelError::Hangup`], never a
//!   `SIGPIPE`: Linux sends with `MSG_NOSIGNAL`, macOS sets `SO_NOSIGPIPE` on the parent's end.
//! - [`DeadlineFd`] is the same read by deadline for a pipe ([`super::Output::Pipe`]).
//!
//! Waits go through [`crate::wait::readable`] (`poll`) with the deadline asked, named.

use std::{
    alloc::Allocator,
    io,
    os::fd::{AsFd, AsRawFd as _, BorrowedFd, FromRawFd as _, OwnedFd},
};

use crate::wait::{Clock, Deadline, Waited};

/// The descriptor the child's end is given as.
pub const CHILD_FD: u32 = 3;

/// The length prefix of a message: its body's length, big-endian.
pub const PREFIX: usize = 4;

/// The most bytes one read asks for.
const READ_MOST: usize = 64 << 10;

/// What a channel step was, for its error.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ChannelOp {
    /// `socketpair`.
    Pair,
    /// Setting close-on-exec, non-blocking or `SO_NOSIGPIPE` on an end.
    Flags,
    /// Waiting for an end to be readable or writable (`poll`).
    Wait,
    Read,
    Write,
}

/// Why a channel step failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ChannelError {
    /// The system refused `op` with this error number.
    Os { op: ChannelOp, os: i32 },
    /// The peer closed its end inside a message: `have` of its `want` bytes came.
    Torn { have: usize, want: usize },
    /// A message's length is past the `most` its reader takes.
    TooLong { len: u32, most: u32 },
    /// The peer's end is closed: what was being written cannot arrive.
    Hangup,
    /// The deadline passed with `sent` of `of` bytes written: the peer reads a torn stream from
    /// here on, so the caller ends the child.
    WriteDeadline { sent: usize, of: usize },
    /// Bytes given to [`Channel::send`] that are not whole messages: `at` is where the last
    /// whole one ends.
    NotWhole { at: usize, of: usize },
}

impl core::fmt::Display for ChannelError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match *self {
            ChannelError::Os { op, os } => write!(f, "the channel's {op:?}: {}", io::Error::from_raw_os_error(os)),
            ChannelError::Torn { have, want } => {
                write!(f, "the child closed its channel inside a message ({have} of {want} bytes)")
            }
            ChannelError::TooLong { len, most } => {
                write!(f, "a channel message of {len} bytes, past the {most} its reader takes")
            }
            ChannelError::Hangup => f.write_str("the child's end of the channel is closed"),
            ChannelError::WriteDeadline { sent, of } => {
                write!(f, "the deadline passed with {sent} of {of} bytes written to the child's channel")
            }
            ChannelError::NotWhole { at, of } => {
                write!(f, "{of} bytes to send are not whole channel messages (the last whole one ends at {at})")
            }
        }
    }
}

impl std::error::Error for ChannelError {}

/// How a read by deadline went.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Read {
    /// This many bytes came.
    Bytes(usize),
    /// The writer's end is closed and nothing is left.
    Closed,
    /// The deadline passed with nothing to read.
    Deadline,
}

/// How [`Channel::recv_message`] went.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Message {
    /// The inbox starts with a whole message of `len` bytes, its prefix counted (more bytes may
    /// follow it: a read takes what is there).
    Whole { len: usize },
    /// The deadline passed first; what came is in the inbox.
    Deadline,
    /// The child's end is closed at a message boundary, with nothing left.
    Closed,
}

/// The child's end, until the child is started with it: then dropped, so the parent holds the
/// one end and sees the child's close.
pub struct ChildEnd(OwnedFd);

impl AsFd for ChildEnd {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl ChildEnd {
    /// The end, borrowed, to give as `(end.fd(), CHILD_FD)` (a caller without `std`'s `AsFd`).
    pub fn fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

/// A descriptor read (and, for a socket, written) by deadline: non-blocking, close-on-exec.
pub struct DeadlineFd {
    fd: OwnedFd,
    what: &'static str,
}

/// The parent's end of a channel.
pub struct Channel {
    end: DeadlineFd,
}

/// A connected pair: the parent's [`Channel`] and the [`ChildEnd`] to give as [`CHILD_FD`].
pub fn pair() -> Result<(Channel, ChildEnd), ChannelError> {
    let mut ends = [0; 2];
    #[cfg(target_os = "linux")]
    let kind = libc::SOCK_STREAM | libc::SOCK_CLOEXEC;
    #[cfg(not(target_os = "linux"))]
    let kind = libc::SOCK_STREAM;
    // SAFETY: two descriptors written by the call into a live local array.
    if unsafe { libc::socketpair(libc::AF_UNIX, kind, 0, ends.as_mut_ptr()) } != 0 {
        return Err(os(ChannelOp::Pair));
    }
    // SAFETY: the call made both; each is owned once from here.
    let parent = unsafe { OwnedFd::from_raw_fd(ends[0]) };
    // SAFETY: as above.
    let child = unsafe { OwnedFd::from_raw_fd(ends[1]) };
    #[cfg(not(target_os = "linux"))]
    {
        crate::net::set_cloexec(parent.as_raw_fd()).map_err(|error| io_os(ChannelOp::Flags, &error))?;
        crate::net::set_cloexec(child.as_raw_fd()).map_err(|error| io_os(ChannelOp::Flags, &error))?;
    }
    #[cfg(target_os = "macos")]
    {
        let on: libc::c_int = 1;
        // SAFETY: the parent's end; an `int` read from a live local of its size.
        let set = unsafe {
            libc::setsockopt(
                parent.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_NOSIGPIPE,
                (&raw const on).cast(),
                size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        if set != 0 {
            return Err(os(ChannelOp::Flags));
        }
    }
    let end = DeadlineFd::new(parent, "the run child's channel").map_err(|error| io_os(ChannelOp::Flags, &error))?;
    Ok((Channel { end }, ChildEnd(child)))
}

impl DeadlineFd {
    /// `fd` made non-blocking (and close-on-exec), read by deadline from here on; `what` names
    /// its waits.
    pub fn new(fd: OwnedFd, what: &'static str) -> io::Result<DeadlineFd> {
        // SAFETY: no pointers; an open descriptor this owns.
        let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
        // SAFETY: as above.
        if flags < 0 || unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error());
        }
        crate::net::set_cloexec(fd.as_raw_fd())?;
        Ok(DeadlineFd { fd, what })
    }

    /// Up to `into.len()` bytes by `deadline` on `clock`.
    pub fn read_some(&self, into: &mut [u8], deadline: Deadline, clock: &impl Clock) -> Result<Read, ChannelError> {
        loop {
            // SAFETY: `into` is live and writable for its length; the descriptor is this one's.
            let got = unsafe { libc::read(self.fd.as_raw_fd(), into.as_mut_ptr().cast(), into.len()) };
            match got {
                0 => return Ok(Read::Closed),
                n if n > 0 => return Ok(Read::Bytes(n as usize)),
                _ => {}
            }
            match io::Error::last_os_error().raw_os_error() {
                Some(libc::EINTR) => continue,
                // A Unix socket whose peer closed with bytes it never read (Linux), or a reset:
                // the peer is gone, which is the end of what it sends.
                Some(libc::ECONNRESET) => return Ok(Read::Closed),
                Some(libc::EAGAIN) => {}
                Some(code) => return Err(ChannelError::Os { op: ChannelOp::Read, os: code }),
                None => return Err(ChannelError::Os { op: ChannelOp::Read, os: 0 }),
            }
            if deadline.passed(clock.now()) {
                return Ok(Read::Deadline);
            }
            if !self.ready(libc::POLLIN, deadline, clock)? {
                return Ok(Read::Deadline);
            }
        }
    }

    /// Whether the descriptor became ready for `events` (or hung up, or failed: the next call
    /// says which) by `deadline`.
    fn ready(&self, events: libc::c_short, deadline: Deadline, clock: &impl Clock) -> Result<bool, ChannelError> {
        let mut poll = [libc::pollfd { fd: self.fd.as_raw_fd(), events, revents: 0 }];
        // wakes: the kernel, when the peer writes, reads or closes, or the deadline.
        match crate::wait::readable(self.what, &mut poll, deadline.finite(), clock) {
            Ok(Waited::Ready(_)) => Ok(true),
            Ok(Waited::Deadline) => Ok(false),
            Err(error) => Err(ChannelError::Os {
                op: ChannelOp::Wait,
                os: error.io_error().and_then(io::Error::raw_os_error).unwrap_or(0),
            }),
        }
    }
}

impl Channel {
    /// The parent's end, for a wait on several descriptors at once.
    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.end.fd.as_fd()
    }

    /// Reads into the end of `into` until `into` starts with a whole message, by `deadline` on
    /// `clock`. `into` holds what an earlier call left (the start of a message) or nothing; the
    /// caller takes whole messages off its front. A message whose body is longer than `most` is
    /// refused before its body is read.
    pub fn recv_message<A: Allocator>(
        &mut self,
        into: &mut Vec<u8, A>,
        most: u32,
        deadline: Deadline,
        clock: &impl Clock,
    ) -> Result<Message, ChannelError> {
        loop {
            let want = match into.first_chunk::<PREFIX>() {
                Some(prefix) => {
                    let len = u32::from_be_bytes(*prefix);
                    if len > most {
                        return Err(ChannelError::TooLong { len, most });
                    }
                    PREFIX + len as usize
                }
                None => PREFIX,
            };
            if into.len() >= want && into.len() >= PREFIX {
                return Ok(Message::Whole { len: want });
            }
            // What the message still lacks, at least: a read takes what is there, up to
            // READ_MOST, so the next messages come in the same call.
            let ask = (want - into.len()).max(READ_MOST.min(4 * want)).min(READ_MOST);
            into.reserve(ask);
            let at = into.len();
            let spare = &mut into.spare_capacity_mut()[..ask];
            // SAFETY: `read` writes at most `ask` bytes into this spare capacity, which a byte
            // slice may view as written-to memory: the bytes past what `read` fills are never
            // read (the length is set to `at + n` alone below).
            let room = unsafe { core::slice::from_raw_parts_mut(spare.as_mut_ptr().cast::<u8>(), ask) };
            match self.end.read_some(room, deadline, clock)? {
                Read::Bytes(n) => {
                    // SAFETY: `read` initialised `n <= ask` bytes from `at`, within capacity.
                    unsafe { into.set_len(at + n) };
                }
                Read::Deadline => return Ok(Message::Deadline),
                Read::Closed if into.is_empty() => return Ok(Message::Closed),
                Read::Closed => return Err(ChannelError::Torn { have: into.len(), want }),
            }
        }
    }

    /// All of `bytes`, which are whole messages, written by `deadline` on `clock`.
    pub fn send(&mut self, bytes: &[u8], deadline: Deadline, clock: &impl Clock) -> Result<(), ChannelError> {
        let whole = whole_messages(bytes);
        if whole != bytes.len() {
            return Err(ChannelError::NotWhole { at: whole, of: bytes.len() });
        }
        let mut sent = 0;
        while sent < bytes.len() {
            let rest = &bytes[sent..];
            #[cfg(target_os = "linux")]
            let flags = libc::MSG_NOSIGNAL;
            #[cfg(not(target_os = "linux"))]
            let flags = 0;
            // SAFETY: `rest` is live for its length; the socket is this end's.
            let put = unsafe { libc::send(self.end.fd.as_raw_fd(), rest.as_ptr().cast(), rest.len(), flags) };
            if put > 0 {
                sent += put as usize;
                continue;
            }
            match io::Error::last_os_error().raw_os_error() {
                Some(libc::EINTR) => continue,
                Some(libc::EPIPE | libc::ECONNRESET) => return Err(ChannelError::Hangup),
                Some(libc::EAGAIN) => {}
                Some(code) => return Err(ChannelError::Os { op: ChannelOp::Write, os: code }),
                None => return Err(ChannelError::Os { op: ChannelOp::Write, os: 0 }),
            }
            if deadline.passed(clock.now()) || !self.end.ready(libc::POLLOUT, deadline, clock)? {
                return Err(ChannelError::WriteDeadline { sent, of: bytes.len() });
            }
        }
        Ok(())
    }
}

/// Where the last whole message at the front of `bytes` ends (a work list over the prefixes).
pub fn whole_messages(bytes: &[u8]) -> usize {
    let mut at = 0;
    while let Some(prefix) = bytes[at..].first_chunk::<PREFIX>() {
        let end = at + PREFIX + u32::from_be_bytes(*prefix) as usize;
        if end > bytes.len() {
            break;
        }
        at = end;
    }
    at
}

fn os(op: ChannelOp) -> ChannelError {
    ChannelError::Os { op, os: io::Error::last_os_error().raw_os_error().unwrap_or(0) }
}

fn io_os(op: ChannelOp, error: &io::Error) -> ChannelError {
    ChannelError::Os { op, os: error.raw_os_error().unwrap_or(0) }
}

#[cfg(test)]
mod tests {
    //! The platform's rules: socketpair(2) "creates an unnamed pair of connected sockets";
    //! read(2) on a stream socket "returns zero ... end of file" once the peer has closed;
    //! send(2) "MSG_NOSIGNAL: Don't generate a SIGPIPE signal if the peer on a stream-oriented
    //! socket has closed the connection. The EPIPE error is still returned" (Linux), and
    //! SO_NOSIGPIPE on macOS (socket(2): "do not generate SIGPIPE, instead return EPIPE").

    use std::time::Duration;

    use super::*;
    use crate::wait::Monotonic;

    fn raw_write(end: &ChildEnd, bytes: &[u8]) {
        // SAFETY: the child's end, a blocking socket; `bytes` live for its length.
        let put = unsafe { libc::write(end.0.as_raw_fd(), bytes.as_ptr().cast(), bytes.len()) };
        assert_eq!(put, bytes.len() as isize, "the test's write went whole");
    }

    /// Prints "channel: split frame joined, deadline kept the half, torn named, hangup named".
    /// A reader that returned a short message at the peer's close, or dropped the half message
    /// at a deadline, fails it.
    #[test]
    fn messages_come_whole_or_are_named() {
        let clock = Monotonic::new();
        let soon = || Deadline::after(clock.now(), Duration::from_millis(50));
        let (mut parent, child) = pair().expect("a pair");
        let mut inbox = Vec::new();
        raw_write(&child, b"\0\0\0\x03a");
        assert_eq!(parent.recv_message(&mut inbox, 16, soon(), &clock).expect("a wait"), Message::Deadline);
        assert_eq!(&inbox[..], b"\0\0\0\x03a", "the half stays for the next call");
        raw_write(&child, b"bc\0\0\0\x09xy");
        assert_eq!(parent.recv_message(&mut inbox, 16, soon(), &clock).expect("a read"), Message::Whole { len: 7 });
        inbox.drain(..7);
        assert_eq!(&inbox[..], b"\0\0\0\x09xy");
        assert!(parent.send(b"\0\0\0\x02h", soon(), &clock).is_err(), "a part message is never sent");
        parent.send(b"\0\0\0\x02hi", soon(), &clock).expect("a whole message is sent");
        drop(child);
        assert_eq!(
            parent.recv_message(&mut inbox, 16, soon(), &clock),
            Err(ChannelError::Torn { have: 6, want: 13 })
        );
        assert_eq!(parent.send(b"\0\0\0\x00", soon(), &clock), Err(ChannelError::Hangup));
        let (mut parent, child) = pair().expect("a pair");
        raw_write(&child, b"\0\0\0\x40");
        assert_eq!(
            parent.recv_message(&mut Vec::new(), 16, soon(), &clock),
            Err(ChannelError::TooLong { len: 64, most: 16 })
        );
        println!("channel: split frame joined, deadline kept the half, torn named, hangup named");
    }
}
