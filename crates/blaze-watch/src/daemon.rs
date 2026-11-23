//! The resident per-checkout `blaze` daemon's core state machine and wire protocol
//! (Wave 7, rule 14 & rule 70): sans-IO, `#![no_std]`, deterministic CBOR framing over a Unix
//! domain socket under `<HOME>/.blaze/daemon/<16-hex(CheckoutKey)>.sock`.
//!
//! **What lives here.**
//! - Socket path derivation ([`socket_path`], [`SOCKET_FOLDER`]): 16 hex digits of the checkout
//!   key under `daemon/`, never a checkout path, so socket paths never exceed `sun_path`.
//! - Deterministic CBOR framing ([`frame_len`], [`Hello`], [`ClientFrame`], [`ServerFrame`],
//!   [`CheckReq`], [`CheckReqRef`], [`CannotServe`]) capped at [`MOST_FRAME`].
//! - [`DaemonCore`]: lifecycle state machine enforcing rule 70 — exact binary [`Version`] and
//!   [`CheckoutKey`] checks, [`IDLE_EXPIRY`] (30 min), [`MAX_LIFE`] (12 h),
//!   [`MAX_PHASE_REBUILDS`] (32), executable replacement, checkout removal, and clean shutdown.
//!
//! Nothing here touches the OS, a clock, randomness, or the global allocator.

#![blaze::sans_io]

use core::alloc::Allocator;
use core::fmt;

use blaze_own::{AllocFailure, Vec};

use crate::Nanos;
use crate::cbor;
use crate::helper::{CheckoutKey, Version};

/// The daemon wire protocol's form: a frame of any other form is refused whole.
pub const FORM: u8 = 1;

/// The sockets' folder, relative to blaze's folder in the home (`<HOME>/.blaze`).
pub const SOCKET_FOLDER: &str = "daemon";

/// Bytes of a socket's file name in [`SOCKET_FOLDER`]: 16 hex digits + `.sock`.
pub const SOCKET_NAME_BYTES: usize = 16 + ".sock".len();

/// Bytes of [`socket_path`]: `<SOCKET_FOLDER>/<16-hex>.sock`.
pub const SOCKET_PATH_BYTES: usize = SOCKET_FOLDER.len() + 1 + SOCKET_NAME_BYTES;

/// Default idle timeout before the resident daemon exits cleanly (30 minutes, rule 70).
pub const IDLE_EXPIRY: Nanos = Nanos::secs(30 * 60);

/// Default maximum wall lifetime of a resident daemon process (12 hours, rule 70).
pub const MAX_LIFE: Nanos = Nanos::secs(12 * 60 * 60);

/// Default cap on full phase-path rebuilds served before the daemon exits cleanly so rare
/// phase-path memory growth self-heals.
pub const MAX_PHASE_REBUILDS: u32 = 32;

/// Maximum bytes of one framed CBOR body (after its 4-byte little-endian length prefix).
pub const MOST_FRAME: usize = 4 << 20;

/// Maximum bytes of a `cwd` byte string in [`CheckReq`].
pub const MOST_CWD: usize = 4096;

/// Maximum number of `argv` elements in [`CheckReq`] (bounded by [`MOST_FRAME`]).
pub const MOST_ARGV: usize = MOST_FRAME;

/// Maximum bytes of one `argv` element in [`CheckReq`] (bounded by [`MOST_FRAME`], preserving
/// Linux's 128 KiB+ `MAX_ARG_STRLEN`).
pub const MOST_ARG_BYTES: usize = MOST_FRAME;

/// Maximum number of `env` `(key, value)` pairs in [`CheckReq`] (bounded by [`MOST_FRAME`]).
pub const MOST_ENV: usize = MOST_FRAME;

/// Maximum bytes of one `env` key or value in [`CheckReq`] (bounded by [`MOST_FRAME`]).
pub const MOST_ENV_BYTES: usize = MOST_FRAME;

/// Maximum bytes of one [`ServerFrame::Out`] or [`ServerFrame::Err`] payload chunk.
pub const MOST_CHUNK: usize = 1 << 20;

const KIND_HELLO: u64 = 0;
const KIND_CHECK: u64 = 1;
const KIND_SIGNAL: u64 = 2;
const KIND_SHUTDOWN: u64 = 3;
const KIND_OUT: u64 = 4;
const KIND_ERR: u64 = 5;
const KIND_DONE: u64 = 6;
const KIND_REFUSED: u64 = 7;

/// The checkout's daemon socket path, relative to `<HOME>/.blaze`:
/// `daemon/<first 8 bytes of CheckoutKey in lowercase hex>.sock`.
pub fn socket_path(key: &CheckoutKey) -> [u8; SOCKET_PATH_BYTES] {
    const HEX: [u8; 16] = *b"0123456789abcdef";
    let mut out = [0u8; SOCKET_PATH_BYTES];
    let mut pos = 0usize;
    for &byte in SOCKET_FOLDER.as_bytes() {
        if let Some(slot) = out.get_mut(pos) {
            *slot = byte;
        }
        pos += 1;
    }
    if let Some(slot) = out.get_mut(pos) {
        *slot = b'/';
    }
    pos += 1;
    for &byte in key.0.iter().take(8) {
        let hi = HEX.get(usize::from(byte >> 4)).copied().unwrap_or(b'0');
        let lo = HEX.get(usize::from(byte & 0xf)).copied().unwrap_or(b'0');
        if let Some(slot) = out.get_mut(pos) {
            *slot = hi;
        }
        if let Some(slot) = out.get_mut(pos + 1) {
            *slot = lo;
        }
        pos += 2;
    }
    for &byte in b".sock" {
        if let Some(slot) = out.get_mut(pos) {
            *slot = byte;
        }
        pos += 1;
    }
    out
}

/// Why a daemon cannot serve a client request.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CannotServe {
    /// The client's binary build id differs from this daemon's; this daemon marks itself
    /// [`Death::Replaced`] and exits so the client's build takes over.
    OtherVersion,
    /// The client's checkout key differs from this daemon's (e.g. 64-bit socket name prefix
    /// collision).
    OtherCheckout,
    /// The checkout directory is gone or moved.
    CheckoutGone,
    /// The daemon is already shutting down (idle, max-life, phase-limit, or explicit shutdown).
    ShuttingDown,
    /// Another request is currently executing in this daemon.
    Busy,
}

