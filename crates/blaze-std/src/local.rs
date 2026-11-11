//! Local connections (docs/v2/plan/design/std.md, "The rest of the set", `Local`): pipes and
//! local sockets between tasks and processes.
//!
//! - [`pair`] makes a connected pair in memory, a [`Pair`] of two [`End`]s: what one writes
//!   the other reads, in order and once, through a bounded buffer each way. It needs no
//!   capability: it reaches nothing outside its own two values.
//! - [`Local`] is the capability to the named local sockets beneath one directory: made from a
//!   [`Dir`] ([`Local::beneath`]), narrowed to one socket's path by [`Local::only`], which
//!   never widens. Its connections and listeners are [`crate::net`]'s, over blaze-core's Unix
//!   socket effects, so it has a real provider wherever sockets do.
//!
//! **Finishing.** An [`End`] is linear: leaving one is a compile error, and it ends with
//! [`End::finish`], after which its peer's reads end with 0 and its peer's writes are refused.
//! A panic that unwinds past one closes it the same way in its `Drop`.
//!
//! **Waiting.** A read of an empty pipe and a write to a full one wait through the one wait
//! ([`Cx::wait_for`]), each in a span naming it, until `cx`'s deadline. In S1's `Inline`
//! layout nothing else runs while a task waits, so such a wait ends at its deadline unless
//! the bytes or the room are there already; under a fiber layout the other end's task wakes
//! it.

use alloc::{collections::VecDeque, sync::Arc};
use core::{fmt, num::NonZero};

use blaze_core::lock::Lock;

use crate::{
    Plant, Stopped,
    cx::Cx,
    fs::{Dir, ReadOnly, Rights},
    mem::Bytes,
    narrow::Allowed,
    net::{self, Addr, Conn, Listener, Net, NetError, NetKind, NetOp},
    path::Path,
};

/// What a call on an [`End`] was.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LocalOp {
    Read,
    Write,
}

#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub enum LocalKind {
    /// The other end was finished: nothing more can be written to it.
    Closed,
    /// The wait stopped first.
    Stopped(Stopped),
}

/// A call on an [`End`] that failed: the call, the pair's name, how, and how many bytes it had
/// moved by then (a write that stops half way has written those, once).
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub struct LocalError {
    pub op: LocalOp,
    pub pair: &'static str,
    pub kind: LocalKind,
    pub moved: usize,
}

impl fmt::Display for LocalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (op, pair, moved) = (self.op, self.pair, self.moved);
        match &self.kind {
            LocalKind::Closed => {
                write!(f, "bstd::local {op:?} on pair {pair}: the other end was finished ({moved} bytes moved)")
            }
            LocalKind::Stopped(stopped) => write!(f, "bstd::local {op:?} on pair {pair}: {stopped} ({moved} bytes moved)"),
        }
    }
}

impl core::error::Error for LocalError {}

/// One direction of a pair: the bytes written and not yet read.
struct Pipe {
    bytes: VecDeque<u8>,
    capacity: usize,
    /// The writing end was finished: a read of what is left, then 0.
    writer_gone: bool,
    /// The reading end was finished: a write is refused.
    reader_gone: bool,
}

/// What a pair's two ends share.
struct Link {
    name: &'static str,
    /// From the first end to the second.
    forth: Lock<Pipe>,
    /// From the second end to the first.
    back: Lock<Pipe>,
}

/// One end of an in-memory pair, finished with [`End::finish`]. Linear (see the module's doc).
#[blaze::linear]
#[must_use = "an end of a local pair is finished with finish(cx)"]
pub struct End {
    half: Half,
}

/// An end's share of the pair and its abandonment path.
struct Half {
    link: Arc<Link>,
    first: bool,
    /// Whether it is still to be closed: cleared by [`End::finish`].
    open: bool,
}

impl Half {
    /// The pipe this end writes to.
    fn out(&self) -> &Lock<Pipe> {
        if self.first { &self.link.forth } else { &self.link.back }
    }

