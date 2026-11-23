//! UDP sockets ([`Datagrams`]) over blaze-core's datagram effects (`Effect::Bind`, `SendTo`,
//! `RecvFrom`, `Unbind`): batched sends and receives in [`Batch`]es, whose runs and heads
//! say each datagram's peer, ECN mark, segment length and the local address it came to.
//!
//! **Reach.** The same capability as TCP's, narrowed the same way and never widened: a bind
//! is a listen's reach ([`Net::bind_only`]; [`Net::connect_only`] and [`Net::listen_only`]
//! bind nowhere), and a send to a peer is a connect's ([`Net::peers_only`]). A batch with one
//! datagram to a peer the `Net` does not reach is refused whole before anything is sent
//! ([`NetKind::PeerNotAllowed`]). A receive takes what any peer sent, as an accept does.
//!
//! **Byte limit.** The `Net`'s ([`Net::limit`]), shared with its connections: a send takes
//! the batch's bytes before it starts, and with no room for them nothing is sent
//! ([`NetKind::OverLimit`]); a receive asks for no more slots than the limit has room for,
//! takes their bytes as it starts and gives back what did not come.
//!
//! **Finishing.** Linear: [`Datagrams::close`] unbinds and waits for the driver's word; a
//! panic that drops one unbinds it without waiting.
//!
//! **Waits.** A send or receive waits under its `Cx` ([`Cx::wait`]: its span and its
//! deadline): a receive with nothing coming ends at the deadline as
//! [`NetKind::Stopped`], its batch left with the driver.

use core::{fmt, net::SocketAddr};

use blaze_core::{
    Dgram, Done, Effect, Tag,
    dgram::{Batch, Options, Slots},
    sockopt::Applied,
};

// `Arc` is net.rs's import: the budget's shared form there.
use super::{Addr, Allowed, Arc, Conn, HostPort, Listener, Net, NetError, NetKind, NetOp, error, options, unexpected};
use crate::{
    Plant,
    budget::Budget,
    cx::Cx,
    io::{Ends, Io},
    unfinished::Unfinished,
};

/// Whether `allowed` reaches `peer` for a datagram: any, or only a UDP host whose address
/// is `peer`.
#[blaze::nopanic]
#[blaze::noalloc]
#[blaze::nonblocking]
fn reaches(allowed: &Allowed, peer: SocketAddr) -> bool {
    match allowed {
        Allowed::Any => true,
        Allowed::Only(Addr::Udp(only)) => only.socket_addr() == Some(peer),
        Allowed::Only(Addr::Tcp(_) | Addr::Unix(_)) | Allowed::Nothing => false,
    }
}

/// The bytes of a batch's runs: what a send moves or a receive brought.
#[blaze::nopanic]
#[blaze::noalloc]
#[blaze::nonblocking]
fn carried(batch: &Batch) -> u64 {
    batch.runs().map(|(head, _)| u64::from(head.len)).sum()
}

impl Net {
    /// Binds only at `at`, of what this binds at, sends where this sends, and listens
    /// nowhere: for a UDP socket's owner.
    pub fn bind_only(&self, at: &HostPort) -> Net {
        // clone: the bind's address, the reach kept, the budget shared: refcounts or copies.
        Net {
            io: self.io.clone(),
            connect: self.connect.clone(),
            listen: Allowed::Nothing,
            bind: self.bind.narrowed(&Addr::Udp(at.clone())),
            budget: self.budget.clone(),
        }
    }

    /// Sends datagrams only to `peer`, of what this sends to, binds where this binds, and
    /// listens nowhere.
    pub fn peers_only(&self, peer: &HostPort) -> Net {
        // clone: the peer's address, the reach kept, the budget shared: refcounts or copies.
        Net {
            io: self.io.clone(),
            connect: self.connect.narrowed(&Addr::Udp(peer.clone())),
            listen: Allowed::Nothing,
            bind: self.bind.clone(),
            budget: self.budget.clone(),
        }
    }