impl fmt::Display for CannotServe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            CannotServe::OtherVersion => "the resident daemon is of another build",
            CannotServe::OtherCheckout => "the resident daemon serves another checkout",
            CannotServe::CheckoutGone => "the checkout directory is gone",
            CannotServe::ShuttingDown => "the resident daemon is shutting down",
            CannotServe::Busy => "the resident daemon is busy serving another request",
        })
    }
}

const fn cannot_serve_code(why: CannotServe) -> u64 {
    match why {
        CannotServe::OtherVersion => 0,
        CannotServe::OtherCheckout => 1,
        CannotServe::CheckoutGone => 2,
        CannotServe::ShuttingDown => 3,
        CannotServe::Busy => 4,
    }
}

fn cannot_serve_of(code: u64) -> Result<CannotServe, WireError> {
    match code {
        0 => Ok(CannotServe::OtherVersion),
        1 => Ok(CannotServe::OtherCheckout),
        2 => Ok(CannotServe::CheckoutGone),
        3 => Ok(CannotServe::ShuttingDown),
        4 => Ok(CannotServe::Busy),
        _ => Err(WireError::Field("an unknown CannotServe code")),
    }
}

/// Why the resident daemon decided to exit (rule 70).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Death {
    /// No request ran for [`DaemonConfig::idle_expiry`].
    Idle,
    /// Total lifetime reached [`DaemonConfig::max_life`].
    MaxLife,
    /// Own executable was replaced on disk or a client of another [`Version`] connected.
    Replaced,
    /// The checkout directory was removed or moved.
    CheckoutGone,
    /// Reached [`DaemonConfig::max_phase_rebuilds`] full phase rebuilds.
    PhaseLimit,
    /// An explicit shutdown frame or signal requested exit.
    Shutdown,
}

impl fmt::Display for Death {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Death::Idle => "idle expiry elapsed",
            Death::MaxLife => "maximum daemon lifetime reached",
            Death::Replaced => "daemon binary replaced or client version mismatch",
            Death::CheckoutGone => "checkout directory removed or moved",
            Death::PhaseLimit => "phase rebuild cap reached",
            Death::Shutdown => "shutdown requested",
        })
    }
}

/// Why a wire frame was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WireError {
    Cbor(cbor::Error),
    /// The frame's form is not [`FORM`].
    Form(u64),
    /// The frame's kind is not expected here.
    Kind(u64),
    /// A field value violated protocol invariants.
    Field(&'static str),
    /// The frame length exceeds [`MOST_FRAME`].
    Length(u64),
    /// The caller's allocator refused memory while decoding variable-length lists.
    NoMemory(AllocFailure),
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WireError::Cbor(error) => write!(f, "daemon frame: {error}"),
            WireError::Form(form) => write!(f, "daemon frame has form {form}, expected {FORM}"),
            WireError::Kind(kind) => write!(f, "daemon frame has unexpected kind {kind}"),
            WireError::Field(what) => write!(f, "daemon frame: {what}"),
            WireError::Length(len) => write!(f, "daemon frame length {len} exceeds cap {MOST_FRAME}"),
            WireError::NoMemory(why) => write!(f, "daemon frame decode: {why}"),
        }
    }
}

impl core::error::Error for WireError {}

impl From<cbor::Error> for WireError {
    fn from(error: cbor::Error) -> WireError {
        WireError::Cbor(error)
    }
}

impl From<AllocFailure> for WireError {
    fn from(why: AllocFailure) -> WireError {
        WireError::NoMemory(why)
    }
}

/// The daemon's initial greeting sent on every accepted connection.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Hello {
    pub form: u8,
    pub version: Version,
    pub instance: u64,
    pub checkout: CheckoutKey,
}

impl Hello {
    /// Constructs a [`Hello`] with the current protocol [`FORM`].
    pub const fn new(version: Version, instance: u64, checkout: CheckoutKey) -> Hello {
        Hello { form: FORM, version, instance, checkout }
    }

    /// Appends a length-prefixed frame `[form, KIND_HELLO, version, instance, checkout]`.
    pub fn encode<A: Allocator>(&self, out: &mut Vec<u8, A>) {
        framed(out, |out| {
            cbor::array(out, 5);
            cbor::uint(out, u64::from(self.form));
            cbor::uint(out, KIND_HELLO);
            cbor::bytes(out, self.version.as_bytes());
            cbor::uint(out, self.instance);
            cbor::bytes(out, &self.checkout.0);
        });
    }

    /// Decodes a [`Hello`] from a frame body (after its 4-byte length prefix).
    pub fn decode(body: &[u8]) -> Result<Hello, WireError> {
        check_body_len(body)?;
        let mut reader = cbor::Reader::new(body);
        reader.array_of(5)?;
        check_form(&mut reader)?;
        let kind = reader.uint()?;
        if kind != KIND_HELLO {
            return Err(WireError::Kind(kind));
        }
        let version = read_version(&mut reader)?;
        let instance = reader.uint()?;
        let checkout = CheckoutKey(reader.fixed::<32>()?);
        reader.end()?;
        Ok(Hello { form: FORM, version, instance, checkout })
    }
}

/// A borrowed view of a check request for zero-allocation encoding on the client side.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CheckReqRef<'a> {
    pub version: Version,
    pub checkout: CheckoutKey,
    pub cwd: &'a [u8],
    pub argv: &'a [&'a [u8]],
    pub stdout_tty: bool,
    pub stderr_tty: bool,
    pub cols: Option<u16>,
    pub env: &'a [(&'a [u8], &'a [u8])],
    pub client_start_mono_ns: u64,
}

impl CheckReqRef<'_> {
    /// Appends a length-prefixed `ClientFrame::Check` frame.
    pub fn encode<A: Allocator>(&self, out: &mut Vec<u8, A>) {
        framed(out, |out| {
            cbor::array(out, 11);
            cbor::uint(out, u64::from(FORM));
            cbor::uint(out, KIND_CHECK);
            cbor::bytes(out, self.version.as_bytes());
            cbor::bytes(out, &self.checkout.0);
            cbor::bytes(out, self.cwd);
            cbor::array(out, u64::try_from(self.argv.len()).unwrap_or(u64::MAX));
            for arg in self.argv {
                cbor::bytes(out, arg);
            }
            cbor::boolean(out, self.stdout_tty);
            cbor::boolean(out, self.stderr_tty);
            cbor::uint(out, self.cols.map_or(0, |c| u64::from(c) + 1));
            cbor::array(out, u64::try_from(self.env.len()).unwrap_or(u64::MAX));
            for &(k, v) in self.env {
                cbor::array(out, 2);
                cbor::bytes(out, k);
                cbor::bytes(out, v);
            }
            cbor::uint(out, self.client_start_mono_ns);
        });
    }
}

