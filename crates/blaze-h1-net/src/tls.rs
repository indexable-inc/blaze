//! The byte stream a request runs over, and the seam where TLS goes.

use core::fmt;

use bstd::{
    cx::Cx,
    net::{Conn, HostPort, NetError},
};

/// A connection's bytes, plain or under TLS. Linear like the [`Conn`] it holds: it ends with
/// [`Stream::finish`].
pub trait Stream {
    /// Sends all of `bytes`.
    fn send_all(&mut self, cx: &Cx, bytes: &[u8]) -> Result<(), StreamError>;
    /// Receives at most `into.len()` bytes into `into`: how many, 0 once the peer is done.
    fn recv_into(&mut self, cx: &Cx, into: &mut [u8]) -> Result<usize, StreamError>;
    /// Ends the stream (TLS's close, then the connection's), and says how it went.
    fn finish(self, cx: &Cx) -> Result<(), StreamError>;
}

/// TLS over a connected [`Conn`], implemented outside this crate.
///
/// [`Tls::connect`] owns the `Conn` it is given: on success it lives in the stream, and on an
/// error the implementation has finished it already (a `Conn` is linear).
///
/// **The options** (for the owner to choose; none is written here):
///
/// - **macOS, Network.framework** (`nw_connection` with `nw_parameters_create_secure_tcp`): the
///   OS's current stack, TLS 1.3, the system trust store and its revocation. It owns the
///   socket, so it cannot run over bstd's `Conn`: it would be a second `Net` effect in
///   blaze-core (connect-with-TLS), not an implementation of this trait. Relax's choice
///   (via `objc_msgSend` to `NSURLSession`, which owns the whole request, too).
/// - **macOS, SecureTransport** (`SSLCreateContext`, `SSLSetIOFuncs`): runs over any byte
///   stream, so it fits this trait exactly, with `SecTrustEvaluateWithError` for the chain. It
///   is deprecated since macOS 10.15 and stops at TLS 1.2: fine for crates.io, GitHub and an
///   R2 bucket today, a liability later. Public API, no private symbol needed (rule 45).
/// - **Linux, the system libssl** (`libssl.so.3`, loaded at run time; memory BIOs, so the
///   engine is sans-IO and the driver moves its bytes over `Conn`): TLS 1.3 and the distro's
///   trust store; costs a `dlopen` and a hard dependency on the host having OpenSSL 3.
///   Relax's Linux path.
/// - **rustls**, which the tree carries today (the old `blaze-http` client, quinn): fits the
///   trait (its `Connection` is sans-IO), but is the third-party stack this work removes, and
///   pulls `ring` with it.
/// - **blaze's own TLS 1.3 client** (one suite, X25519 and AES-GCM or ChaCha20-Poly1305,
///   ECDSA P-256 and RSA-PSS verification, an X.509 chain walk over bstd's `Roots`): about
///   four thousand lines and the burden of owning a security protocol.
/// - **No TLS for records**: fetch remote-cache records over plain HTTP, since each is
///   trusted only after its signed statement, digest and size are checked anyway; crates.io
///   files are checked against the index's SHA-256. The index itself and git still need TLS.
pub trait Tls {
    type Stream: Stream;

    /// A TLS stream to `server` over `conn`, its certificate checked against `server`'s name.
    fn connect(&self, cx: &Cx, conn: Conn, server: &HostPort) -> Result<Self::Stream, StreamError>;
}

/// A TLS failure: the step, the engine's code and what it means.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TlsError {
    pub op: TlsOp,
    /// The engine's own code (an `OSStatus`, an OpenSSL error), 0 when it has none.
    pub code: i64,
    pub says: &'static str,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TlsOp {
    /// No TLS stack was given ([`NoTls`]).
    Unavailable,
    Handshake,
    /// The server's certificate did not check out for its name.
    Verify,
    Send,
    Recv,
    Finish,
}

impl fmt::Display for TlsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TLS {:?} failed (code {}): {}", self.op, self.code, self.says)
    }
}

/// A failure of a [`Stream`]: the connection's or its TLS's.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub enum StreamError {
    Net(NetError),
    Tls(TlsError),
}

impl fmt::Display for StreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StreamError::Net(e) => write!(f, "{e}"),
            StreamError::Tls(e) => write!(f, "{e}"),
        }
    }
}

/// A plain connection, for `http`.
#[blaze::linear]
pub struct Plain(pub Conn);

impl Stream for Plain {
    fn send_all(&mut self, cx: &Cx, bytes: &[u8]) -> Result<(), StreamError> {
        self.0.send_all(cx, bytes).map_err(StreamError::Net)
    }

    fn recv_into(&mut self, cx: &Cx, into: &mut [u8]) -> Result<usize, StreamError> {
        self.0.read_into(into).wait(cx).map_err(StreamError::Net)
    }

    #[blaze::consumes]
    fn finish(self, cx: &Cx) -> Result<(), StreamError> {
        self.0.finish(cx).map_err(StreamError::Net)
    }
}

/// No TLS stack: every `https` connection is finished and refused, [`TlsOp::Unavailable`].
#[derive(Clone, Copy, Debug)]
pub struct NoTls;

/// [`NoTls`] gives no stream; this type has no value.
pub enum NoStream {}

impl Stream for NoStream {
    fn send_all(&mut self, _: &Cx, _: &[u8]) -> Result<(), StreamError> {
        match *self {}
    }

    fn recv_into(&mut self, _: &Cx, _: &mut [u8]) -> Result<usize, StreamError> {
        match *self {}
    }

    fn finish(self, _: &Cx) -> Result<(), StreamError> {
        match self {}
    }
}

impl Tls for NoTls {
    type Stream = NoStream;

    fn connect(&self, cx: &Cx, conn: Conn, _server: &HostPort) -> Result<NoStream, StreamError> {
        conn.finish(cx).map_err(StreamError::Net)?;
        Err(StreamError::Tls(TlsError { op: TlsOp::Unavailable, code: 0, says: "no TLS stack was given to this client" }))
    }
}
