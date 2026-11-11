//! Streams to an origin (docs/v2/plan/design/std.md, "Capabilities are layered, and each has
//! providers", `Streams`): WebTransport's shape, a bidirectional stream of bytes opened to an
//! [`Origin`]. The capability to ask for above `Datagrams`, as [`crate::requests::Http`] is
//! above `Net`. Narrowed to one origin by [`Streams::only`], which never widens.
//!
//! **Providers.** Two of the three exist: [`Streams::handler`], a function you pass, and
//! [`crate::Io::streams`], the host's, which today is the simulation's alone
//! ([`SimStreams`]). The third, streams made by bstd itself over `quic` from a `Net`'s
//! datagram socket, a `Clock` and a `Roots`, is not written: blaze-core has no datagram
//! effects, so `Net` has no `Datagrams` to make it from.
//!
//! **Finishing.** A [`Stream`] is linear, finished with [`Stream::finish`], which tells the
//! peer no more will be sent.

use std::{collections::VecDeque, fmt, sync::Arc};

use crate::{
    Stopped,
    cx::Cx,
    hosted::{self, Refused},
    io::Io,
    mem::Bytes,
    narrow::Allowed,
    url::Origin,
};

/// What [`Streams::handler`] takes: what was sent on a stream to an origin in, the peer's
/// reply added to the bytes out.
pub type StreamHandler = dyn Fn(&Cx, &Origin, &[u8], &mut Bytes<'_>) -> Result<(), Stopped> + Send + Sync;

/// The simulated host's streams ([`crate::Io::streams`]).
#[derive(Debug, Default)]
pub struct SimStreams {
    /// Everything sent, in order, with the origin it was sent to.
    pub sent: Vec<(Origin, Vec<u8>)>,
    /// What the peers will send, in order: one a receive.
    pub replies: VecDeque<Vec<u8>>,
    /// How many streams were finished.
    pub finished: u32,
}

/// What a stream call was.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StreamsOp {
    Open,
    Send,
    Recv,
    Finish,
}

/// A stream call that failed: the call, the origin and how.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub struct StreamsError {
    pub op: StreamsOp,
    pub origin: Origin,
    pub kind: Refused,
}

impl fmt::Display for StreamsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "bstd::streams {:?} to {}: {}", self.op, self.origin, self.kind)
    }
}

impl std::error::Error for StreamsError {}

/// Who is at the other end.
#[derive(Clone)]
enum Provider {
    Host(Io),
    Handler(Arc<StreamHandler>),
}

/// Streams to origins: from [`crate::Io::streams`] or [`Streams::handler`].
#[derive(Clone)]
pub struct Streams {
    provider: Provider,
    allowed: Allowed<Origin>,
    plant: crate::Plant,
}

impl Streams {
    /// Streams the host carries for the program.
    pub(crate) fn host(io: Io) -> Streams {
        let plant = io.plant();
        Streams { provider: Provider::Host(io), allowed: Allowed::Any, plant }
    }

    /// Streams whose peer is `handler`: each send is answered by what it adds.
    pub fn handler(
        handler: impl Fn(&Cx, &Origin, &[u8], &mut Bytes<'_>) -> Result<(), Stopped> + Send + Sync + 'static,
    ) -> Streams {
        // alloc: once per capability, its handler.
        Streams { provider: Provider::Handler(Arc::new(handler)), allowed: Allowed::Any, plant: crate::Plant::None }
    }

    /// Only streams to `origin`, if this reaches it already.
    pub fn only(&self, origin: &Origin) -> Streams {
        // clone: authority held, narrowed: refcounts.
        Streams { provider: self.provider.clone(), allowed: self.allowed.one(origin, self.plant), plant: self.plant }
    }

    /// A stream opened to `origin`: finished with [`Stream::finish`].
    pub fn open(&self, cx: &Cx, origin: &Origin) -> Result<Stream, StreamsError> {
        // clone: an error's name of its origin, refcounts.
        let refuse = |kind| StreamsError { op: StreamsOp::Open, origin: origin.clone(), kind };
        if !self.allowed.allows(origin) {
            return Err(refuse(Refused::NotAllowed));
        }
        match &self.provider {
            Provider::Host(io) => {
                if let Err(kind) = hosted::call(io, cx, |_| Ok(())) {
                    return Err(refuse(kind));
                }
            }
            Provider::Handler(_) => {}
        }
        // clone: the stream's provider and origin, refcounts; alloc: none, an empty inbox.
        Ok(Stream { provider: self.provider.clone(), origin: origin.clone(), inbox: VecDeque::new() })
    }
}

impl fmt::Debug for Streams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let provider = match &self.provider {
            Provider::Host(_) => "the host's",
            Provider::Handler(_) => "a handler",
        };
        write!(f, "Streams({provider}, {:?})", self.allowed)
    }
}