/// A decoded check request borrowing all byte slices from the frame body, with its `argv` and
/// `env` lists stored in the caller's allocator `A`.
#[derive(Debug)]
pub struct CheckReq<'a, A: Allocator> {
    pub version: Version,
    pub checkout: CheckoutKey,
    pub cwd: &'a [u8],
    pub argv: Vec<&'a [u8], A>,
    pub stdout_tty: bool,
    pub stderr_tty: bool,
    pub cols: Option<u16>,
    pub env: Vec<(&'a [u8], &'a [u8]), A>,
    pub client_start_mono_ns: u64,
}

impl<A: Allocator, B: Allocator> PartialEq<CheckReq<'_, B>> for CheckReq<'_, A> {
    fn eq(&self, other: &CheckReq<'_, B>) -> bool {
        self.as_ref() == other.as_ref()
    }
}

impl<A: Allocator> Eq for CheckReq<'_, A> {}

impl<'a, A: Allocator> CheckReq<'a, A> {
    /// Borrows this request as a [`CheckReqRef`] for encoding.
    pub fn as_ref(&self) -> CheckReqRef<'_> {
        CheckReqRef {
            version: self.version,
            checkout: self.checkout,
            cwd: self.cwd,
            argv: self.argv.as_slice(),
            stdout_tty: self.stdout_tty,
            stderr_tty: self.stderr_tty,
            cols: self.cols,
            env: self.env.as_slice(),
            client_start_mono_ns: self.client_start_mono_ns,
        }
    }

    /// Appends a length-prefixed `ClientFrame::Check` frame.
    pub fn encode<B: Allocator>(&self, out: &mut Vec<u8, B>) {
        self.as_ref().encode(out);
    }
}

/// Frames sent from a `blaze` client to the resident daemon.
#[derive(Debug)]
pub enum ClientFrame<'a, A: Allocator> {
    Check(CheckReq<'a, A>),
    Signal(u8),
    Shutdown,
}

impl<A: Allocator, B: Allocator> PartialEq<ClientFrame<'_, B>> for ClientFrame<'_, A> {
    fn eq(&self, other: &ClientFrame<'_, B>) -> bool {
        match (self, other) {
            (ClientFrame::Check(a), ClientFrame::Check(b)) => a == b,
            (ClientFrame::Signal(a), ClientFrame::Signal(b)) => a == b,
            (ClientFrame::Shutdown, ClientFrame::Shutdown) => true,
            _ => false,
        }
    }
}

impl<A: Allocator> Eq for ClientFrame<'_, A> {}

impl<'a, A: Allocator> ClientFrame<'a, A> {
    /// Appends this frame (with its 4-byte little-endian length prefix) to `out`.
    pub fn encode<B: Allocator>(&self, out: &mut Vec<u8, B>) {
        match self {
            ClientFrame::Check(req) => req.encode(out),
            ClientFrame::Signal(sig) => encode_signal(*sig, out),
            ClientFrame::Shutdown => encode_shutdown(out),
        }
    }
}