    /// A UDP socket bound at `at` (port 0: one the system picks), set up as `options` asks:
    /// its per-datagram options (ECN, local address, don't-fragment, coalescing) and its
    /// tuning ([`options::UdpOptions`]: buffers, reuse, DSCP, path-MTU mode), set before the
    /// bind. An option this system lacks, asked [`options::Ask::Must`], and options that
    /// contradict are refused before the driver is asked ([`NetKind::Unavailable`]); what
    /// was set is [`Datagrams::applied`]. Finished with [`Datagrams::close`].
    #[track_caller]
    pub fn bind_datagrams(&self, cx: &Cx, at: &HostPort, options: &Options) -> Result<Datagrams, NetError> {
        // clone: the address, for its errors.
        let addr = Addr::Udp(at.clone());
        if !self.bind.allows(&addr) {
            return Err(error(NetOp::Bind, &addr, NetKind::NotAllowed));
        }
        let skipped = options::udp_checked(&self.io, options, &addr)?;
        let Some(socket_addr) = at.socket_addr() else {
            return Err(error(NetOp::Bind, &addr, NetKind::NoLookup));
        };
        let options = *options;
        let done = self
            .io
            .call(cx, |tag| Effect::Bind { at: socket_addr, options, tag })
            .map_err(|stopped| error(NetOp::Bind, &addr, NetKind::Stopped(stopped)))?;
        match done {
            Done::Bound { sock, local, buffers } => Ok(Datagrams {
                // clone: the world, a refcount.
                socket: Socket { io: self.io.clone(), sock, open: true },
                addr,
                local,
                options,
                applied: Applied { buffers, skipped },
                // clone: the reach and the budget shared: refcounts or copies.
                peers: self.connect.clone(),
                budget: self.budget.clone(),
                unfinished: Unfinished::new("net::Datagrams", "close(cx)"),
            }),
            done => Err(unexpected(NetOp::Bind, &addr, done)),
        }
    }
}

/// A bound UDP socket, finished with [`Datagrams::close`]. Linear (see the module's doc).
#[blaze::linear]
#[must_use = "a UDP socket is finished with close(cx)"]
pub struct Datagrams {
    socket: Socket,
    /// The address it was asked to bind at, for its errors.
    addr: Addr,
    /// The address it is bound at.
    local: SocketAddr,
    options: Options,
    applied: Applied,
    /// The peers its sends may reach: the `Net`'s connect reach.
    peers: Allowed,
    /// The byte limit of the `Net` it came from.
    budget: Option<Arc<Budget>>,
    unfinished: Unfinished,
}

/// A socket's driver handle and its abandonment path.
struct Socket {
    io: Io,
    sock: Dgram,
    /// Cleared once it is unbound.
    open: bool,
}

impl Drop for Socket {
    fn drop(&mut self) {
        if self.open {
            let sock = self.sock;
            let tag = self.io.submit(|tag| Effect::Unbind { sock, tag });
            self.io.forget(tag, Ends::Once);
        }
    }
}

