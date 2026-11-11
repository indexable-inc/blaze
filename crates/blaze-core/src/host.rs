//! The host's own effects, the same on every native driver: UDP sockets ([`Effect::Bind`],
//! [`Effect::SendTo`], [`Effect::RecvFrom`], [`Effect::Unbind`]), the standard streams
//! ([`Effect::ReadStdin`], [`Effect::WriteOut`]), the wall clock ([`Effect::Now`]) and the
//! system's random bytes ([`Effect::Random`]). Making and removing an empty directory are file
//! effects, in `threads::run` with the rest.
//!
//! On Unix a [`Host`] runs them for the threads, kqueue and io_uring drivers. What waits on a
//! peer (a receive, a read of the input, a write the output's reader is slow to take) waits on
//! a thread of the host's through [`crate::wait::readable`], with no deadline (it waits as long
//! as nothing comes) and with the host's stop and, for a socket, the socket's own among its
//! descriptors, so an unbind or the driver's end wakes it. A send the system takes at once is
//! answered on the driver's thread; one it cannot take yet waits on a thread for room. Every
//! answer goes through the driver's [`Sink`], which wakes the driver's wait.
//!
//! The datagram drivers do not yet put a socket on their own kernel queue (io_uring's
//! `IORING_OP_RECVMSG`, kqueue's `EVFILT_READ`): a receive in flight is a thread of its own.
//!
//! Windows' (the IOCP driver's) and WASI's are [`wall`] and [`random`] alone: their drivers
//! refuse the sockets and the streams, naming why.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::{Done, IoError, RANDOM_MOST};

#[cfg(unix)]
pub(crate) use unix::{Host, Sink};

/// [`Effect::Now`](crate::Effect::Now) of the system's wall clock.
pub(crate) fn wall() -> Done {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(since) => Done::Wall(since),
        // alloc: an error path.
        Err(error) => Done::Failed(IoError(format!(
            "the wall clock reads {:?} before the Unix epoch",
            error.duration()
        ))),
    }
}

/// [`Effect::Random`](crate::Effect::Random) of `len` bytes of the system's secure source
/// (`std::random`'s: `getrandom` on Linux, `arc4random_buf` on macOS, `ProcessPrng` on Windows,
/// `random_get` on WASI).
pub(crate) fn random(len: usize) -> Done {
    if len > RANDOM_MOST {
        // alloc: an error path.
        return Done::Failed(IoError(format!(
            "a draw of {len} random bytes: one draw is at most RANDOM_MOST, {RANDOM_MOST}"
        )));
    }
    // alloc: the bytes, which the completion carries.
    let mut bytes = Vec::with_capacity(len);
    while bytes.len() < len {
        let word: u64 = std::random::random(..);
        let take = (len - bytes.len()).min(8);
        bytes.extend_from_slice(&word.to_le_bytes()[..take]);
    }
    Done::Random(bytes)
}

/// `effect`, a socket's or a standard stream's, refused where the driver has none: the IOCP
/// driver's and WASI's. Its buffer comes back as [`Effect::Send`]'s and [`Effect::Recv`]'s do.
///
/// [`Effect::Send`]: crate::Effect::Send
/// [`Effect::Recv`]: crate::Effect::Recv
#[cfg(not(unix))]
pub(crate) fn refused(effect: crate::Effect) -> Done {
    use crate::Effect;
    #[cfg(windows)]
    let why = "on Windows: the IOCP driver's datagram receive is to be a WSARecvFrom on its \
               port and a standard stream's wait a wait on the stream's handle, and neither is \
               built yet";
    #[cfg(not(windows))]
    let why = "on WASI (the wasm32-wasip1 seed): it has no sockets, and a read of its input \
               or a write of its output would block with no deadline wait to put it behind";
    match effect {
        Effect::SendTo { bytes, .. } => {
            Done::unsent(bytes, format_args!("Effect::SendTo is refused {why}"))
        }
        Effect::WriteOut { bytes, .. } => {
            Done::unsent(bytes, format_args!("Effect::WriteOut is refused {why}"))
        }
        Effect::RecvFrom { into, .. } => {
            Done::unread(into, format_args!("Effect::RecvFrom is refused {why}"))
        }
        Effect::ReadStdin { into, .. } => {
            Done::unread(into, format_args!("Effect::ReadStdin is refused {why}"))
        }
        // alloc: an error path.
        Effect::Bind { .. } => Done::Failed(IoError(format!("Effect::Bind is refused {why}"))),
        // alloc: an error path.
        Effect::Unbind { .. } => Done::Failed(IoError(format!("Effect::Unbind is refused {why}"))),
        effect => unreachable!("{:?} is no socket's or stream's", effect.tag()),
    }
}