impl<'a, A: Allocator + Copy> ClientFrame<'a, A> {
    /// Decodes a [`ClientFrame`] from a frame body (after its 4-byte length prefix).
    pub fn decode_in(body: &'a [u8], alloc: A) -> Result<ClientFrame<'a, A>, WireError> {
        check_body_len(body)?;
        let mut reader = cbor::Reader::new(body);
        let items = reader.array(11, "a client frame's fields")?;
        if items < 2 {
            return Err(WireError::Field("a client frame with fewer than 2 items"));
        }
        check_form(&mut reader)?;
        let frame = match (reader.uint()?, items) {
            (KIND_CHECK, 11) => {
                let version = read_version(&mut reader)?;
                let checkout = CheckoutKey(reader.fixed::<32>()?);
                let cwd = reader.bytes(MOST_CWD, "cwd past cap")?;
                let argc = reader.array(body.len(), "argv count past frame size")?;
                let mut argv = Vec::try_with_capacity_in(argc, alloc)?;
                for _ in 0..argc {
                    argv.push(reader.bytes(body.len(), "argv item past frame size")?);
                }
                let stdout_tty = reader.boolean()?;
                let stderr_tty = reader.boolean()?;
                let raw_cols = reader.uint_most(u64::from(u16::MAX) + 1, "cols past u16::MAX")?;
                let cols = match raw_cols.checked_sub(1) {
                    None => None,
                    Some(n) => Some(u16::try_from(n).map_err(|_| WireError::Field("cols past u16::MAX"))?),
                };
                let envc = reader.array(body.len(), "env count past frame size")?;
                let mut env = Vec::try_with_capacity_in(envc, alloc)?;
                for _ in 0..envc {
                    reader.array_of(2)?;
                    let k = reader.bytes(body.len(), "env key past frame size")?;
                    let v = reader.bytes(body.len(), "env value past frame size")?;
                    env.push((k, v));
                }
                let client_start_mono_ns = reader.uint()?;
                ClientFrame::Check(CheckReq {
                    version,
                    checkout,
                    cwd,
                    argv,
                    stdout_tty,
                    stderr_tty,
                    cols,
                    env,
                    client_start_mono_ns,
                })
            }
            (KIND_SIGNAL, 3) => {
                let raw = reader.uint_most(u64::from(u8::MAX), "signal number past u8::MAX")?;
                let sig = u8::try_from(raw).map_err(|_| WireError::Field("signal number past u8::MAX"))?;
                ClientFrame::Signal(sig)
            }
            (KIND_SHUTDOWN, 2) => ClientFrame::Shutdown,
            (kind, _) => return Err(WireError::Kind(kind)),
        };
        reader.end()?;
        Ok(frame)
    }
}

/// Appends a length-prefixed `ClientFrame::Signal` frame without constructing a `ClientFrame`.
pub fn encode_signal<A: Allocator>(sig: u8, out: &mut Vec<u8, A>) {
    framed(out, |out| {
        cbor::array(out, 3);
        cbor::uint(out, u64::from(FORM));
        cbor::uint(out, KIND_SIGNAL);
        cbor::uint(out, u64::from(sig));
    });
}

/// Appends a length-prefixed `ClientFrame::Shutdown` frame without constructing a `ClientFrame`.
pub fn encode_shutdown<A: Allocator>(out: &mut Vec<u8, A>) {
    framed(out, |out| {
        cbor::array(out, 2);
        cbor::uint(out, u64::from(FORM));
        cbor::uint(out, KIND_SHUTDOWN);
    });
}

/// Encodes the length-prefixed CBOR header for [`ServerFrame::Out`] (`is_err == false`) or
/// [`ServerFrame::Err`] (`is_err == true`) preceding `payload_len` raw bytes, allowing callers to
/// stream large stdout/stderr slices via vectored send without copying the payload into a frame
/// buffer.
pub fn encode_chunk_header(is_err: bool, payload_len: usize) -> ([u8; 16], usize) {
    let mut out = [0u8; 16];
    let arr = cbor::Head::new(cbor::major::ARRAY, 3);
    let form = cbor::Head::new(cbor::major::UINT, u64::from(FORM));
    let kind = cbor::Head::new(cbor::major::UINT, if is_err { KIND_ERR } else { KIND_OUT });
    let bytes_head = cbor::Head::new(cbor::major::BYTES, u64::try_from(payload_len).unwrap_or(u64::MAX));
    let mut pos = 4usize;
    for part in [arr.as_bytes(), form.as_bytes(), kind.as_bytes(), bytes_head.as_bytes()] {
        if let Some(slot) = out.get_mut(pos..pos + part.len()) {
            slot.copy_from_slice(part);
        }
        pos += part.len();
    }
    let body_len = u32::try_from((pos - 4).saturating_add(payload_len)).unwrap_or(u32::MAX);
    out[..4].copy_from_slice(&body_len.to_le_bytes());
    (out, pos)
}

/// Frames sent from the resident daemon back to a `blaze` client.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ServerFrame<'a> {
    Out(&'a [u8]),
    Err(&'a [u8]),
    Done { status: u8 },
    Refused { why: CannotServe },
}

impl<'a> ServerFrame<'a> {
    /// Appends this frame (with its 4-byte little-endian length prefix) to `out`.
    pub fn encode<A: Allocator>(&self, out: &mut Vec<u8, A>) {
        framed(out, |out| match *self {
            ServerFrame::Out(bytes) => {
                cbor::array(out, 3);
                cbor::uint(out, u64::from(FORM));
                cbor::uint(out, KIND_OUT);
                cbor::bytes(out, bytes);
            }
            ServerFrame::Err(bytes) => {
                cbor::array(out, 3);
                cbor::uint(out, u64::from(FORM));
                cbor::uint(out, KIND_ERR);
                cbor::bytes(out, bytes);
            }
            ServerFrame::Done { status } => {
                cbor::array(out, 3);
                cbor::uint(out, u64::from(FORM));
                cbor::uint(out, KIND_DONE);
                cbor::uint(out, u64::from(status));
            }
            ServerFrame::Refused { why } => {
                cbor::array(out, 3);
                cbor::uint(out, u64::from(FORM));
                cbor::uint(out, KIND_REFUSED);
                cbor::uint(out, cannot_serve_code(why));
            }
        });
    }

    /// Decodes a [`ServerFrame`] from a frame body (after its 4-byte length prefix).
    pub fn decode(body: &'a [u8]) -> Result<ServerFrame<'a>, WireError> {
        check_body_len(body)?;
        let mut reader = cbor::Reader::new(body);
        reader.array_of(3)?;
        check_form(&mut reader)?;
        let frame = match reader.uint()? {
            KIND_OUT => ServerFrame::Out(reader.bytes(MOST_CHUNK, "stdout chunk past cap")?),
            KIND_ERR => ServerFrame::Err(reader.bytes(MOST_CHUNK, "stderr chunk past cap")?),
            KIND_DONE => {
                let raw = reader.uint_most(u64::from(u8::MAX), "exit status past u8::MAX")?;
                let status = u8::try_from(raw).map_err(|_| WireError::Field("exit status past u8::MAX"))?;
                ServerFrame::Done { status }
            }
            KIND_REFUSED => ServerFrame::Refused { why: cannot_serve_of(reader.uint()?)? },
            kind => return Err(WireError::Kind(kind)),
        };
        reader.end()?;
        Ok(frame)
    }
}

/// Reads the 4-byte little-endian length prefix of a frame: `None` if fewer than 4 bytes are
/// available, `Some(Err(WireError::Length))` if the declared length exceeds [`MOST_FRAME`].
pub fn frame_len(head: &[u8]) -> Option<Result<usize, WireError>> {
    frame_len_planted(head, DaemonPlant::None)
}

fn frame_len_planted(head: &[u8], plant: DaemonPlant) -> Option<Result<usize, WireError>> {
    let word = u32::from_le_bytes(head.get(..4)?.try_into().ok()?);
    let len = usize::try_from(word).unwrap_or(usize::MAX);
    if len > MOST_FRAME && plant != DaemonPlant::IgnoresFrameCap {
        Some(Err(WireError::Length(u64::from(word))))
    } else {
        Some(Ok(len))
    }
}

fn framed<A: Allocator>(out: &mut Vec<u8, A>, body: impl FnOnce(&mut Vec<u8, A>)) {
    let at = out.len();
    out.extend_from_slice(&[0; 4]);
    body(out);
    let len = u32::try_from(out.len().saturating_sub(at + 4)).unwrap_or(u32::MAX);
    if let Some(head) = out.get_mut(at..at + 4) {
        head.copy_from_slice(&len.to_le_bytes());
    }
}

fn check_body_len(body: &[u8]) -> Result<(), WireError> {
    if body.len() > MOST_FRAME {
        return Err(WireError::Length(u64::try_from(body.len()).unwrap_or(u64::MAX)));
    }
    Ok(())
}

fn check_form(reader: &mut cbor::Reader<'_>) -> Result<(), WireError> {
    let form = reader.uint()?;
    if form != u64::from(FORM) {
        return Err(WireError::Form(form));
    }
    Ok(())
}

fn read_version(reader: &mut cbor::Reader<'_>) -> Result<Version, WireError> {
    Version::of(reader.bytes(32, "a version past 32 bytes")?).ok_or(WireError::Field("an empty version"))
}

/// Tunable bounds of a [`DaemonCore`] instance (rule 70).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DaemonConfig {
    pub idle_expiry: Nanos,
    pub max_life: Nanos,
    pub max_phase_rebuilds: u32,
}

impl DaemonConfig {
    /// Production defaults: 30 min idle, 12 h max life, 32 phase rebuilds.
    pub const SHIPPED: DaemonConfig = DaemonConfig {
        idle_expiry: IDLE_EXPIRY,
        max_life: MAX_LIFE,
        max_phase_rebuilds: MAX_PHASE_REBUILDS,
    };
}

impl Default for DaemonConfig {
    fn default() -> DaemonConfig {
        DaemonConfig::SHIPPED
    }
}

/// Planted bugs for [`DaemonCore`] and wire framing controls (rules 43 and 64).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum DaemonPlant {
    None,
    /// Accepts a client of another version without dying.
    SurvivesReplace,
    /// Never transitions to [`Death::Idle`].
    NeverIdles,
    /// Never transitions to [`Death::MaxLife`].
    LivesForever,
    /// Never transitions to [`Death::PhaseLimit`].
    IgnoresPhaseLimit,
    /// Accepts frames whose declared length exceeds [`MOST_FRAME`].
    IgnoresFrameCap,
}

/// Sans-IO lifecycle state machine for one resident per-checkout `blaze` daemon (rule 70).
#[derive(Clone, Copy, Debug)]
pub struct DaemonCore {
    config: DaemonConfig,
    hello: Hello,
    started_at: Nanos,
    last_active: Nanos,
    phase_rebuilds: u32,
    requests_served: u64,
    busy: bool,
    death: Option<Death>,
    /// Deferred replacement reason observed while `busy == true`, promoted to `death` once the
    /// active request completes (`on_request_done`) so an in-flight build is never aborted mid-run.
    pending_death: Option<Death>,
    plant: DaemonPlant,
}

impl DaemonCore {
    /// Creates a live [`DaemonCore`] serving `checkout` under binary `version`.
    pub const fn new(
        config: DaemonConfig,
        version: Version,
        checkout: CheckoutKey,
        instance: u64,
        now: Nanos,
    ) -> DaemonCore {
        DaemonCore::planted(config, version, checkout, instance, now, DaemonPlant::None)
    }