impl Datagrams {
    /// The address it is bound at: the port the system picked where it was asked for 0.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn local(&self) -> SocketAddr {
        self.local
    }

    /// What was set: its buffers as the kernel said them once bound (Linux doubles what is
    /// asked; the simulation keeps it), and the options asked
    /// [`options::Ask::WhereAvailable`] that this system lacks.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn applied(&self) -> Applied {
        self.applied
    }

    /// The options it was bound with.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn options(&self) -> Options {
        self.options
    }

    /// Every datagram of `batch` sent, each to its head's peer with its head's ECN mark (and
    /// from its head's local address, where it names one), in order: a run of several
    /// datagrams goes as one segmented send where the system has one. The answer is the
    /// batch cleared, its capacity kept, once the system took every datagram (not once they
    /// arrived). Refused before anything is sent: a datagram to a peer this does not reach
    /// ([`NetKind::PeerNotAllowed`]) and a batch the byte limit has no room for
    /// ([`NetKind::OverLimit`]).
    #[track_caller]
    pub fn send(&self, cx: &Cx, batch: Batch) -> Result<Batch, NetError> {
        let io = &self.socket.io;
        if let Some((head, _)) = batch.runs().find(|(head, _)| !reaches(&self.peers, head.peer)) {
            return Err(error(NetOp::Send, &self.addr, NetKind::PeerNotAllowed { peer: head.peer }));
        }
        let asked = carried(&batch);
        // The planted bug of the budget tests: the limit is looked at once the bytes are gone.
        let after = io.plant() == Plant::BudgetCheckedAfter;
        let budget = self.budget.as_deref();
        match budget {
            Some(budget) if !after => {
                budget.take(asked).map_err(|over| error(NetOp::Send, &self.addr, NetKind::OverLimit(over)))?;
            }
            _ => {}
        }
        let sock = self.socket.sock;
        let done = io
            .call(cx, |tag| Effect::SendTo { sock, batch, tag })
            .map_err(|stopped| error(NetOp::Send, &self.addr, NetKind::Stopped(stopped)))?;
        match done {
            Done::SentDatagrams(batch) => match budget {
                Some(budget) if after => budget
                    .take(asked)
                    .map(|()| batch)
                    .map_err(|over| error(NetOp::Send, &self.addr, NetKind::OverLimit(over))),
                _ => Ok(batch),
            },
            Done::DatagramsFailed { error: failed, .. } => {
                Err(error(NetOp::Send, &self.addr, NetKind::Datagrams(*failed)))
            }
            done => Err(unexpected(NetOp::Send, &self.addr, done)),
        }
    }

    /// The next datagrams it receives, at least one and as many as are there at once up to
    /// `slots`, into `into` (cleared first, its capacity kept: hand each batch to the next
    /// receive and nothing is allocated once it has grown). Each head says the peer, the ECN
    /// mark, the local address it came to (where bound with `local_address`) and, coalesced,
    /// the segment length. Under a byte limit it asks for no more slots than the limit has
    /// room for, takes their bytes as it starts and gives back what did not come; with no
    /// room for one slot it never starts ([`NetKind::OverLimit`]). Waits until `cx`'s
    /// deadline; stopped, the receive is left with the driver and `into` with it.
    #[track_caller]
    pub fn recv(&self, cx: &Cx, into: Batch, slots: Slots) -> Result<Batch, NetError> {
        let io = &self.socket.io;
        cx.stopped_by(io.now()).map_err(|stopped| error(NetOp::Recv, &self.addr, NetKind::Stopped(stopped)))?;
        let budget = self.budget.as_deref();
        let mut slots = slots;
        let mut taken = 0u64;
        if let Some(budget) = budget {
            let fit = budget.left() / u64::from(slots.size.max(1));
            if fit == 0 {
                let over = budget.over(u64::from(slots.size));
                return Err(error(NetOp::Recv, &self.addr, NetKind::OverLimit(over)));
            }
            // Below `slots.most`, so it fits a u16.
            slots.most = u16::try_from(fit.min(u64::from(slots.most))).unwrap_or(slots.most);
            taken = u64::from(slots.most) * u64::from(slots.size);
            budget.take(taken).map_err(|over| error(NetOp::Recv, &self.addr, NetKind::OverLimit(over)))?;
        }
        // What was taken from the limit and did not come is given back.
        let unused = |came: u64| {
            if let Some(budget) = budget {
                budget.give_back(taken - came.min(taken));
            }
        };
        let sock = self.socket.sock;
        let tag: Tag = io.submit(|tag| Effect::RecvFrom { sock, into, slots, tag });
        let done = match cx.wait(io, tag, Ends::Once) {
            Ok(done) => done,
            Err(stopped) => {
                io.forget(tag, Ends::Once);
                unused(0);
                return Err(error(NetOp::Recv, &self.addr, NetKind::Stopped(stopped)));
            }
        };
        match done {
            Done::Datagrams(batch) => {
                // At most the slots' bytes come: blaze-core fills no more than `most` slots
                // of `size` bytes each.
                unused(carried(&batch));
                Ok(batch)
            }
            Done::DatagramsFailed { error: failed, .. } => {
                unused(0);
                Err(error(NetOp::Recv, &self.addr, NetKind::Datagrams(*failed)))
            }
            done => {
                unused(0);
                Err(unexpected(NetOp::Recv, &self.addr, done))
            }
        }
    }

    /// The socket unbound: each receive in flight on it ends, and the driver's word that it
    /// is unbound, or why not, op [`NetOp::Close`]. The one way a `Datagrams` ends.
    #[blaze::consumes]
    pub fn close(mut self, cx: &Cx) -> Result<(), NetError> {
        self.unfinished.finished();
        self.socket.open = false;
        let sock = self.socket.sock;
        let done = self
            .socket
            .io
            .call(cx, |tag| Effect::Unbind { sock, tag })
            .map_err(|stopped| error(NetOp::Close, &self.addr, NetKind::Stopped(stopped)))?;
        match done {
            Done::Unbound => Ok(()),
            done => Err(unexpected(NetOp::Close, &self.addr, done)),
        }
    }
}

