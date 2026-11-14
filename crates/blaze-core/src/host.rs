//! The host's own effects, the same on every native driver: UDP sockets ([`Effect::Bind`],
//! [`Effect::SendTo`], [`Effect::RecvFrom`], [`Effect::Unbind`]), names resolved
//! ([`Effect::Resolve`]), the standard streams ([`Effect::ReadStdin`], [`Effect::WriteOut`]),
//! the wall clock ([`Effect::Now`]) and the system's random bytes ([`Effect::Random`]). Making
//! and removing an empty directory are file effects, in `threads::run` with the rest.
//!
//! On Unix a [`Host`] runs them for the threads, kqueue and io_uring drivers. What waits on a
//! peer (a receive, a read of the input, a write the output's reader is slow to take) waits on
//! a thread of the host's through [`crate::wait::readable`], with no deadline (it waits as long
//! as nothing comes) and with the host's stop and, for a socket, the socket's own among its
//! descriptors, so an unbind or the driver's end wakes it. Every answer goes through the
//! driver's [`Sink`], which wakes the driver's wait.
//!
//! A batch of datagrams moves in one system call per [`crate::dgram::BATCH_MOST`] messages
//! (`net::udp`: `sendmmsg` and `recvmmsg` on Linux, `sendmsg_x` and `recvmsg_x` on macOS). A
//! send is tried at once on the driver's thread, where nearly every one ends; one the socket
//! has no room for waits on a thread for room. Each socket has one receiving thread, started
//! with its first receive, which answers its receives in the order asked and parks on the
//! socket's [`crate::wait::Parker`] while none is queued: a receive costs no thread's start.
//! A resolve is `getaddrinfo` on a thread of its own, waited for until
//! [`crate::dgram::RESOLVE_WAIT`].
//!
//! The datagram drivers do not yet put a socket on their own kernel queue (io_uring's
//! `IORING_OP_RECVMSG` multishot with provided buffers, kqueue's `EVFILT_READ`): a socket's
//! receives wait on a thread of its own, and a datagram's arrival costs that thread's wake and
//! a wake of the driver.
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
        Effect::SendTo { sock, batch, .. } => Done::dgram_failed(
            batch,
            false,
            sock,
            None,
            None,
            None,
            format_args!("Effect::SendTo is refused {why}"),
        ),
        Effect::WriteOut { bytes, .. } => {
            Done::unsent(bytes, format_args!("Effect::WriteOut is refused {why}"))
        }
        Effect::RecvFrom { sock, into, .. } => Done::dgram_failed(
            into,
            true,
            sock,
            None,
            None,
            None,
            format_args!("Effect::RecvFrom is refused {why}"),
        ),
        Effect::Resolve { name, port, .. } => Done::Unresolved(crate::dgram::Unresolved {
            name,
            port,
            why: format!("Effect::Resolve is refused {why}"),
        }),
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
        net::{SocketAddr, ToSocketAddrs as _, UdpSocket},
        os::fd::{AsFd as _, AsRawFd as _, BorrowedFd, OwnedFd},
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
    };

    use blaze_hash::HashMap;
    use crossbeam_channel::{Receiver, Sender};
    use crossbeam_queue::SegQueue;

    use crate::{
        Completion, Dgram, Done, Effect, IoError, RECV_MOST, Stream, Tag,
        dgram::{self, Batch, Ecn, Options, Slots},
        lock::Lock,
        net::udp::{self, At},
        wait::{self, Deadline, Monotonic, Parker, Waited},
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
        sock: Dgram,
        socket: UdpSocket,
        local: SocketAddr,
        options: Options,
        /// Fired as it is unbound.
        stop: Stop,
        /// The receives asked of it, in order, for its one receiving thread.
        receives: SegQueue<Receive>,
        /// What its receiving thread parks on while `receives` is empty.
        parker: Parker,
        /// Whether its receiving thread was started.
        receiving: AtomicBool,
        /// Set as it is unbound: its receiving thread ends once `receives` is drained.
        unbound: AtomicBool,
    }

    /// A receive of a socket, for its receiving thread.
    struct Receive {
        tag: Tag,
        into: Batch,
        slots: Slots,
    }

    impl Bound {
        fn failed(
            &self,
            batch: Batch,
            clear: bool,
            peer: Option<SocketAddr>,
            error: Option<&io::Error>,
            why: impl std::fmt::Display,
        ) -> Done {
            let errno = error.and_then(io::Error::raw_os_error);
            Done::dgram_failed(batch, clear, self.sock, Some(self.local), peer, errno, why)
        }
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
                    | Effect::Resolve { .. }
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
                Effect::Bind { at, options, tag } => {
                    let sock = Dgram(self.next);
                    let done = match bind(sock, at, options) {
                        Ok(bound) => {
                            self.next += 1;
                            let local = bound.local;
                            // Once per socket bound, shared with its receiving thread.
                            self.socks.insert(sock.0, Arc::new_in(bound, Global));
                            Done::Bound { sock, local }
                        }
                        Err((what, error)) => Done::Failed(IoError(format!(
                            "binding a UDP socket at {at} ({options:?}): {what}: {error}"
                        ))),
                    };
                    self.tell(tag, done);
                }
                Effect::Unbind { sock, tag } => {
                    let done = match self.socks.remove(&sock.0) {
                        Some(bound) => {
                            // The receives in flight wake and end; the socket closes with the
                            // last of them.
                            bound.unbound.store(true, Ordering::Release);
                            bound.stop.fire();
                            bound.parker.unpark();
                            Done::Unbound
                        }
                        // alloc: an error path.
                        None => Done::Failed(IoError(format!(
                            "unbinding {sock:?}: no such socket is bound"
                        ))),
                    };
                    self.tell(tag, done);
                }
                Effect::SendTo { sock, batch, tag } => self.send_to(sock, batch, tag),
                Effect::RecvFrom { sock, into, slots, tag } => {
                    self.recv_from(sock, into, slots, tag);
                }
                Effect::Resolve { name, port, tag } => self.resolve(name, port, tag),
                Effect::WriteOut { stream, bytes, tag } => self.write_out(stream, bytes, tag),
                Effect::ReadStdin { most, into, tag } => self.read_stdin(most, into, tag),
                effect => unreachable!("{:?} is no effect of the host's", effect.tag()),
            }
        }

        /// A batch's send: tried at once on the driver's thread, which is where nearly every
        /// send ends (one `sendmmsg` or `sendmsg_x` per [`dgram::BATCH_MOST`] runs); one the
        /// socket has no room for yet waits for room on a thread of its own.
        fn send_to(&mut self, sock: Dgram, batch: Batch, tag: Tag) {
            let Some(bound) = self.socks.get(&sock.0) else {
                let done = Done::dgram_failed(
                    batch,
                    false,
                    sock,
                    None,
                    None,
                    None,
                    "sending: no such socket is bound",
                );
                return self.tell(tag, done);
            };
            if let Some(refused) = batch.refused() {
                let peer = batch.heads.first().map(|it| it.peer);
                let done = bound.failed(batch, false, peer, None, format_args!("sending {refused}"));
                return self.tell(tag, done);
            }
            if !bound.options.ecn
                && let Some(head) = batch.heads.iter().find(|it| it.ecn != Ecn::NotEct)
            {
                let (peer, ecn) = (head.peer, head.ecn);
                let done = bound.failed(
                    batch,
                    false,
                    Some(peer),
                    None,
                    format_args!(
                        "sending a datagram marked {ecn:?} on a socket bound without \
                         Options::ecn: its codepoint would not be sent"
                    ),
                );
                return self.tell(tag, done);
            }
            match send_now(bound, batch, At::default()) {
                Ok(done) => self.tell(tag, done),
                // No room in the socket's buffer yet: a thread waits for it.
                Err((batch, at)) => {
                    // clone: a count of the socket, for the thread that waits on it.
                    let bound = Arc::clone(bound);
                    let started = self.shared().and_then(|shared| {
                        // clone: a count, for the thread.
                        let thread = Arc::clone(&shared);
                        spawn("blaze-host-send", move || {
                            let done = send_when_room(&thread, &bound, batch, at);
                            thread.sink.tell(Completion { tag, done });
                        })
                    });
                    if let Err(error) = started {
                        // The batch went with the thread not started.
                        let why = format!("sending: starting its wait for room: {error}");
                        let done =
                            Done::dgram_failed(Batch::new(), false, sock, None, None, None, why);
                        self.tell(tag, done);
                    }
                }
            }
        }

        /// A batch's receive, queued for the socket's one receiving thread (started with its
        /// first receive), which answers them in the order asked.
        fn recv_from(&mut self, sock: Dgram, into: Batch, slots: Slots, tag: Tag) {
            let Some(bound) = self.socks.get(&sock.0) else {
                let done = Done::dgram_failed(
                    into,
                    true,
                    sock,
                    None,
                    None,
                    None,
                    "receiving: no such socket is bound",
                );
                return self.tell(tag, done);
            };
            if let Some(why) = slots.refused(bound.options.coalesce) {
                let done = bound.failed(into, true, None, None, why);
                return self.tell(tag, done);
            }
            bound.receives.push(Receive { tag, into, slots });
            if bound.receiving.swap(true, Ordering::AcqRel) {
                bound.parker.unpark();
                return;
            }
            // clone: a count of the socket, for its receiving thread.
            let bound = Arc::clone(bound);
            let started = self.shared().and_then(|shared| {
                // clone: a count of the socket, for the thread.
                let thread = Arc::clone(&bound);
                spawn("blaze-host-recv", move || serve_receives(&shared, &thread))
            });
            if let Err(error) = started {
                bound.receiving.store(false, Ordering::Release);
                while let Some(Receive { tag, into, .. }) = bound.receives.pop() {
                    let why = format!("receiving: starting its thread: {error}");
                    self.tell(tag, bound.failed(into, true, None, None, why));
                }
            }
        }

        /// A name resolved by `getaddrinfo` on a thread of its own, which the thread that
        /// waits for it gives [`dgram::RESOLVE_WAIT`]: `getaddrinfo` cannot be cancelled, so
        /// one past its wait fails the resolve, naming the name, and its answer, when it
        /// comes, has no one to go to.
        fn resolve(&mut self, name: String, port: u16, tag: Tag) {
            if name.len() > dgram::NAME_MOST || name.contains('\0') || name.is_empty() {
                let why = format!(
                    "a name of {} bytes: one is 1 to NAME_MOST ({}) bytes with no NUL",
                    name.len(),
                    dgram::NAME_MOST
                );
                return self.tell(tag, Done::Unresolved(dgram::Unresolved { name, port, why }));
            }
            // clone: the name, for the error of a thread not started, which takes the other.
            let named = name.clone();
            let started = self.shared().and_then(|shared| {
                spawn("blaze-host-resolve", move || {
                    let done = resolve_by(name, port);
                    shared.sink.tell(Completion { tag, done });
                })
            });
            if let Err(error) = started {
                let why = format!("starting its thread: {error}");
                let unresolved = dgram::Unresolved { name: named, port, why };
                self.tell(tag, Done::Unresolved(unresolved));
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
                bound.unbound.store(true, Ordering::Release);
                bound.stop.fire();
                bound.parker.unpark();
            }
        }
    }

    fn spawn(name: &str, f: impl FnOnce() + Send + 'static) -> io::Result<()> {
        // alloc: a thread's name, once per thread.
        std::thread::Builder::new().name(name.to_owned()).spawn(f).map(drop)
    }

    fn bind(
        sock: Dgram,
        at: SocketAddr,
        options: Options,
    ) -> Result<Bound, (&'static str, io::Error)> {
        let socket: UdpSocket = udp::bind(at, options)?;
        let local = socket.local_addr().map_err(|error| ("its address", error))?;
        let stop = Stop::new().map_err(|error| ("its stop", error))?;
        Ok(Bound {
            sock,
            socket,
            local,
            options,
            stop,
            receives: SegQueue::new(),
            parker: Parker::new(),
            receiving: AtomicBool::new(false),
            unbound: AtomicBool::new(false),
        })
    }

    /// Sends of the batch from `at`, one system call each, until all of it is sent: done, or
    /// the batch back and how far it got where the socket has no room yet. Each call moves
    /// at least one message, so this ends in at most as many calls as the batch has
    /// datagrams.
    fn send_now(bound: &Bound, mut batch: Batch, mut at: At) -> Result<Done, (Batch, At)> {
        let v6 = bound.local.is_ipv6();
        while !at.done(&batch) {
            match udp::send(&bound.socket, v6, &batch, at) {
                Ok(next) => at = next,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    return Err((batch, at));
                }
                Err(error) => {
                    let peer = batch.heads.get(at.run).map(|it| it.peer);
                    return Ok(bound.failed(batch, false, peer, Some(&error), error.to_string()));
                }
            }
        }
        batch.clear();
        Ok(Done::SentDatagrams(batch))
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

    /// The rest of a batch's send after a wait for room. Each wait ends with room, the
    /// socket's unbind or the host's end; each send after it moves at least a message, so the
    /// loop ends in at most as many turns as the batch has datagrams.
    fn send_when_room(shared: &Shared, bound: &Bound, batch: Batch, at: At) -> Done {
        let (mut batch, mut at) = (batch, at);
        loop {
            let what = "room in a UDP socket's buffer for an Effect::SendTo";
            let peer = batch.heads.get(at.run).map(|it| it.peer);
            match wait_for(what, bound.socket.as_fd(), libc::POLLOUT, Some(&bound.stop), shared) {
                Woke::Ready => {}
                Woke::Stopped => {
                    return bound.failed(batch, false, peer, None, "unbound with the send in flight");
                }
                Woke::Failed(error) => {
                    return bound.failed(batch, false, peer, Some(&error), error.to_string());
                }
            }
            match send_now(bound, batch, at) {
                Ok(done) => return done,
                Err((back, next)) => (batch, at) = (back, next),
            }
        }
    }

    /// A socket's receiving thread: each receive queued, in order, until the socket is
    /// unbound or the host ends, when each left ends failed. It parks while none is queued.
    fn serve_receives(shared: &Shared, bound: &Bound) {
        let clock = Monotonic::new();
        loop {
            while let Some(Receive { tag, into, slots }) = bound.receives.pop() {
                let done = match bound.unbound.load(Ordering::Acquire) {
                    true => bound.failed(into, true, None, None, "unbound with the receive in flight"),
                    false => receive(shared, bound, into, slots),
                };
                shared.sink.tell(Completion { tag, done });
            }
            if bound.unbound.load(Ordering::Acquire) {
                // A receive pushed between the pop above and the flag is answered, not lost.
                if bound.receives.is_empty() {
                    return;
                }
                continue;
            }
            let what = "the next Effect::RecvFrom of a UDP socket";
            if let Err(error) = bound.parker.park(what, None, &clock) {
                while let Some(Receive { tag, into, .. }) = bound.receives.pop() {
                    let done = bound.failed(into, true, None, None, format_args!("{error}"));
                    shared.sink.tell(Completion { tag, done });
                }
                return;
            }
        }
    }

    /// One receive: a wait until the socket is readable (or unbound, or the host ends), then
    /// one call that takes what is there. A wake with nothing there (another reader of the
    /// socket's, or a spurious one) waits again; each wait is unbounded in time, as the
    /// effect waits as long as nothing comes, and ends with the socket's unbind.
    fn receive(shared: &Shared, bound: &Bound, mut into: Batch, slots: Slots) -> Done {
        loop {
            match udp::recv(&bound.socket, &mut into, slots) {
                Ok(_) => return Done::Datagrams(into),
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                Err(error) => {
                    return bound.failed(into, true, None, Some(&error), error.to_string());
                }
            }
            let what = "a datagram for an Effect::RecvFrom";
            match wait_for(what, bound.socket.as_fd(), libc::POLLIN, Some(&bound.stop), shared) {
                Woke::Ready => {}
                Woke::Stopped => {
                    return bound.failed(into, true, None, None, "unbound with the receive in flight");
                }
                Woke::Failed(error) => {
                    return bound.failed(into, true, None, Some(&error), error.to_string());
                }
            }
        }
    }

    /// `name`'s addresses by `getaddrinfo`, on a thread of their own, waited for until
    /// [`dgram::RESOLVE_WAIT`].
    fn resolve_by(name: String, port: u16) -> Done {
        struct Answer {
            got: Lock<Option<io::Result<Vec<SocketAddr>>>>,
            parker: Parker,
        }
        let answer = Arc::new_in(
            Answer { got: Lock::new("core.host.resolve_answer", None), parker: Parker::new() },
            Global,
        );
        // clone: a count, for the thread that calls getaddrinfo.
        let theirs = Arc::clone(&answer);
        // clone: the name, for that thread; this one names it in the answer.
        let asked = name.clone();
        let started = spawn("blaze-host-getaddrinfo", move || {
            let got = (asked.as_str(), port).to_socket_addrs().map(|found| {
                let mut addresses = Vec::new_in(Global);
                addresses.extend(found);
                addresses
            });
            *theirs.got.lock() = Some(got);
            theirs.parker.unpark();
        });
        if let Err(error) = started {
            let why = format!("starting its thread: {error}");
            return Done::Unresolved(dgram::Unresolved { name, port, why });
        }
        let clock = Monotonic::new();
        let deadline = Deadline::after(wait::Clock::now(&clock), dgram::RESOLVE_WAIT);
        // Each park ends with the answer, a spurious wake (parked again) or the deadline, so
        // this ends by the deadline.
        loop {
            if let Some(got) = answer.got.lock().take() {
                return match got {
                    Ok(addresses) if addresses.is_empty() => Done::Unresolved(dgram::Unresolved {
                        name,
                        port,
                        why: "the system resolver gave no address".to_owned(),
                    }),
                    Ok(addresses) => Done::Resolved(addresses),
                    Err(error) => Done::Unresolved(dgram::Unresolved {
                        name,
                        port,
                        why: error.to_string(),
                    }),
                };
            }
            let what = "getaddrinfo's answer for an Effect::Resolve";
            match answer.parker.park(what, Some(deadline), &clock) {
                Ok(Waited::Ready(())) => {}
                Ok(Waited::Deadline) => {
                    let why = format!(
                        "the system resolver gave no answer in RESOLVE_WAIT, {:?}",
                        dgram::RESOLVE_WAIT
                    );
                    return Done::Unresolved(dgram::Unresolved { name, port, why });
                }
                Err(error) => {
                    let why = error.to_string();
                    return Done::Unresolved(dgram::Unresolved { name, port, why });
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