    pub(crate) const fn planted(
        config: DaemonConfig,
        version: Version,
        checkout: CheckoutKey,
        instance: u64,
        now: Nanos,
        plant: DaemonPlant,
    ) -> DaemonCore {
        DaemonCore {
            config,
            hello: Hello::new(version, instance, checkout),
            started_at: now,
            last_active: now,
            phase_rebuilds: 0,
            requests_served: 0,
            busy: false,
            death: None,
            pending_death: None,
            plant,
        }
    }

    /// Greeting sent as the first frame on every accepted connection.
    pub const fn hello(&self) -> Hello {
        self.hello
    }

    /// Current exit reason, if this daemon has transitioned to dying.
    pub const fn death(&self) -> Option<Death> {
        self.death
    }

    /// Whether this daemon should stop accepting requests and exit.
    pub const fn is_dead(&self) -> bool {
        self.death.is_some()
    }

    /// Whether a request is currently running.
    pub const fn is_busy(&self) -> bool {
        self.busy
    }

    /// Number of full phase-path rebuilds served so far.
    pub const fn phase_rebuilds(&self) -> u32 {
        self.phase_rebuilds
    }

    /// Total number of requests completed by this daemon.
    pub const fn requests_served(&self) -> u64 {
        self.requests_served
    }

    /// Timestamp of the last completed request (or start time if none).
    pub const fn last_active(&self) -> Nanos {
        self.last_active
    }

    /// Next monotonic timestamp at which a timer tick may expire this daemon, or `None` if
    /// already dead.
    pub const fn next_deadline(&self) -> Option<Nanos> {
        if self.death.is_some() {
            return None;
        }
        let max_at = self.started_at.after(self.config.max_life);
        if self.busy {
            return Some(max_at);
        }
        let idle_at = self.last_active.after(self.config.idle_expiry);
        Some(if idle_at.0 < max_at.0 { idle_at } else { max_at })
    }

    /// Validates a client's `version` and `checkout` against this daemon's identity.
    ///
    /// A checkout mismatch returns `Err(CannotServe::OtherCheckout)` without terminating this
    /// daemon. A version mismatch for the same checkout marks [`Death::Replaced`] so this daemon
    /// exits cleanly and lets the client's binary spawn its own daemon, returning
    /// `Err(CannotServe::OtherVersion)`.
    pub fn on_hello_check(&mut self, req_version: Version, req_checkout: CheckoutKey) -> Result<(), CannotServe> {
        if let Some(death) = self.death {
            return Err(match death {
                Death::Replaced => CannotServe::OtherVersion,
                Death::CheckoutGone => CannotServe::CheckoutGone,
                Death::Idle | Death::MaxLife | Death::PhaseLimit | Death::Shutdown => CannotServe::ShuttingDown,
            });
        }
        if req_checkout != self.hello.checkout {
            return Err(CannotServe::OtherCheckout);
        }
        if req_version != self.hello.version && self.plant != DaemonPlant::SurvivesReplace {
            if self.busy {
                self.pending_death.get_or_insert(Death::Replaced);
            } else {
                self.death = Some(Death::Replaced);
            }
            return Err(CannotServe::OtherVersion);
        }
        if self.busy {
            return Err(CannotServe::Busy);
        }
        Ok(())
    }

    /// Marks the start of an accepted request so idle expiry does not fire mid-request and
    /// concurrent clients receive [`CannotServe::Busy`].
    pub fn on_request_start(&mut self, now: Nanos) {
        self.busy = true;
        self.last_active = now;
    }