/// Outcome of waiting simultaneously on a UDP socket and a TCP listener ([`RecvArm::wait_or_accept`]).
pub enum DatagramsOrAccept {
    /// One or more UDP datagrams arrived.
    Datagrams(Batch),
    /// A TCP client connected to the listener.
    Accepted(Conn),
}

/// Keeps a single `Effect::RecvFrom` armed across TCP accepts and poll deadlines so no UDP
/// datagram is ever forgotten or dropped between iterations on a single-driver event loop.
pub struct RecvArm {
    tag: Option<Tag>,
    taken: u64,
}

impl RecvArm {
    pub const fn new() -> RecvArm {
        RecvArm { tag: None, taken: 0 }
    }

    /// Waits for either a UDP datagram on `udp` or an incoming TCP connection on `listener`,
    /// reusing the same in-flight `Effect::RecvFrom` tag whenever `listener` fires or `cx`
    /// hits a deadline.
    pub fn wait_or_accept(
        &mut self,
        cx: &Cx,
        udp: &Datagrams,
        listener: &mut Listener,
        into: &mut Option<Batch>,
        slots: Slots,
    ) -> Result<DatagramsOrAccept, NetError> {
        let io = &udp.socket.io;
        cx.stopped_by(io.now()).map_err(|stopped| error(NetOp::Recv, &udp.addr, NetKind::Stopped(stopped)))?;
        let udp_tag = match self.tag {
            Some(tag) => tag,
            None => {
                let budget = udp.budget.as_deref();
                let mut slots = slots;
                let mut taken = 0u64;
                if let Some(budget) = budget {
                    let fit = budget.left() / u64::from(slots.size.max(1));
                    if fit == 0 {
                        let over = budget.over(u64::from(slots.size));
                        return Err(error(NetOp::Recv, &udp.addr, NetKind::OverLimit(over)));
                    }
                    slots.most = u16::try_from(fit.min(u64::from(slots.most))).unwrap_or(slots.most);
                    taken = u64::from(slots.most) * u64::from(slots.size);
                    budget.take(taken).map_err(|over| error(NetOp::Recv, &udp.addr, NetKind::OverLimit(over)))?;
                }
                let batch = into.take().unwrap_or_else(Batch::new);
                let sock = udp.socket.sock;
                let tag = io.submit(|tag| Effect::RecvFrom { sock, into: batch, slots, tag });
                self.tag = Some(tag);
                self.taken = taken;
                tag
            }
        };
        let tags = [udp_tag, listener.accept_tag()];
        match cx.wait_any(io, &tags) {
            Ok((0, done)) => {
                self.tag = None;
                let taken = core::mem::replace(&mut self.taken, 0);
                let unused = |came: u64| {
                    if let Some(budget) = udp.budget.as_deref() {
                        budget.give_back(taken - came.min(taken));
                    }
                };
                match done {
                    Done::Datagrams(batch) => {
                        unused(carried(&batch));
                        Ok(DatagramsOrAccept::Datagrams(batch))
                    }
                    Done::DatagramsFailed { error: failed, .. } => {
                        unused(0);
                        Err(error(NetOp::Recv, &udp.addr, NetKind::Datagrams(*failed)))
                    }
                    done => {
                        unused(0);
                        Err(unexpected(NetOp::Recv, &udp.addr, done))
                    }
                }
            }
            Ok((_, done)) => listener.accept_done(done).map(DatagramsOrAccept::Accepted),
            Err(stopped) => Err(error(NetOp::Recv, &udp.addr, NetKind::Stopped(stopped))),
        }
    }

    /// Forgets any armed `Effect::RecvFrom` before closing `udp`.
    pub fn disarm(&mut self, udp: &Datagrams) {
        if let Some(tag) = self.tag.take() {
            udp.socket.io.forget(tag, Ends::Once);
            let taken = core::mem::replace(&mut self.taken, 0);
            if let Some(budget) = udp.budget.as_deref() {
                budget.give_back(taken);
            }
        }
    }
}

impl Default for RecvArm {
    fn default() -> RecvArm {
        RecvArm::new()
    }
}

impl fmt::Debug for Datagrams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Datagrams({:?} at {})", self.socket.sock, self.local)
    }
}