#[cfg(unix)]
mod unix {
    use std::{
        alloc::Global,
        io,
        net::{SocketAddr, UdpSocket},
        os::fd::{AsFd as _, AsRawFd as _, BorrowedFd, OwnedFd},
        sync::Arc,
    };

    use blaze_hash::HashMap;
    use crossbeam_channel::{Receiver, Sender};

    use crate::{
        Completion, DATAGRAM_MOST, Dgram, Done, Effect, IoError, RECV_MOST, Stream, Tag,
        wait::{self, Monotonic, Waited},
    };

    /// Where the host tells a completion: the driver's mailbox, waking its wait. Called from
    /// the driver's thread and from the host's.
    pub(crate) trait Sink: Send + Sync + 'static {
        fn tell(&self, completion: Completion);
    }

    /// The most bytes one write of a standard stream writes after its wait said it is
    /// writable: POSIX's least `PIPE_BUF`, which a pipe that is writable always takes whole
    /// without blocking.
    const WRITE_MOST: usize = 512;

    /// A stop: a pipe whose read end is readable for good once a byte is written to it (the
    /// byte is never read) or once its write end is closed.
    struct Stop {
        read: OwnedFd,
        write: OwnedFd,
    }

    impl Stop {
        fn new() -> io::Result<Stop> {
            let (read, write) = crate::raw::pipe()?;
            Ok(Stop { read, write })
        }

        fn fire(&self) {
            // A full pipe fails it, and is readable already: the stop is seen either way.
            _ = crate::raw::write(self.write.as_fd(), b"x");
        }
    }

    /// A socket bound, shared by the host's table and the threads that wait on it.
    struct Bound {
        socket: UdpSocket,
        local: SocketAddr,
        /// Fired as it is unbound.
        stop: Stop,
    }

    /// What the host's threads share with it.
    struct Shared {
        // dyn: the driver's mailbox, given once as the driver is made.
        sink: Arc<dyn Sink>,
        /// Fired as the host is dropped: every thread of its ends.
        stop: Stop,
    }

    /// A write of a standard stream, for the stream's thread.
    struct Write {
        tag: Tag,
        bytes: Vec<u8>,
    }

    /// A read of the standard input, for its thread.
    struct Read {
        tag: Tag,
        most: usize,
        into: Vec<u8>,
    }

    /// The host's effects for one driver. Kept on the driver's thread.
    pub(crate) struct Host {
        // dyn: as `Shared::sink`, until the first effect that needs a thread.
        sink: Arc<dyn Sink>,
        /// Made with the first effect that waits.
        shared: Option<Arc<Shared>>,
        /// Each socket bound, by its [`Dgram`].
        socks: HashMap<u64, Arc<Bound>, Global>,
        /// The next socket's name: from 1, never named twice.
        next: u64,
        /// The queues of the threads that write the standard output and error, and read the
        /// standard input, each made with its stream's first effect.
        stdout: Option<Sender<Write>>,
        stderr: Option<Sender<Write>>,
        stdin: Option<Sender<Read>>,
    }

    impl Host {
        // dyn: as `Shared::sink`.
        pub(crate) fn new(sink: Arc<dyn Sink>) -> Host {
            Host {
                sink,
                shared: None,
                socks: HashMap::new_in(Global),
                next: 1,
                stdout: None,
                stderr: None,
                stdin: None,
            }
        }

        /// Whether `effect` is the host's: [`Host::submit`] takes it, and it ends with one
        /// completion told through the sink.
        pub(crate) fn takes(effect: &Effect) -> bool {
            matches!(
                effect,
                Effect::Bind { .. }
                    | Effect::SendTo { .. }
                    | Effect::RecvFrom { .. }
                    | Effect::Unbind { .. }
                    | Effect::ReadStdin { .. }
                    | Effect::WriteOut { .. }
                    | Effect::Now { .. }
                    | Effect::Random { .. }
            )
        }

        fn tell(&self, tag: Tag, done: Done) {
            self.sink.tell(Completion { tag, done });
        }

        /// What the host's threads share, made for the first effect that waits.
        fn shared(&mut self) -> io::Result<Arc<Shared>> {
            if let Some(shared) = &self.shared {
                // clone: a count of the shared state, for a thread.
                return Ok(Arc::clone(shared));
            }
            // clone: a count of the driver's mailbox.
            // alloc: once per host.
            let shared = Arc::new(Shared { sink: Arc::clone(&self.sink), stop: Stop::new()? });
            // clone: a count, kept.
            self.shared = Some(Arc::clone(&shared));
            Ok(shared)
        }

        /// Runs the host's `effect` ([`Host::takes`]), its completion told through the sink.
        pub(crate) fn submit(&mut self, effect: Effect) {
            match effect {
                Effect::Now { tag } => self.tell(tag, super::wall()),
                Effect::Random { len, tag } => self.tell(tag, super::random(len)),
                Effect::Bind { at, tag } => {
                    let done = match bind(at) {
                        Ok(bound) => {
                            let sock = Dgram(self.next);
                            self.next += 1;
                            let local = bound.local;
                            // alloc: once per socket bound, shared with its receives.
                            self.socks.insert(sock.0, Arc::new(bound));
                            Done::Bound { sock, local }
                        }
                        // alloc: an error path.
                        Err(error) => {
                            Done::Failed(IoError(format!("binding a UDP socket at {at}: {error}")))
                        }
                    };
                    self.tell(tag, done);
                }
                Effect::Unbind { sock, tag } => {
                    let done = match self.socks.remove(&sock.0) {
                        Some(bound) => {
                            // The receives in flight wake and end; the socket closes with the
                            // last of them.
                            bound.stop.fire();
                            Done::Unbound
                        }
                        // alloc: an error path.
                        None => Done::Failed(IoError(format!(
                            "unbinding {sock:?}: no such socket is bound"
                        ))),
                    };
                    self.tell(tag, done);
                }
                Effect::SendTo { sock, to, bytes, tag } => self.send_to(sock, to, bytes, tag),
                Effect::RecvFrom { sock, into, tag } => self.recv_from(sock, into, tag),
                Effect::WriteOut { stream, bytes, tag } => self.write_out(stream, bytes, tag),
                Effect::ReadStdin { most, into, tag } => self.read_stdin(most, into, tag),
                effect => unreachable!("{:?} is no effect of the host's", effect.tag()),
            }
        }

        fn send_to(&mut self, sock: Dgram, to: SocketAddr, bytes: Vec<u8>, tag: Tag) {
            let Some(bound) = self.socks.get(&sock.0) else {
                let done = Done::unsent(
                    bytes,
                    format_args!("sending to {to} from {sock:?}: no such socket is bound"),
                );
                return self.tell(tag, done);
            };
            if bytes.len() > DATAGRAM_MOST {
                let (len, local) = (bytes.len(), bound.local);
                let done = Done::unsent(
                    bytes,
                    format_args!(
                        "sending {len} bytes from {local} to {to}: a datagram is at most \
                         DATAGRAM_MOST, {DATAGRAM_MOST}"
                    ),
                );
                return self.tell(tag, done);
            }
            match send_now(bound, to, bytes) {
                Ok(done) => self.tell(tag, done),
                // No room in the socket's buffer yet: a thread waits for it.
                Err(bytes) => {
                    // clone: a count of the socket, for the thread that waits on it.
                    let bound = Arc::clone(bound);
                    let started = self.shared().and_then(|shared| {
                        // clone: a count, for the thread.
                        let thread = Arc::clone(&shared);
                        spawn("blaze-host-send", move || {
                            let done = send_when_room(&thread, &bound, to, bytes);
                            thread.sink.tell(Completion { tag, done });
                        })
                    });
                    if let Err(error) = started {
                        // alloc: an error path; the bytes went with the thread not started.
                        let why = format!("sending to {to}: starting its wait: {error}");
                        self.tell(tag, Done::unsent(Vec::new(), why));
                    }
                }
            }
        }

        fn recv_from(&mut self, sock: Dgram, into: Vec<u8>, tag: Tag) {
            let Some(bound) = self.socks.get(&sock.0) else {
                let done = Done::unread(
                    into,
                    format_args!("receiving on {sock:?}: no such socket is bound"),
                );
                return self.tell(tag, done);
            };
            // clone: a count of the socket, for the thread that waits on it.
            let bound = Arc::clone(bound);
            let shared = match self.shared() {
                Ok(shared) => shared,
                Err(error) => {
                    let local = bound.local;
                    let done = Done::unread(into, format_args!("receiving on {local}: {error}"));
                    return self.tell(tag, done);
                }
            };
            let started = spawn("blaze-host-recv", move || {
                let done = receive(&shared, &bound, into);
                shared.sink.tell(Completion { tag, done });
            });
            if let Err(error) = started {
                // alloc: an error path; the buffer went with the thread not started.
                let why = format!("receiving on {sock:?}: starting its wait: {error}");
                self.tell(tag, Done::unread(Vec::new(), why));
            }
        }

        fn write_out(&mut self, stream: Stream, bytes: Vec<u8>, tag: Tag) {
            let queue = match stream {
                Stream::Stdout => self.stdout.as_ref(),
                Stream::Stderr => self.stderr.as_ref(),
            };
            let queue = match queue {
                // clone: a channel's handle.
                Some(queue) => queue.clone(),
                None => match self.stream_thread(stream) {
                    Ok(queue) => queue,
                    Err(error) => {
                        let done = Done::unsent(
                            bytes,
                            format_args!("writing {stream}: starting its thread: {error}"),
                        );
                        return self.tell(tag, done);
                    }
                },
            };
            if let Err(crossbeam_channel::SendError(write)) = queue.send(Write { tag, bytes }) {
                let done = Done::unsent(
                    write.bytes,
                    format_args!("writing {stream}: its thread has ended"),
                );
                self.tell(tag, done);
            }
        }

        /// The thread that writes `stream`, started, and its queue kept.
        fn stream_thread(&mut self, stream: Stream) -> io::Result<Sender<Write>> {
            let shared = self.shared()?;
            // alloc: once per stream written, the queue of its writes.
            let (send, take): (Sender<Write>, Receiver<Write>) = crossbeam_channel::unbounded();
            spawn("blaze-host-out", move || {
                while let Ok(Write { tag, bytes }) = take.recv() {
                    let done = write_all(&shared, stream, bytes);
                    shared.sink.tell(Completion { tag, done });
                }
            })?;
            let kept = match stream {
                Stream::Stdout => &mut self.stdout,
                Stream::Stderr => &mut self.stderr,
            };
            // clone: a channel's handle, kept.
            *kept = Some(send.clone());
            Ok(send)
        }

        fn read_stdin(&mut self, most: usize, into: Vec<u8>, tag: Tag) {
            if self.stdin.is_none() {
                let started = self.shared().and_then(|shared| {
                    // alloc: once per host that reads its input, the queue of its reads.
                    let (send, take): (Sender<Read>, Receiver<Read>) =
                        crossbeam_channel::unbounded();
                    spawn("blaze-host-in", move || {
                        while let Ok(Read { tag, most, into }) = take.recv() {
                            let done = read_input(&shared, most, into);
                            shared.sink.tell(Completion { tag, done });
                        }
                    })?;
                    Ok(send)
                });
                match started {
                    Ok(send) => self.stdin = Some(send),
                    Err(error) => {
                        let done = Done::unread(
                            into,
                            format_args!(
                                "reading the standard input: starting its thread: {error}"
                            ),
                        );
                        return self.tell(tag, done);
                    }
                }
            }
            let queue = self.stdin.as_ref().expect("started above");
            if let Err(crossbeam_channel::SendError(read)) = queue.send(Read { tag, most, into }) {
                let why = "reading the standard input: its thread has ended";
                self.tell(tag, Done::unread(read.into, why));
            }
        }
    }

    impl Drop for Host {
        fn drop(&mut self) {
            if let Some(shared) = &self.shared {
                shared.stop.fire();
            }
            for bound in self.socks.values() {
                bound.stop.fire();
            }
        }
    }

    fn spawn(name: &str, f: impl FnOnce() + Send + 'static) -> io::Result<()> {
        // alloc: a thread's name, once per thread.
        std::thread::Builder::new().name(name.to_owned()).spawn(f).map(drop)
    }

    fn bind(at: SocketAddr) -> io::Result<Bound> {
        // std's sockets are made close-on-exec.
        let socket = UdpSocket::bind(at)?;
        socket.set_nonblocking(true)?;
        let local = socket.local_addr()?;
        Ok(Bound { socket, local, stop: Stop::new()? })
    }

    /// One try of the send: done, or the bytes back where the socket has no room yet.
    fn send_now(bound: &Bound, to: SocketAddr, mut bytes: Vec<u8>) -> Result<Done, Vec<u8>> {
        loop {
            match bound.socket.send_to(&bytes, to) {
                Ok(_) => {
                    bytes.clear();
                    return Ok(Done::Sent(bytes));
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Err(bytes),
                Err(error) => {
                    let local = bound.local;
                    return Ok(Done::unsent(
                        bytes,
                        format_args!("sending from {local} to {to}: {error}"),
                    ));
                }
            }
        }
    }

    /// What woke a wait of the host's: its descriptor, or a stop.
    enum Woke {
        Ready,
        Stopped,
        Failed(io::Error),
    }

    /// Waits until `fd` is ready for `events`, or `stop` or the host's stop fires.
    fn wait_for(
        what: &'static str,
        fd: BorrowedFd<'_>,
        events: libc::c_short,
        stop: Option<&Stop>,
        shared: &Shared,
    ) -> Woke {
        let mut fds = [
            libc::pollfd { fd: fd.as_raw_fd(), events, revents: 0 },
            libc::pollfd { fd: shared.stop.read.as_raw_fd(), events: libc::POLLIN, revents: 0 },
            // A second look at the host's stop where there is no socket's: the same either way.
            libc::pollfd {
                fd: stop.unwrap_or(&shared.stop).read.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        match wait::readable(what, &mut fds, None, &Monotonic::new()) {
            Ok(Waited::Ready(_)) if fds[1].revents != 0 || fds[2].revents != 0 => Woke::Stopped,
            Ok(Waited::Ready(_)) => Woke::Ready,
            // No deadline: it does not pass.
            Ok(Waited::Deadline) => Woke::Ready,
            Err(error) => Woke::Failed(io::Error::other(error)),
        }
    }

    fn send_when_room(shared: &Shared, bound: &Bound, to: SocketAddr, bytes: Vec<u8>) -> Done {
        let mut bytes = bytes;
        loop {
            let what = "room in a UDP socket's buffer for an Effect::SendTo";
            let local = bound.local;
            match wait_for(what, bound.socket.as_fd(), libc::POLLOUT, Some(&bound.stop), shared) {
                Woke::Ready => {}
                Woke::Stopped => {
                    return Done::unsent(
                        bytes,
                        format_args!(
                            "sending from {local} to {to}: unbound with the send in flight"
                        ),
                    );
                }
                Woke::Failed(error) => {
                    return Done::unsent(
                        bytes,
                        format_args!("sending from {local} to {to}: {error}"),
                    );
                }
            }
            match send_now(bound, to, bytes) {
                Ok(done) => return done,
                Err(back) => bytes = back,
            }
        }
    }

    fn receive(shared: &Shared, bound: &Bound, mut into: Vec<u8>) -> Done {
        loop {
            let what = "a datagram for an Effect::RecvFrom";
            let local = bound.local;
            match wait_for(what, bound.socket.as_fd(), libc::POLLIN, Some(&bound.stop), shared) {
                Woke::Ready => {}
                Woke::Stopped => {
                    return Done::unread(
                        into,
                        format_args!("receiving on {local}: unbound with the receive in flight"),
                    );
                }
                Woke::Failed(error) => {
                    return Done::unread(into, format_args!("receiving on {local}: {error}"));
                }
            }
            into.clear();
            // alloc: none once the asker's buffer has grown to a datagram's most.
            into.resize(DATAGRAM_MOST, 0);
            match bound.socket.recv_from(&mut into) {
                Ok((n, from)) => {
                    into.truncate(n);
                    return Done::Datagram { from, bytes: into };
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) =>
                {
                    into.clear();
                }
                Err(error) => {
                    into.clear();
                    return Done::unread(into, format_args!("receiving on {local}: {error}"));
                }
            }
        }
    }

    /// All of `bytes` written to `stream`, [`WRITE_MOST`] at a time after a wait for room.
    fn write_all(shared: &Shared, stream: Stream, mut bytes: Vec<u8>) -> Done {
        let (out, err) = (std::io::stdout(), std::io::stderr());
        let fd = match stream {
            Stream::Stdout => out.as_fd(),
            Stream::Stderr => err.as_fd(),
        };
        let mut at = 0;
        while at < bytes.len() {
            let what = "room in a standard stream for an Effect::WriteOut";
            match wait_for(what, fd, libc::POLLOUT, None, shared) {
                Woke::Ready => {}
                Woke::Stopped => {
                    return Done::unsent(bytes, format_args!("writing {stream}: the driver ended"));
                }
                Woke::Failed(error) => {
                    return Done::unsent(bytes, format_args!("writing {stream}: {error}"));
                }
            }
            let end = bytes.len().min(at + WRITE_MOST);
            match crate::raw::write(fd, &bytes[at..end]) {
                Ok(n) => at += n,
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                Err(error) => {
                    return Done::unsent(bytes, format_args!("writing {stream}: {error}"));
                }
            }
        }
        bytes.clear();
        Done::Sent(bytes)
    }

    /// What the standard input holds next, at most `most` and [`RECV_MOST`], after `into`'s.
    fn read_input(shared: &Shared, most: usize, mut into: Vec<u8>) -> Done {
        let most = most.min(RECV_MOST);
        if most == 0 {
            return Done::Read(into);
        }
        let input = std::io::stdin();
        let fd = input.as_fd();
        loop {
            let what = "the standard input for an Effect::ReadStdin";
            match wait_for(what, fd, libc::POLLIN, None, shared) {
                Woke::Ready => {}
                Woke::Stopped => {
                    let why = "reading the standard input: the driver ended";
                    return Done::unread(into, why);
                }
                Woke::Failed(error) => {
                    return Done::unread(into, format_args!("reading the standard input: {error}"));
                }
            }
            let kept = into.len();
            // alloc: none once the asker's buffer has room for what comes.
            into.resize(kept + most, 0);
            match crate::raw::read(fd, &mut into[kept..]) {
                Ok(n) => {
                    into.truncate(kept + n);
                    return Done::Read(into);
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) =>
                {
                    into.truncate(kept);
                }
                Err(error) => {
                    into.truncate(kept);
                    return Done::unread(into, format_args!("reading the standard input: {error}"));
                }
            }
        }
    }
}