    /// Periodic liveness check driven by the event loop. Returns the [`Death`] reason once the
    /// daemon should exit.
    pub fn on_tick(&mut self, now: Nanos, own_exe_replaced: bool, checkout_gone: bool) -> Option<Death> {
        if self.death.is_some() {
            return self.death;
        }
        if own_exe_replaced && self.plant != DaemonPlant::SurvivesReplace {
            if self.busy {
                self.pending_death.get_or_insert(Death::Replaced);
                return None;
            }
            self.death = Some(Death::Replaced);
            return self.death;
        }
        if checkout_gone {
            self.death = Some(Death::CheckoutGone);
            return self.death;
        }
        if self.plant != DaemonPlant::LivesForever && now.since(self.started_at) >= self.config.max_life {
            self.death = Some(Death::MaxLife);
            return self.death;
        }
        if !self.busy
            && self.plant != DaemonPlant::NeverIdles
            && now.since(self.last_active) >= self.config.idle_expiry
        {
            self.death = Some(Death::Idle);
            return self.death;
        }
        None
    }

    /// Records completion of one request. Updates `last_active`, increments `phase_rebuilds` when
    /// `was_phase_rebuild` is true, and transitions to [`Death::PhaseLimit`] once
    /// `phase_rebuilds >= config.max_phase_rebuilds` (or promotes a deferred [`Death::Replaced`]).
    pub fn on_request_done(&mut self, now: Nanos, was_phase_rebuild: bool) -> Option<Death> {
        self.busy = false;
        self.last_active = now;
        self.requests_served = self.requests_served.saturating_add(1);
        if was_phase_rebuild {
            self.phase_rebuilds = self.phase_rebuilds.saturating_add(1);
            if self.death.is_none()
                && self.plant != DaemonPlant::IgnoresPhaseLimit
                && self.phase_rebuilds >= self.config.max_phase_rebuilds
            {
                self.death = Some(Death::PhaseLimit);
            }
        }
        if self.death.is_none()
            && let Some(pending) = self.pending_death.take()
        {
            self.death = Some(pending);
        }
        self.death
    }

    /// Transitions the daemon to [`Death::Shutdown`] (preserving any earlier death reason).
    pub fn on_shutdown(&mut self) -> Death {
        if self.death.is_none() {
            self.death = Some(Death::Shutdown);
        }
        self.death.unwrap_or(Death::Shutdown)
    }
}

#[cfg(test)]
mod tests {
    use std::alloc::Global;

    use blaze_own::Vec;

    use super::*;

    fn ver(bytes: &[u8]) -> Version {
        Version::of(bytes).expect("valid test version")
    }

    fn key(byte: u8) -> CheckoutKey {
        let mut raw = [0u8; 32];
        for (i, slot) in raw.iter_mut().enumerate() {
            *slot = byte.wrapping_add(i as u8);
        }
        CheckoutKey(raw)
    }

    fn strip_frame(buf: &[u8]) -> &[u8] {
        let len = frame_len(buf).expect("at least 4 bytes").expect("within MOST_FRAME");
        assert_eq!(buf.len(), 4 + len);
        &buf[4..]
    }

    #[test]
    fn socket_path_format_uses_daemon_folder_and_16_hex_digits() {
        let mut raw = [0u8; 32];
        raw[..8].copy_from_slice(&[0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef]);
        let path = socket_path(&CheckoutKey(raw));
        assert_eq!(&path, b"daemon/0123456789abcdef.sock");
        assert_eq!(path.len(), SOCKET_PATH_BYTES);
    }

    #[test]
    fn hello_round_trip_and_form_rejection() {
        let hello = Hello::new(ver(b"build-uuid-1234"), 0xdead_beef_cafe_babe, key(0x10));
        let mut buf = Vec::new_in(Global);
        hello.encode(&mut buf);
        let decoded = Hello::decode(strip_frame(&buf)).expect("hello decodes");
        assert_eq!(decoded, hello);

        // Wrong form is rejected by name.
        let bad = Hello { form: FORM + 1, ..hello };
        let mut bad_buf = Vec::new_in(Global);
        bad.encode(&mut bad_buf);
        assert_eq!(Hello::decode(strip_frame(&bad_buf)), Err(WireError::Form(u64::from(FORM + 1))));
    }

    #[test]
    fn client_frames_round_trip_all_variants_and_cols_edge_cases() {
        for cols in [None, Some(0), Some(80), Some(u16::MAX)] {
            let argv_items: &[&[u8]] = &[b"blaze", b"check", b"--json"];
            let env_items: &[(&[u8], &[u8])] = &[(b"TERM", b"xterm-256color"), (b"EMPTY", b""), (b"RAW", &[0xff, 0xfe])];
            let req_ref = CheckReqRef {
                version: ver(b"v1"),
                checkout: key(0x42),
                cwd: b"/Volumes/Projects/demo",
                argv: argv_items,
                stdout_tty: true,
                stderr_tty: false,
                cols,
                env: env_items,
                client_start_mono_ns: 123_456_789_012,
            };
            let mut buf = Vec::new_in(Global);
            req_ref.encode(&mut buf);
            let decoded = ClientFrame::decode_in(strip_frame(&buf), Global).expect("check frame decodes");
            let ClientFrame::Check(req) = decoded else {
                panic!("expected Check frame, got {decoded:?}");
            };
            assert_eq!(req.as_ref(), req_ref);

            // Re-encoding the decoded CheckReq produces byte-for-byte identical deterministic CBOR.
            let mut reencoded = Vec::new_in(Global);
            ClientFrame::Check(req).encode(&mut reencoded);
            assert_eq!(buf.as_slice(), reencoded.as_slice());
        }

        // Signal round-trip
        let mut sig_buf = Vec::new_in(Global);
        encode_signal(2, &mut sig_buf);
        assert_eq!(ClientFrame::decode_in(strip_frame(&sig_buf), Global), Ok(ClientFrame::<Global>::Signal(2)));

        // Shutdown round-trip
        let mut shut_buf = Vec::new_in(Global);
        encode_shutdown(&mut shut_buf);
        assert_eq!(ClientFrame::decode_in(strip_frame(&shut_buf), Global), Ok(ClientFrame::<Global>::Shutdown));
    }