/// A stream to an origin, finished with [`Stream::finish`]. Linear (see the module's doc); a
/// panic that drops one sends nothing more and tells no one.
#[blaze::linear]
#[must_use = "a stream is finished with finish(cx)"]
pub struct Stream {
    provider: Provider,
    origin: Origin,
    /// What a handler replied and a receive has not taken yet.
    inbox: VecDeque<u8>,
}

impl Stream {
    pub fn origin(&self) -> &Origin {
        &self.origin
    }

    fn refuse(&self, op: StreamsOp, kind: Refused) -> StreamsError {
        // clone: an error's name of its origin, refcounts.
        StreamsError { op, origin: self.origin.clone(), kind }
    }

    /// All of `bytes` sent, or an error: never a part said sent.
    pub fn send_all(&mut self, cx: &Cx, bytes: &[u8]) -> Result<(), StreamsError> {
        let sent = match &self.provider {
            Provider::Host(io) => hosted::call(io, cx, |host| {
                // clone: the simulated host's record: refcounts, and alloc: the bytes sent.
                host.streams.sent.push((self.origin.clone(), bytes.to_vec()));
                Ok(())
            }),
            Provider::Handler(handler) => {
                let mut reply = cx.bytes();
                match handler(cx, &self.origin, bytes, &mut reply) {
                    Ok(()) => {
                        // alloc: the stream's inbox, freed as it is received.
                        self.inbox.extend(reply.as_slice());
                        Ok(())
                    }
                    Err(stopped) => Err(Refused::Stopped(stopped)),
                }
            }
        };
        sent.map_err(|kind| self.refuse(StreamsOp::Send, kind))
    }

    /// The bytes the peer sent and this has not received, at most `most`, added to `into`; how
    /// many. From the host it waits for the peer's next send until `cx`'s deadline; from a
    /// handler, whose replies are all there once its send returned, 0 says there is no more.
    pub fn recv(&mut self, cx: &Cx, most: usize, into: &mut Bytes<'_>) -> Result<usize, StreamsError> {
        let received = match &self.provider {
            Provider::Host(io) => hosted::wait(io, cx, "bstd::streams recv", |host| {
                let reply = host.streams.replies.front_mut()?;
                let take = most.min(reply.len());
                let taken = into.try_extend(&reply[..take]).map(|()| take).map_err(Refused::Stopped);
                if taken.is_ok() {
                    reply.drain(..take);
                    if reply.is_empty() {
                        host.streams.replies.pop_front();
                    }
                }
                Some(taken)
            }),
            Provider::Handler(_) => {
                let take = most.min(self.inbox.len());
                let (front, _) = self.inbox.as_slices();
                let take = take.min(front.len());
                match into.try_extend(&front[..take]) {
                    Ok(()) => {
                        self.inbox.drain(..take);
                        Ok(take)
                    }
                    Err(stopped) => Err(Refused::Stopped(stopped)),
                }
            }
        };
        received.map_err(|kind| self.refuse(StreamsOp::Recv, kind))
    }

    /// The stream finished: the peer is told no more will be sent. What it sent and this never
    /// received is dropped. The one way a `Stream` ends.
    #[blaze::consumes]
    pub fn finish(self, cx: &Cx) -> Result<(), StreamsError> {
        let Stream { provider, origin, inbox: _ } = self;
        let finished = match &provider {
            Provider::Host(io) => hosted::call(io, cx, |host| {
                host.streams.finished += 1;
                Ok(())
            }),
            Provider::Handler(_) => Ok(()),
        };
        finished.map_err(|kind| StreamsError { op: StreamsOp::Finish, origin, kind })
    }
}

impl fmt::Debug for Stream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Stream(to {})", self.origin)
    }
}