    /// The pipe this end reads from.
    fn inward(&self) -> &Lock<Pipe> {
        if self.first { &self.link.back } else { &self.link.forth }
    }

    fn close(&mut self) {
        self.open = false;
        self.out().lock().writer_gone = true;
        self.inward().lock().reader_gone = true;
    }
}

impl Drop for Half {
    fn drop(&mut self) {
        if self.open {
            self.close();
        }
    }
}

/// A connected pair made in memory: what `first` writes `second` reads, and the reverse.
/// Take it apart (`let Pair { first, second } = pair;`) and finish each end.
#[must_use = "each end of a local pair is finished with finish(cx)"]
pub struct Pair {
    pub first: End,
    pub second: End,
}

/// A connected pair in memory named `name`, each direction holding at most `capacity` bytes
/// written and not yet read: the pair's one buffer, sized by its maker and never grown. A
/// capacity of zero cannot be asked for (it was once raised to one unsaid). The two buffers
/// are made in the global heap, once, here.
pub fn pair(name: &'static str, capacity: NonZero<usize>) -> Pair {
    let capacity = capacity.get();
    let pipe = |lock: &'static str| {
        // alloc: once per pair, a direction's buffer, never grown past its capacity.
        let bytes = VecDeque::with_capacity(capacity);
        Lock::new(lock, Pipe { bytes, capacity, writer_gone: false, reader_gone: false })
    };
    // alloc: once per pair, what its two ends share.
    let link = Arc::new(Link { name, forth: pipe("bstd.local.forth"), back: pipe("bstd.local.back") });
    let first = End { half: Half { link: Arc::clone(&link), first: true, open: true } };
    let second = End { half: Half { link, first: false, open: true } };
    Pair { first, second }
}

impl End {
    /// The pair's name.
    pub fn pair(&self) -> &'static str {
        self.half.link.name
    }

    /// All of `bytes` written, in order, each byte once, or an error that says how many
    /// bytes it had written by then ([`LocalError::moved`]): never a part said whole. Where
    /// the pipe is full it waits for the other end to read, until `cx`'s deadline.
    pub fn write_all(&self, cx: &Cx, bytes: &[u8]) -> Result<(), LocalError> {
        let name = self.half.link.name;
        let refuse = |kind, moved| LocalError { op: LocalOp::Write, pair: name, kind, moved };
        let io = cx.io();
        cx.stopped_by(io.now()).map_err(|stopped| refuse(LocalKind::Stopped(stopped), 0))?;
        let cx = cx.span("bstd::local write_all: room in the pipe");
        let drops = cx.plant() == Plant::LocalDropsChunk;
        let mut moved = 0;
        let waited = cx.wait_for(io, || {
            let mut pipe = self.half.out().lock();
            if pipe.reader_gone {
                return Some(Err(LocalKind::Closed));
            }
            let room = pipe.capacity - pipe.bytes.len();
            let take = room.min(bytes.len() - moved);
            // The planted bug: a chunk written behind unread bytes is said written and lost.
            if !(drops && !pipe.bytes.is_empty()) {
                pipe.bytes.extend(&bytes[moved..moved + take]);
            }
            moved += take;
            (moved == bytes.len()).then_some(Ok(()))
        });
        match waited {
            Ok(Ok(())) => Ok(()),
            Ok(Err(kind)) => Err(refuse(kind, moved)),
            Err(stopped) => Err(refuse(LocalKind::Stopped(stopped), moved)),
        }
    }

    /// The bytes the other end wrote and this has not read, at most `most`, added to `into`;
    /// how many, 0 once the other end was finished and everything it wrote is read. Where the
    /// pipe is empty it waits for the other end to write, until `cx`'s deadline.
    pub fn read(&self, cx: &Cx, most: usize, into: &mut Bytes<'_>) -> Result<usize, LocalError> {
        let name = self.half.link.name;
        let refuse = |stopped| LocalError { op: LocalOp::Read, pair: name, kind: LocalKind::Stopped(stopped), moved: 0 };
        let io = cx.io();
        cx.stopped_by(io.now()).map_err(refuse)?;
        if most == 0 {
            return Ok(0);
        }
        // Room is made before the pipe's lock is taken: nothing allocates under it.
        into.try_reserve(most).map_err(refuse)?;
        let cx = cx.span("bstd::local read: bytes in the pipe");
        let waited = cx.wait_for(io, || {
            let mut pipe = self.half.inward().lock();
            let take = most.min(pipe.bytes.len());
            if take == 0 {
                return pipe.writer_gone.then_some(Ok(0));
            }
            let slot = match into.grow_zeroed(take) {
                Ok(slot) => slot,
                Err(stopped) => return Some(Err(stopped)),
            };
            for (to, byte) in slot.iter_mut().zip(pipe.bytes.drain(..take)) {
                *to = byte;
            }
            Some(Ok(take))
        });
        match waited {
            Ok(Ok(read)) => Ok(read),
            Ok(Err(stopped)) | Err(stopped) => Err(refuse(stopped)),
        }
    }

    /// The end finished: the other end's reads give what was written, then 0, and its writes
    /// are refused. What the other end wrote and this never read is dropped. It waits for
    /// nothing. The one way an `End` ends.
    #[blaze::consumes]
    pub fn finish(mut self, cx: &Cx) {
        cx.event("bstd::local: an end finished");
        self.half.close();
    }
}