    #[test]
    fn server_frames_round_trip_all_variants() {
        let cases = [
            ServerFrame::Out(b"hello stdout\n"),
            ServerFrame::Out(b""),
            ServerFrame::Err(b"warning: unused\n"),
            ServerFrame::Done { status: 0 },
            ServerFrame::Done { status: 101 },
            ServerFrame::Refused { why: CannotServe::OtherVersion },
            ServerFrame::Refused { why: CannotServe::OtherCheckout },
            ServerFrame::Refused { why: CannotServe::CheckoutGone },
            ServerFrame::Refused { why: CannotServe::ShuttingDown },
            ServerFrame::Refused { why: CannotServe::Busy },
        ];
        for frame in cases {
            let mut buf = Vec::new_in(Global);
            frame.encode(&mut buf);
            let decoded = ServerFrame::decode(strip_frame(&buf)).expect("server frame decodes");
            assert_eq!(decoded, frame);
        }
    }

    fn check_frame_cap_oracle(plant: DaemonPlant) -> Result<(), &'static str> {
        let ok_head = (MOST_FRAME as u32).to_le_bytes();
        if frame_len_planted(&ok_head, plant) != Some(Ok(MOST_FRAME)) {
            return Err("exact MOST_FRAME length should be accepted");
        }
        let over = (MOST_FRAME as u32) + 1;
        let over_head = over.to_le_bytes();
        if frame_len_planted(&over_head, plant) != Some(Err(WireError::Length(u64::from(over)))) {
            return Err("length past MOST_FRAME must be rejected");
        }
        if frame_len_planted(&[1, 2, 3], plant).is_some() {
            return Err("fewer than 4 bytes must return None");
        }
        Ok(())
    }

    #[test]
    fn frame_cap_rejection_and_planted_control() {
        assert_eq!(check_frame_cap_oracle(DaemonPlant::None), Ok(()));
        assert!(check_frame_cap_oracle(DaemonPlant::IgnoresFrameCap).is_err(), "planted IgnoresFrameCap must be caught");
    }

    fn check_version_mismatch_oracle(plant: DaemonPlant) -> Result<(), &'static str> {
        let ck = key(1);
        let other_ck = key(9);
        let v1 = ver(b"build-1");
        let v2 = ver(b"build-2");
        let mut core = DaemonCore::planted(DaemonConfig::SHIPPED, v1, ck, 7, Nanos(1_000), plant);

        // Different checkout is refused without killing the daemon (even if version differs too).
        if core.on_hello_check(v2, other_ck) != Err(CannotServe::OtherCheckout) {
            return Err("different checkout must return OtherCheckout");
        }
        if core.is_dead() {
            return Err("different checkout must not kill this daemon");
        }

        // Exact match succeeds.
        if core.on_hello_check(v1, ck) != Ok(()) {
            return Err("matching version and checkout must succeed");
        }