impl fmt::Debug for End {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let which = if self.half.first { "first" } else { "second" };
        write!(f, "End({which} of {})", self.half.link.name)
    }
}

/// The named local sockets beneath one directory: from [`Local::beneath`].
#[derive(Clone)]
pub struct Local {
    dir: Dir<ReadOnly>,
    /// Whether it may make a socket (its directory was writable): else it only connects.
    listens: bool,
    allowed: Allowed<Path>,
}

impl Local {
    /// The sockets beneath `dir`: connecting to them, and listening on them where `dir` is
    /// writable (a listen makes the socket's file).
    pub fn beneath<#[blaze::linear] P: Rights>(dir: &Dir<P>) -> Local {
        Local { dir: dir.read_only(), listens: P::WRITE, allowed: Allowed::Any }
    }

    /// Only the socket at `path`, if this reaches it already.
    pub fn only(&self, path: &Path) -> Local {
        // clone: authority held, narrowed: refcounts.
        Local { dir: self.dir.clone(), listens: self.listens, allowed: self.allowed.one(path, self.dir.io().plant()) }
    }

    /// Connects only, to the sockets this reaches.
    pub fn connect_only(&self) -> Local {
        // clone: authority held, narrowed: refcounts.
        Local { dir: self.dir.clone(), listens: false, allowed: self.allowed.clone() }
    }

    /// The socket at `path` as an address, and the network that reaches only it.
    fn socket(&self, path: &Path) -> (Addr, Net) {
        (Addr::Unix(self.dir.path_beneath(path)), Net::new(self.dir.io().clone()))
    }

    /// A connection to the socket at `path`: finished with [`Conn::finish`].
    pub fn connect(&self, cx: &Cx, path: &Path) -> Result<Conn, NetError> {
        let (addr, net) = self.socket(path);
        if !self.allowed.allows(path) {
            return Err(net::error(NetOp::Connect, &addr, NetKind::NotAllowed));
        }
        net.connect(cx, &addr)
    }

    /// Listens on the socket at `path`, making it: finished with [`Listener::close`].
    pub fn listen(&self, cx: &Cx, path: &Path) -> Result<Listener, NetError> {
        let (addr, net) = self.socket(path);
        if !self.listens || !self.allowed.allows(path) {
            return Err(net::error(NetOp::Listen, &addr, NetKind::NotAllowed));
        }
        net.listen(cx, &addr)
    }
}

impl fmt::Debug for Local {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Local(beneath {}, listens: {}, {:?})", self.dir.name(), self.listens, self.allowed)
    }
}