        // Same checkout, different version -> refused AND marks Death::Replaced.
        if core.on_hello_check(v2, ck) != Err(CannotServe::OtherVersion) {
            return Err("version mismatch must return OtherVersion");
        }
        if core.death() != Some(Death::Replaced) {
            return Err("version mismatch must mark Death::Replaced");
        }
        Ok(())
    }

    #[test]
    fn version_mismatch_triggers_death_replaced_and_planted_control() {
        assert_eq!(check_version_mismatch_oracle(DaemonPlant::None), Ok(()));
        assert!(check_version_mismatch_oracle(DaemonPlant::SurvivesReplace).is_err(), "planted SurvivesReplace must be caught");
    }

    fn check_idle_and_max_life_oracle(plant: DaemonPlant) -> Result<(), &'static str> {
        let cfg = DaemonConfig {
            idle_expiry: Nanos::secs(60),
            max_life: Nanos::secs(150),
            max_phase_rebuilds: 4,
        };
        let v = ver(b"v1");
        let ck = key(2);

        // Idle path: active request prevents idle expiry, then quiet period triggers Death::Idle.
        let mut idle_core = DaemonCore::planted(cfg, v, ck, 1, Nanos(0), plant);
        if idle_core.on_tick(Nanos::secs(59), false, false).is_some() {
            return Err("expired before idle_expiry");
        }
        idle_core.on_request_start(Nanos::secs(59));
        if idle_core.on_hello_check(v, ck) != Err(CannotServe::Busy) {
            return Err("busy daemon must refuse concurrent request");
        }
        // Still busy past idle_expiry -> must NOT idle out mid-request.
        if idle_core.on_tick(Nanos::secs(130), false, false).is_some() {
            return Err("busy daemon must not idle out before max_life");
        }
        idle_core.on_request_done(Nanos::secs(130), false);
        // After request completes at t=130, idle_expiry would be t=190, but max_life is t=150!
        if idle_core.next_deadline() != Some(Nanos::secs(150)) {
            return Err("next_deadline should pick max_life when earlier than idle_expiry");
        }
        if idle_core.on_tick(Nanos::secs(150), false, false) != Some(Death::MaxLife) {
            return Err("must transition to Death::MaxLife at max_life");
        }

        // Pure idle expiry before max_life.
        let mut pure_idle = DaemonCore::planted(cfg, v, ck, 2, Nanos(0), plant);
        if pure_idle.on_tick(Nanos::secs(60), false, false) != Some(Death::Idle) {
            return Err("must transition to Death::Idle at idle_expiry");
        }
        Ok(())
    }

    #[test]
    fn idle_and_max_life_expiry_and_planted_controls() {
        assert_eq!(check_idle_and_max_life_oracle(DaemonPlant::None), Ok(()));
        assert!(check_idle_and_max_life_oracle(DaemonPlant::NeverIdles).is_err(), "planted NeverIdles must be caught");
        assert!(check_idle_and_max_life_oracle(DaemonPlant::LivesForever).is_err(), "planted LivesForever must be caught");
    }

    fn check_phase_limit_oracle(plant: DaemonPlant) -> Result<(), &'static str> {
        let cfg = DaemonConfig {
            idle_expiry: IDLE_EXPIRY,
            max_life: MAX_LIFE,
            max_phase_rebuilds: 3,
        };
        let mut core = DaemonCore::planted(cfg, ver(b"v1"), key(3), 5, Nanos(0), plant);
        // Non-rebuild requests never count toward phase_rebuilds.
        for i in 1..=10u64 {
            core.on_request_start(Nanos::secs(i));
            if core.on_request_done(Nanos::secs(i), false).is_some() {
                return Err("non-phase requests must not trip PhaseLimit");
            }
        }
        if core.phase_rebuilds() != 0 || core.requests_served() != 10 {
            return Err("unexpected counters after non-rebuild requests");
        }
        // First 2 phase rebuilds stay alive.
        for i in 11..=12u64 {
            core.on_request_start(Nanos::secs(i));
            if core.on_request_done(Nanos::secs(i), true).is_some() {
                return Err("tripped PhaseLimit too early");
            }
        }
        // 3rd phase rebuild trips Death::PhaseLimit.
        core.on_request_start(Nanos::secs(13));
        if core.on_request_done(Nanos::secs(13), true) != Some(Death::PhaseLimit) {
            return Err("must trip Death::PhaseLimit at max_phase_rebuilds");
        }
        if core.on_hello_check(ver(b"v1"), key(3)) != Err(CannotServe::ShuttingDown) {
            return Err("dead PhaseLimit daemon must refuse new requests with ShuttingDown");
        }
        Ok(())
    }

    #[test]
    fn phase_rebuild_cap_and_planted_control() {
        assert_eq!(check_phase_limit_oracle(DaemonPlant::None), Ok(()));
        assert!(check_phase_limit_oracle(DaemonPlant::IgnoresPhaseLimit).is_err(), "planted IgnoresPhaseLimit must be caught");
    }

    #[test]
    fn tick_exe_replaced_checkout_gone_and_shutdown() {
        let mut c1 = DaemonCore::new(DaemonConfig::SHIPPED, ver(b"v1"), key(4), 1, Nanos(0));
        assert_eq!(c1.on_tick(Nanos::secs(1), true, false), Some(Death::Replaced));
        assert_eq!(c1.on_shutdown(), Death::Replaced, "first death reason is preserved");

        let mut c2 = DaemonCore::new(DaemonConfig::SHIPPED, ver(b"v1"), key(4), 2, Nanos(0));
        assert_eq!(c2.on_tick(Nanos::secs(1), false, true), Some(Death::CheckoutGone));
        assert_eq!(c2.on_hello_check(ver(b"v1"), key(4)), Err(CannotServe::CheckoutGone));

        let mut c3 = DaemonCore::new(DaemonConfig::SHIPPED, ver(b"v1"), key(4), 3, Nanos(0));
        assert_eq!(c3.on_shutdown(), Death::Shutdown);
        assert_eq!(c3.on_hello_check(ver(b"v1"), key(4)), Err(CannotServe::ShuttingDown));
    }

    #[test]
    fn encode_chunk_header_matches_server_frame_encode() {
        for &is_err in &[false, true] {
            for &len in &[0usize, 1, 23, 24, 255, 256, 65_535, 65_536, 520_000] {
                let payload = std::vec![0xABu8; len];
                let arena = std::boxed::Box::new(blaze_own::Inline::<600_000>::new());
                let mut expected = Vec::with_capacity_in(len + 32, &*arena);
                let frame = if is_err {
                    ServerFrame::Err(&payload)
                } else {
                    ServerFrame::Out(&payload)
                };
                frame.encode(&mut expected);

                let (head, head_len) = encode_chunk_header(is_err, len);
                let mut vectored = std::vec::Vec::with_capacity(head_len + len);
                vectored.extend_from_slice(&head[..head_len]);
                vectored.extend_from_slice(&payload);
                assert_eq!(vectored.as_slice(), expected.as_slice(), "mismatch for is_err={is_err} len={len}");
            }
        }
    }

    #[test]
    fn busy_request_defers_replacement_until_request_done() {
        let mut core = DaemonCore::new(DaemonConfig::SHIPPED, ver(b"v1"), key(7), 9, Nanos(0));
        core.on_request_start(Nanos::secs(1));
        assert!(core.is_busy());
        // Newer client connects while busy: refused with OtherVersion, daemon stays alive until done.
        assert_eq!(core.on_hello_check(ver(b"v2"), key(7)), Err(CannotServe::OtherVersion));
        assert!(!core.is_dead(), "replacement must be deferred while a request is active");
        // Executable replaced tick while busy also defers death.
        assert_eq!(core.on_tick(Nanos::secs(2), true, false), None);
        assert!(!core.is_dead(), "exe-replaced tick must defer death while busy");
        // Completing the active request promotes deferred Death::Replaced.
        assert_eq!(core.on_request_done(Nanos::secs(3), false), Some(Death::Replaced));
        assert_eq!(core.death(), Some(Death::Replaced));
    }

    #[test]
    fn host_valid_large_arg_and_many_args_round_trip_within_frame_cap() {
        let alloc = blaze_own::testing::heap().alloc();
        let mut big_arg = Vec::with_capacity_in(128 << 10, alloc);
        big_arg.resize(128 << 10, b'x'); // > 64 KiB, well within MOST_FRAME
        let mut wire = Vec::new_in(alloc);
        let argv_large: [&[u8]; 2] = [b"blaze", big_arg.as_slice()];
        let req_large = CheckReqRef {
            version: ver(b"v1"),
            checkout: key(8),
            cwd: b"/ws",
            argv: &argv_large,
            stdout_tty: false,
            stderr_tty: false,
            cols: None,
            env: &[],
            client_start_mono_ns: 1,
        };
        req_large.encode(&mut wire);
        let body = &wire[4..];
        let decoded = ClientFrame::decode_in(body, alloc).expect("128 KiB arg must decode");
        match decoded {
            ClientFrame::Check(c) => assert_eq!(c.argv[1].len(), 128 << 10),
            _ => panic!("expected Check"),
        }

        // > 4096 short args (5000 args) within MOST_FRAME.
        let mut many_args: Vec<&[u8], _> = Vec::with_capacity_in(5000, alloc);
        many_args.resize(5000, b"a".as_slice());
        let mut wire2 = Vec::new_in(alloc);
        let req_many = CheckReqRef {
            version: ver(b"v1"),
            checkout: key(8),
            cwd: b"/ws",
            argv: many_args.as_slice(),
            stdout_tty: false,
            stderr_tty: false,
            cols: None,
            env: &[],
            client_start_mono_ns: 2,
        };
        req_many.encode(&mut wire2);
        let decoded2 = ClientFrame::decode_in(&wire2[4..], alloc).expect("5000 args must decode");
        match decoded2 {
            ClientFrame::Check(c) => assert_eq!(c.argv.len(), 5000),
            _ => panic!("expected Check"),
        }
    }
}
