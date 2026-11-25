//! `blaze peer [--bind <ip>:0] [--udp-port 41729] [--idle-secs 1800] [--segments <dir>]`:
//! zero-trust LAN `.crate` and `BZS1` (`segz-`) compressed segment cache peer server.
//!
//! On startup, scans `<CARGO_HOME>/registry/cache/<crates.io-folder>/*.crate` once (indexing by
//! exact 32-byte SHA-256) and optionally `--segments <dir>` for `Namespace::PUBLIC` `BZS1`
//! (`segz-<blake3>`) or canonical `BLPS` (`seg-<blake3>`) segments (always served compressed as
//! `BZS1` over the wire — zero uncompressed segment transfer bloat, zero path traversal).
//! Listens on UDP for `BLZP` `HAVE` (`.crate`) and `HAVE_SEGZ` (`BZS1` segment) probes, replies
//! unicast `HAS` / `HAS_SEGZ`, and serves verified content at `GET /v1/crate/<sha256>` and
//! `GET /v1/segz/<blake3>` over `bstd::net::Listener`.
//!
//! Holds no state between restarts and exits cleanly after its idle expiry (rule 70).

use core::alloc::Allocator;
use core::fmt;
use core::net::{IpAddr, Ipv4Addr, SocketAddr};
use core::sync::atomic::{AtomicBool, Ordering};
use core::time::Duration;

use blaze_cas::plain::{Namespace, check_segment};
use blaze_fetch::local::{CARGO_CACHE, crates_io_folder};
use blaze_fetch::peer::{
    DEFAULT_UDP_PORT, HAS_BYTES, HAVE_HEADER_BYTES, HAVE_MAX_BYTES, HAVE_MAX_HASHES, HasPacket, MAGIC,
    MAX_REQUEST_HEAD, MULTICAST_GROUP, PeerRequest, VERSION, decode_have, encode_has, error_response,
    is_lan_ip, parse_http_request, parse_lower_hex_32, sha256, write_ok_head,
};
use blaze_ids::ContentId;
use blaze_own::Vec as OwnVec;
use blaze_remote::wire_segment::{
    MAGIC as BZS1_MAGIC, VERSION as BZS1_VERSION, VerifiedWireSegment, WIRE_HEADER, WIRE_MIN_LEN,
    WireSegmentError, decode_wire_segment_verified, encode_wire_segment,
};
use bstd::Stopped;
use bstd::args::Args;
use bstd::clock::Clock;
use bstd::cx::Cx;
use bstd::entry::Status;
use bstd::env::{Env, VarName};
use bstd::fs::{Dir, Fs, FsError, HomeError, ReadOnly};
use bstd::mem::{Bytes, Frame, MemError, Tag};
use bstd::net::{
    Addr, Ask, Batch, Conn, Datagrams, DatagramsOrAccept, DgramOptions, Ecn, HostPort, Listener, MulticastV4, Net,
    NetKind, RecvArm, Slots, UdpOptions,
};
use bstd::out::Out;
use bstd::path::{Path, PathError};
use bstd::rng::Rng;

use crate::CliError;

/// Maximum `.crate` archive size indexed/served (matches `blaze_fetch::Limits::REAL.crate_most`).
const MAX_CRATE_BYTES: u64 = 256 << 20;
/// Maximum `BZS1` wire segment size indexed/served (matches `store/take.rs::SEGMENT_TAKEN_MOST`).
const MAX_SEGZ_BYTES: u64 = 64 << 20;
/// `BLZP` v1 datagram kind: client -> LAN probe for up to 32 `BZS1` segment BLAKE3 digests.
pub(crate) const KIND_HAVE_SEGZ: u8 = 3;
/// `BLZP` v1 datagram kind: peer -> client unicast reply announcing matching `BZS1` segments.
pub(crate) const KIND_HAS_SEGZ: u8 = 4;
/// HTTP route prefix serving `BZS1` compressed segments by 64-char lowercase hex BLAKE3 digest.
pub(crate) const SEGZ_ROUTE_PREFIX: &str = "/v1/segz/";
/// Maximum directory depth walked by `scan_segz_dir` (covers `<origin>/public/<tc>/<target>`).
const SEGZ_SCAN_DEPTH_MOST: u8 = 3;
/// Maximum HTTP body read iterations when fetching a `BZS1` segment from a LAN peer (rule 18).
const SEGZ_FETCH_READS_MOST: usize = (64 << 20) / (16 << 10) + 64;
/// Default idle expiry before `blaze peer` exits cleanly (rule 70: 30 minutes).
const DEFAULT_IDLE_SECS: u64 = 1800;
/// Watchdog poll interval for production idle-expiry checks.
const WATCHDOG_IDLE_TICK: Duration = Duration::from_secs(1);
/// Watchdog poll interval when an explicit stop flag is attached (tests).
const WATCHDOG_STOP_TICK: Duration = Duration::from_millis(20);
/// Per-connection HTTP request/response deadline (rule 18).
const CONN_WAIT: Duration = Duration::from_secs(5);
/// Shutdown close timeout for sockets.
const CLOSE_WAIT: Duration = Duration::from_secs(2);
/// Ephemeral port range probed when `--bind <ip>:0` is requested (`HostPort` requires port >= 1).
const EPHEMERAL_START: u16 = 41730;
const EPHEMERAL_SPAN: u16 = 18000;
const PORT_TRIES: u16 = 32;

const CARGO_REGISTRY: &str = "registry";
const CARGO_CACHE_IN_REGISTRY: &str = "cache";

/// Typed error for `blaze peer` (rule 19).
#[derive(Debug)]
pub(crate) enum PeerError {
    Usage(&'static str),
    NoNet,
    NoCargoHome,
    CacheOpen(HomeError),
    Fs(FsError),
    Path(PathError),
    Mem(MemError),
    Listen(NetKind),
    UdpBind(NetKind),
    PortExhausted(IpAddr),
}

impl fmt::Display for PeerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PeerError::Usage(msg) => write!(f, "usage: blaze peer [--bind <ip>:0] [--udp-port 41729] [--idle-secs 1800] [--segments <dir>]: {msg}"),
            PeerError::NoNet => f.write_str("blaze peer: network capability is unavailable"),
            PeerError::NoCargoHome => f.write_str("blaze peer: neither CARGO_HOME nor HOME is set"),
            PeerError::CacheOpen(err) => write!(f, "blaze peer: could not open cargo registry cache: {err}"),
            PeerError::Fs(err) => write!(f, "blaze peer: filesystem error: {err}"),
            PeerError::Path(err) => write!(f, "blaze peer: invalid path: {err}"),
            PeerError::Mem(err) => write!(f, "blaze peer: frame allocation failed: {err}"),
            PeerError::Listen(kind) => write!(f, "blaze peer: TCP listen failed: {kind:?}"),
            PeerError::UdpBind(kind) => write!(f, "blaze peer: UDP bind failed: {kind:?}"),
            PeerError::PortExhausted(ip) => write!(f, "blaze peer: no free ephemeral TCP port found on {ip} after {PORT_TRIES} tries"),
        }
    }
}

impl From<FsError> for PeerError {
    fn from(err: FsError) -> PeerError {
        PeerError::Fs(err)
    }
}

impl From<PathError> for PeerError {
    fn from(err: PathError) -> PeerError {
        PeerError::Path(err)
    }
}

impl From<MemError> for PeerError {
    fn from(err: MemError) -> PeerError {
        PeerError::Mem(err)
    }
}

struct PeerConfig {
    bind_ip: IpAddr,
    bind_port: u16,
    udp_ip: IpAddr,
    udp_port: u16,
    idle_expiry: Duration,
    /// Optional index into `args` of the `--segments` directory value (avoids heap copy).
    segments_arg: Option<usize>,
    /// Inline `--segments=<dir>` prefix slice offset if passed with `=`.
    segments_eq: bool,
}

fn parse_args(args: &Args) -> Result<PeerConfig, PeerError> {
    let mut bind_ip = IpAddr::V4(Ipv4Addr::UNSPECIFIED);
    let mut bind_port: u16 = 0;
    let mut udp_ip: Option<IpAddr> = None;
    let mut udp_port: u16 = DEFAULT_UDP_PORT;
    let mut idle_secs: u64 = DEFAULT_IDLE_SECS;
    let mut segments_arg: Option<usize> = None;
    let mut segments_eq = false;

    let mut at = 2;
    while let Some(arg) = args.get(at) {
        let cur = at;
        at += 1;
        let mut next_val = || -> Result<&str, PeerError> {
            let val = args.get(at).ok_or(PeerError::Usage("flag requires a value"))?;
            at += 1;
            Ok(val)
        };
        if arg == "--bind" || arg.starts_with("--bind=") {
            let raw = match arg.strip_prefix("--bind=") {
                Some(v) => v,
                None => next_val()?,
            };
            let parsed: SocketAddr = raw.parse().map_err(|_| PeerError::Usage("--bind expects <ip>:<port> (e.g. 0.0.0.0:0 or 127.0.0.1:0)"))?;
            bind_ip = parsed.ip();
            bind_port = parsed.port();
        } else if arg == "--udp-port" || arg.starts_with("--udp-port=") {
            let raw = match arg.strip_prefix("--udp-port=") {
                Some(v) => v,
                None => next_val()?,
            };
            let port = raw.parse::<u16>().map_err(|_| PeerError::Usage("--udp-port expects a port 1..=65535"))?;
            if port == 0 {
                return Err(PeerError::Usage("--udp-port must be 1..=65535"));
            }
            udp_port = port;
        } else if arg == "--udp-bind" || arg.starts_with("--udp-bind=") {
            let raw = match arg.strip_prefix("--udp-bind=") {
                Some(v) => v,
                None => next_val()?,
            };
            let ip: IpAddr = raw.parse().map_err(|_| PeerError::Usage("--udp-bind expects an IP address"))?;
            udp_ip = Some(ip);
        } else if arg == "--idle-secs" || arg.starts_with("--idle-secs=") {
            let raw = match arg.strip_prefix("--idle-secs=") {
                Some(v) => v,
                None => next_val()?,
            };
            let secs = raw.parse::<u64>().map_err(|_| PeerError::Usage("--idle-secs expects a positive integer"))?;
            if secs == 0 {
                return Err(PeerError::Usage("--idle-secs must be at least 1"));
            }
            idle_secs = secs;
        } else if arg == "--segments" {
            let val = next_val()?;
            if val.is_empty() {
                return Err(PeerError::Usage("--segments expects a non-empty directory path"));
            }
            segments_arg = Some(cur + 1);
            segments_eq = false;
        } else if let Some(val) = arg.strip_prefix("--segments=") {
            if val.is_empty() {
                return Err(PeerError::Usage("--segments expects a non-empty directory path"));
            }
            segments_arg = Some(cur);
            segments_eq = true;
        } else {
            return Err(PeerError::Usage("unknown argument"));
        }
    }

    let udp_ip = udp_ip.unwrap_or(bind_ip);
    Ok(PeerConfig {
        bind_ip,
        bind_port,
        udp_ip,
        udp_port,
        idle_expiry: Duration::from_secs(idle_secs),
        segments_arg,
        segments_eq,
    })
}

fn segments_dir_arg<'a>(args: &'a Args, cfg: &PeerConfig) -> Option<&'a str> {
    let idx = cfg.segments_arg?;
    let raw = args.get(idx)?;
    if cfg.segments_eq { raw.strip_prefix("--segments=") } else { Some(raw) }
}

struct CrateEntry {
    path: Path,
    len: u64,
}

type CrateIndex<'a> = OwnVec<([u8; 32], CrateEntry), &'a Frame>;
type CrateTable<'a> = &'a [([u8; 32], CrateEntry)];

fn lookup_crate<'a>(table: CrateTable<'a>, digest: &[u8; 32]) -> Option<&'a CrateEntry> {
    let pos = table.binary_search_by_key(digest, |(k, _)| *k).ok()?;
    table.get(pos).map(|(_, entry)| entry)
}

/// How a LAN-advertised public segment is stored on disk.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SegzKind {
    /// Already `BZS1`-encoded (`segz-<64-lower-hex>`): served directly off disk.
    PreEncoded,
    /// Canonical `BLPS` (`seg-<64-lower-hex>`): encoded to `BZS1` before sending on the wire so
    /// uncompressed segment bloat never traverses the LAN.
    RawBlps,
}

struct SegzEntry {
    path: Path,
    len: u64,
    kind: SegzKind,
}

type SegzIndex<'a> = OwnVec<(ContentId, SegzEntry), &'a Frame>;
type SegzTable<'a> = &'a [(ContentId, SegzEntry)];

fn lookup_segz<'a>(table: SegzTable<'a>, digest: &ContentId) -> Option<&'a SegzEntry> {
    let pos = table.binary_search_by_key(digest, |(k, _)| *k).ok()?;
    table.get(pos).map(|(_, entry)| entry)
}

/// Zero-copy view of a `BLZP` v1 `HAVE_SEGZ` probe carrying up to 32 segment BLAKE3 digests.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct HaveSegzRef<'a> {
    pub nonce: u64,
    count: usize,
    payload: &'a [u8],
}

impl<'a> HaveSegzRef<'a> {
    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn hash(&self, index: usize) -> Option<ContentId> {
        if index >= self.count {
            return None;
        }
        let start = index.checked_mul(32)?;
        let end = start.checked_add(32)?;
        let slice = self.payload.get(start..end)?;
        let mut out = [0u8; 32];
        out.copy_from_slice(slice);
        Some(ContentId(out))
    }

    pub fn iter(&self) -> impl Iterator<Item = (usize, ContentId)> + '_ {
        (0..self.count).filter_map(|i| self.hash(i).map(|h| (i, h)))
    }
}

/// Encodes a `BLZP` v1 `HAVE_SEGZ` datagram for `digests` (`1..=HAVE_MAX_HASHES`).
pub(crate) fn encode_have_segz(nonce: u64, digests: &[ContentId], out: &mut [u8; HAVE_MAX_BYTES]) -> Option<usize> {
    if digests.is_empty() || digests.len() > HAVE_MAX_HASHES {
        return None;
    }
    let [c0, c1] = u16::try_from(digests.len()).ok()?.to_le_bytes();
    let [n0, n1, n2, n3, n4, n5, n6, n7] = nonce.to_le_bytes();
    let [m0, m1, m2, m3] = MAGIC;
    let header = [m0, m1, m2, m3, VERSION, KIND_HAVE_SEGZ, c0, c1, n0, n1, n2, n3, n4, n5, n6, n7];
    out.get_mut(..HAVE_HEADER_BYTES)?.copy_from_slice(&header);
    let mut at = HAVE_HEADER_BYTES;
    for id in digests {
        let next = at.checked_add(32)?;
        out.get_mut(at..next)?.copy_from_slice(&id.0);
        at = next;
    }
    Some(at)
}

/// Decodes a `BLZP` v1 `HAVE_SEGZ` datagram, validating magic, version, kind, and exact length.
pub(crate) fn decode_have_segz(bytes: &[u8]) -> Option<HaveSegzRef<'_>> {
    let (head, payload) = bytes.split_first_chunk::<HAVE_HEADER_BYTES>()?;
    let [m0, m1, m2, m3, ver, kind, c0, c1, n0, n1, n2, n3, n4, n5, n6, n7] = *head;
    if [m0, m1, m2, m3] != MAGIC || ver != VERSION || kind != KIND_HAVE_SEGZ {
        return None;
    }
    let count = usize::from(u16::from_le_bytes([c0, c1]));
    if count == 0 || count > HAVE_MAX_HASHES {
        return None;
    }
    let expected = count.checked_mul(32)?;
    if payload.len() != expected {
        return None;
    }
    let nonce = u64::from_le_bytes([n0, n1, n2, n3, n4, n5, n6, n7]);
    Some(HaveSegzRef { nonce, count, payload })
}

/// Encodes a 24-byte `BLZP` v1 `HAS_SEGZ` reply announcing matching `BZS1` segments.
pub(crate) fn encode_has_segz(has: HasPacket) -> [u8; HAS_BYTES] {
    let [m0, m1, m2, m3] = MAGIC;
    let [p0, p1] = has.tcp_port.to_le_bytes();
    let [n0, n1, n2, n3, n4, n5, n6, n7] = has.nonce.to_le_bytes();
    let [k0, k1, k2, k3] = has.mask.to_le_bytes();
    let [a0, a1, a2, a3] = has.active_transfers.to_le_bytes();
    [m0, m1, m2, m3, VERSION, KIND_HAS_SEGZ, p0, p1, n0, n1, n2, n3, n4, n5, n6, n7, k0, k1, k2, k3, a0, a1, a2, a3]
}

/// Decodes a 24-byte `BLZP` v1 `HAS_SEGZ` reply, rejecting port 0 and empty masks.
pub(crate) fn decode_has_segz(bytes: &[u8]) -> Option<HasPacket> {
    let arr: &[u8; HAS_BYTES] = bytes.try_into().ok()?;
    let [m0, m1, m2, m3, ver, kind, p0, p1, n0, n1, n2, n3, n4, n5, n6, n7, k0, k1, k2, k3, a0, a1, a2, a3] = *arr;
    if [m0, m1, m2, m3] != MAGIC || ver != VERSION || kind != KIND_HAS_SEGZ {
        return None;
    }
    let tcp_port = u16::from_le_bytes([p0, p1]);
    let mask = u32::from_le_bytes([k0, k1, k2, k3]);
    if tcp_port == 0 || mask == 0 {
        return None;
    }
    let nonce = u64::from_le_bytes([n0, n1, n2, n3, n4, n5, n6, n7]);
    let active_transfers = u32::from_le_bytes([a0, a1, a2, a3]);
    Some(HasPacket { tcp_port, nonce, mask, active_transfers })
}

/// Parsed HTTP/1.x request accepted by `blaze peer` (either `.crate` or `BZS1` `segz-` segment).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum PeerHttpRequest {
    GetCrate([u8; 32]),
    GetSegz(ContentId),
    NotFound,
    MethodNotAllowed,
    BadRequest,
    NeedMore,
}

/// Parses an HTTP/1.x request head for `GET /v1/crate/<sha256>` or `GET /v1/segz/<blake3>`.
pub(crate) fn parse_peer_http_request(buf: &[u8]) -> PeerHttpRequest {
    match parse_http_request(buf) {
        PeerRequest::GetCrate(digest) => return PeerHttpRequest::GetCrate(digest),
        PeerRequest::MethodNotAllowed => return PeerHttpRequest::MethodNotAllowed,
        PeerRequest::BadRequest => return PeerHttpRequest::BadRequest,
        PeerRequest::NeedMore => return PeerHttpRequest::NeedMore,
        PeerRequest::NotFound => {}
    }
    // `parse_http_request` already validated framing, `MAX_REQUEST_HEAD`, `HTTP/1.x`, and `GET`.
    let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
        return PeerHttpRequest::BadRequest;
    };
    let Some(head) = buf.get(..end) else {
        return PeerHttpRequest::BadRequest;
    };
    let line_end = head.windows(2).position(|w| w == b"\r\n").unwrap_or(head.len());
    let Some(line) = head.get(..line_end) else {
        return PeerHttpRequest::BadRequest;
    };
    let Some(rest) = line.strip_prefix(b"GET ") else {
        return PeerHttpRequest::NotFound;
    };
    let Some(sp) = rest.iter().position(|&b| b == b' ') else {
        return PeerHttpRequest::NotFound;
    };
    let Some(target) = rest.get(..sp) else {
        return PeerHttpRequest::NotFound;
    };
    let Some(hex) = target.strip_prefix(SEGZ_ROUTE_PREFIX.as_bytes()) else {
        return PeerHttpRequest::NotFound;
    };
    match parse_lower_hex_32(hex) {
        Some(bytes) => PeerHttpRequest::GetSegz(ContentId(bytes)),
        None => PeerHttpRequest::NotFound,
    }
}

fn lower_hex_64(bytes: &[u8; 32]) -> [u8; 64] {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = [0u8; 64];
    for (i, &b) in bytes.iter().enumerate() {
        let hi = usize::from(b >> 4);
        let lo = usize::from(b & 0x0f);
        if let (Some(&hc), Some(&lc), Some([dst_h, dst_l])) = (
            HEX.get(hi),
            HEX.get(lo),
            out.get_mut(i * 2..i * 2 + 2),
        ) {
            *dst_h = hc;
            *dst_l = lc;
        }
    }
    out
}

/// Validates a `segz-<64-lower-hex>` file's 64-byte `BZS1` header: must have magic `BZS1`,
/// version 1, `Namespace::PUBLIC` (private/LOCAL segments NEVER leave the machine), and header
/// digest matching `expected`.
fn check_bzs1_public_header(hdr: &[u8; WIRE_HEADER], expected: ContentId) -> bool {
    let Some(magic) = hdr.get(0..4) else { return false };
    if magic != BZS1_MAGIC {
        return false;
    }
    let Some(ver_bytes) = hdr.get(4..8).and_then(|s| <[u8; 4]>::try_from(s).ok()) else { return false };
    if u32::from_le_bytes(ver_bytes) != BZS1_VERSION {
        return false;
    }
    let Some(ns_bytes) = hdr.get(8..16).and_then(|s| <[u8; 8]>::try_from(s).ok()) else { return false };
    if Namespace(u64::from_le_bytes(ns_bytes)) != Namespace::PUBLIC {
        return false;
    }
    let Some(dig_bytes) = hdr.get(32..64) else { return false };
    dig_bytes == expected.0.as_slice()
}

/// Walks `dir` (up to [`SEGZ_SCAN_DEPTH_MOST`] levels) indexing `Namespace::PUBLIC` `segz-<hex>`
/// (and canonical `seg-<hex>` fallback compressed to `BZS1` on serve) files into a sorted table.
/// Private (`Namespace::LOCAL` or non-PUBLIC) segments are strictly skipped.
fn scan_segz_dir<'a>(
    cx: &Cx,
    dir: &Dir<ReadOnly>,
    frame: &'a Frame,
) -> Result<SegzIndex<'a>, PeerError> {
    let mut index = OwnVec::new_in(frame);
    let mut worklist: OwnVec<(Option<Path>, u8), &'a Frame> = OwnVec::new_in(frame);
    worklist.push((None, 0));
    while let Some((prefix, depth)) = worklist.pop() {
        let entries = match prefix.as_ref() {
            None => match dir.own_entries(cx) {
                Ok(e) => e,
                Err(_) => continue,
            },
            Some(path) => match dir.entries(cx, path) {
                Ok(Some(e)) => e,
                Ok(None) | Err(_) => continue,
            },
        };
        for (name, _) in entries.iter() {
            let Ok(part) = Path::new(name) else { continue };
            let rel = match prefix.as_ref() {
                None => part,
                Some(base) => base.join(&part),
            };
            let Ok(Some(meta)) = dir.stat(cx, &rel) else { continue };
            if meta.dir {
                if depth < SEGZ_SCAN_DEPTH_MOST {
                    worklist.push((Some(rel), depth + 1));
                }
                continue;
            }
            if let Some(hex) = name.strip_prefix("segz-")
                && let Some(bytes) = parse_lower_hex_32(hex.as_bytes())
            {
                if meta.len < WIRE_MIN_LEN || meta.len > MAX_SEGZ_BYTES {
                    continue;
                }
                let Ok(file) = dir.open_read(cx, &rel) else { continue };
                let mut hdr = [0u8; WIRE_HEADER];
                let Ok(got) = file.read_into(&mut hdr, 0).wait(cx) else { continue };
                let digest = ContentId(bytes);
                if got == WIRE_HEADER && check_bzs1_public_header(&hdr, digest) {
                    index.push((digest, SegzEntry { path: rel, len: meta.len, kind: SegzKind::PreEncoded }));
                }
                continue;
            }
            if let Some(hex) = name.strip_prefix("seg-")
                && let Some(bytes) = parse_lower_hex_32(hex.as_bytes())
            {
                if meta.len == 0 || meta.len > MAX_SEGZ_BYTES {
                    continue;
                }
                let Ok(reserve) = usize::try_from(meta.len) else { continue };
                let Ok(scratch) = Frame::with_reserve(reserve.max(1 << 16).saturating_add(1 << 16), Tag::Other) else {
                    continue;
                };
                let mut buf = Bytes::new_in(&scratch, "blaze peer: verify raw BLPS segment");
                if dir.read_all(cx, &rel, &mut buf).is_err() {
                    continue;
                }
                let digest = ContentId(bytes);
                if let Ok(desc) = check_segment(buf.as_slice())
                    && desc.namespace == Namespace::PUBLIC
                    && desc.digest == digest
                {
                    index.push((digest, SegzEntry { path: rel, len: meta.len, kind: SegzKind::RawBlps }));
                }
            }
        }
    }
    // Deterministic sort; prefer `PreEncoded` (`segz-`) over `RawBlps` (`seg-`) for the same digest.
    index.sort_by(|(ka, ea), (kb, eb)| {
        ka.cmp(kb).then_with(|| match (ea.kind, eb.kind) {
            (SegzKind::PreEncoded, SegzKind::RawBlps) => core::cmp::Ordering::Less,
            (SegzKind::RawBlps, SegzKind::PreEncoded) => core::cmp::Ordering::Greater,
            _ => core::cmp::Ordering::Equal,
        })
    });
    index.dedup_by_key(|(k, _)| *k);
    Ok(index)
}

/// Scans `<cargo_home>/registry/cache/<crates.io-folder>/*.crate` once on startup and hashes
/// every archive by SHA-256 into a sorted in-memory table (Send + Sync as a slice).
fn scan_cargo_cache<'a>(
    cx: &Cx,
    fs: &Fs,
    cargo_home: &str,
    frame: &'a Frame,
) -> Result<(Dir<ReadOnly>, CrateIndex<'a>), PeerError> {
    let registry = fs
        .home(cx, cargo_home, &Path::new(CARGO_REGISTRY)?)
        .map_err(PeerError::CacheOpen)?
        .read_only();
    let cache_path = Path::new(CARGO_CACHE_IN_REGISTRY)?;
    let mut index = OwnVec::new_in(frame);
    let Some(folders) = registry.entries(cx, &cache_path)? else {
        return Ok((registry, index));
    };
    for (folder_name, _) in folders.iter() {
        if !crates_io_folder(folder_name) {
            continue;
        }
        let Ok(folder_rel) = Path::new(folder_name) else { continue };
        let folder_path = cache_path.join(&folder_rel);
        let Ok(Some(files)) = registry.entries(cx, &folder_path) else { continue };
        for (file_name, _) in files.iter() {
            if !file_name.ends_with(".crate") {
                continue;
            }
            let Ok(file_rel) = Path::new(file_name) else { continue };
            let rel_path = folder_path.join(&file_rel);
            let Ok(Some(meta)) = registry.stat(cx, &rel_path) else { continue };
            if meta.dir || meta.len == 0 || meta.len > MAX_CRATE_BYTES {
                continue;
            }
            let Ok(reserve) = usize::try_from(meta.len) else { continue };
            let Ok(scratch) = Frame::with_reserve(reserve.max(1 << 16).saturating_add(1 << 16), Tag::Other) else {
                continue;
            };
            let mut buf = Bytes::new_in(&scratch, "blaze peer: hash cached .crate");
            if registry.read_all(cx, &rel_path, &mut buf).is_err() {
                continue;
            }
            let digest = sha256(buf.as_slice());
            let len = u64::try_from(buf.as_slice().len()).unwrap_or(meta.len);
            index.push((digest, CrateEntry { path: rel_path, len }));
        }
    }
    index.sort_by_key(|(k, _)| *k);
    index.dedup_by_key(|(k, _)| *k);
    Ok((registry, index))
}

fn ip_to_host_str(ip: IpAddr, frame: &Frame) -> bstd::mem::String<&Frame> {
    bstd::mem::format_in!(frame, "{ip}")
}

/// Opens a `bstd::net::Listener` on `(ip, port)`. When `port == 0`, probes candidate ports in
/// `EPHEMERAL_START..EPHEMERAL_START + EPHEMERAL_SPAN` seeded by `rng` (verifying bind via a
/// 1 ms non-blocking accept probe).
fn open_listener(
    cx: &Cx,
    net: &Net,
    clock: &Clock,
    rng: &mut Rng,
    ip: IpAddr,
    port: u16,
    frame: &Frame,
) -> Result<(Listener, u16, Option<Conn>), PeerError> {
    let host = ip_to_host_str(ip, frame);
    if port != 0 {
        let hp = HostPort::new(host.as_str(), port).map_err(|_| PeerError::Usage("invalid --bind address"))?;
        let mut listener = net.listen(cx, &Addr::Tcp(hp)).map_err(|e| PeerError::Listen(e.kind))?;
        let probe_cx = cx.until(clock.after(Duration::from_millis(1)));
        match listener.accept(&probe_cx) {
            Ok(conn) => return Ok((listener, port, Some(conn))),
            Err(err) if matches!(err.kind, NetKind::Stopped(Stopped::Deadline { .. })) => {
                return Ok((listener, port, None));
            }
            Err(err) => {
                let _ = listener.close(&cx.until(clock.after(CLOSE_WAIT)));
                return Err(PeerError::Listen(err.kind));
            }
        }
    }
    let start_offset = u16::try_from(rng.next_u64() % u64::from(EPHEMERAL_SPAN)).unwrap_or(0);
    for attempt in 0..PORT_TRIES {
        let candidate = EPHEMERAL_START.wrapping_add((start_offset.wrapping_add(attempt)) % EPHEMERAL_SPAN);
        let Ok(hp) = HostPort::new(host.as_str(), candidate) else { continue };
        let Ok(mut listener) = net.listen(cx, &Addr::Tcp(hp)) else { continue };
        let probe_cx = cx.until(clock.after(Duration::from_millis(1)));
        match listener.accept(&probe_cx) {
            Ok(conn) => return Ok((listener, candidate, Some(conn))),
            Err(err) if matches!(err.kind, NetKind::Stopped(Stopped::Deadline { .. })) => {
                return Ok((listener, candidate, None));
            }
            Err(_) => {
                let _ = listener.close(&cx.until(clock.after(CLOSE_WAIT)));
            }
        }
    }
    Err(PeerError::PortExhausted(ip))
}

fn open_udp(
    cx: &Cx,
    net: &Net,
    ip: IpAddr,
    port: u16,
    frame: &Frame,
) -> Result<(Datagrams, bool), PeerError> {
    let host = ip_to_host_str(ip, frame);
    let hp = HostPort::new(host.as_str(), port).map_err(|_| PeerError::Usage("invalid --udp-bind / --udp-port"))?;
    let join_mcast = matches!(ip, IpAddr::V4(v4) if !v4.is_loopback());
    if join_mcast {
        let opts = DgramOptions {
            tuning: UdpOptions {
                reuse_address: Ask::WhereAvailable(true),
                reuse_port: Ask::WhereAvailable(true),
                multicast_join: Ask::Must(MulticastV4 { group: MULTICAST_GROUP, interface: Ipv4Addr::UNSPECIFIED }),
                ..UdpOptions::DEFAULT
            },
            ..DgramOptions::default()
        };
        if let Ok(sock) = net.bind_datagrams(cx, &hp, &opts) {
            return Ok((sock, true));
        }
    }
    let opts = DgramOptions {
        tuning: UdpOptions {
            reuse_address: Ask::WhereAvailable(true),
            reuse_port: Ask::WhereAvailable(true),
            ..UdpOptions::DEFAULT
        },
        ..DgramOptions::default()
    };
    let sock = net.bind_datagrams(cx, &hp, &opts).map_err(|e| PeerError::UdpBind(e.kind))?;
    Ok((sock, false))
}

/// Serves one HTTP/1.1 connection (`GET /v1/crate/<sha256>` or `GET /v1/segz/<blake3>`),
/// enforcing bounded header length (`MAX_REQUEST_HEAD`) and bounded per-connection deadline
/// (`CONN_WAIT`). Segment transfers are ALWAYS `BZS1`-compressed on the wire.
#[expect(clippy::too_many_arguments, reason = "connection handler takes explicit directories and tables")]
fn serve_conn(
    cx: &Cx,
    clock: &Clock,
    conn: Conn,
    registry: &Dir<ReadOnly>,
    index: CrateTable<'_>,
    segments_dir: Option<&Dir<ReadOnly>>,
    segz_index: SegzTable<'_>,
    corrupt_for_test: bool,
) {
    let conn_cx = cx.until(clock.after(CONN_WAIT));
    let mut head_buf = [0u8; MAX_REQUEST_HEAD];
    let mut got = 0usize;
    let parsed = loop {
        let so_far = head_buf.get(..got).unwrap_or(&[]);
        match parse_peer_http_request(so_far) {
            PeerHttpRequest::NeedMore => {
                let Some(tail) = head_buf.get_mut(got..) else {
                    break PeerHttpRequest::BadRequest;
                };
                if tail.is_empty() {
                    break PeerHttpRequest::BadRequest;
                }
                match conn.read_into(tail).wait(&conn_cx) {
                    Ok(0) => break PeerHttpRequest::BadRequest,
                    Ok(n) => got += n,
                    Err(_) => {
                        let _ = conn.finish(&cx.until(clock.after(CLOSE_WAIT)));
                        return;
                    }
                }
            }
            req => break req,
        }
    };

    match parsed {
        PeerHttpRequest::GetCrate(digest) => match lookup_crate(index, &digest) {
            None => {
                let _ = conn.send_all(&conn_cx, error_response(404));
            }
            Some(entry) => {
                let reserve = usize::try_from(entry.len).unwrap_or(1 << 16);
                if let Ok(scratch) = Frame::with_reserve(reserve.max(1 << 16).saturating_add(1 << 16), Tag::Other) {
                    let mut body = Bytes::new_in(&scratch, "blaze peer: read .crate for client");
                    if registry.read_all(&conn_cx, &entry.path, &mut body).is_ok() {
                        let mut head = [0u8; 160];
                        let payload = body.as_slice();
                        let payload_len = u64::try_from(payload.len()).unwrap_or(0);
                        let head_len = write_ok_head(payload_len, &mut head);
                        let head_slice = head.get(..head_len).unwrap_or(&[]);
                        if corrupt_for_test
                            && let Some((&first, rest)) = payload.split_first()
                        {
                            let flipped = [first ^ 0xff];
                            let _ = conn.send_vectored(&conn_cx, &[head_slice, &flipped, rest]);
                        } else {
                            let _ = conn.send_vectored(&conn_cx, &[head_slice, payload]);
                        }
                    } else {
                        let _ = conn.send_all(&conn_cx, error_response(503));
                    }
                } else {
                    let _ = conn.send_all(&conn_cx, error_response(503));
                }
            }
        },
        PeerHttpRequest::GetSegz(digest) => match (segments_dir, lookup_segz(segz_index, &digest)) {
            (Some(seg_dir), Some(entry)) => {
                let reserve = usize::try_from(entry.len).unwrap_or(1 << 16);
                // Extra headroom when compressing raw BLPS to BZS1 inside the connection frame.
                let frame_cap = match entry.kind {
                    SegzKind::PreEncoded => reserve.max(1 << 16).saturating_add(1 << 16),
                    SegzKind::RawBlps => reserve.saturating_mul(3).max(4 << 20).saturating_add(2 << 20),
                };
                if let Ok(scratch) = Frame::with_reserve(frame_cap, Tag::Other) {
                    let mut raw_body = Bytes::new_in(&scratch, "blaze peer: read BZS1 segment for client");
                    if seg_dir.read_all(&conn_cx, &entry.path, &mut raw_body).is_ok() {
                        let mut encoded = OwnVec::new_in(&scratch);
                        let wire_payload: Option<&[u8]> = match entry.kind {
                            SegzKind::PreEncoded => Some(raw_body.as_slice()),
                            SegzKind::RawBlps => encode_wire_segment(raw_body.as_slice(), &mut encoded, &scratch)
                                .ok()
                                .map(|()| encoded.as_slice()),
                        };
                        if let Some(payload) = wire_payload {
                            let mut head = [0u8; 160];
                            let payload_len = u64::try_from(payload.len()).unwrap_or(0);
                            let head_len = write_ok_head(payload_len, &mut head);
                            let head_slice = head.get(..head_len).unwrap_or(&[]);
                            if corrupt_for_test && let Some((&last, init)) = payload.split_last() {
                                let flipped = [last ^ 0x5a];
                                let _ = conn.send_vectored(&conn_cx, &[head_slice, init, &flipped]);
                            } else {
                                let _ = conn.send_vectored(&conn_cx, &[head_slice, payload]);
                            }
                        } else {
                            let _ = conn.send_all(&conn_cx, error_response(503));
                        }
                    } else {
                        let _ = conn.send_all(&conn_cx, error_response(503));
                    }
                } else {
                    let _ = conn.send_all(&conn_cx, error_response(503));
                }
            }
            _ => {
                let _ = conn.send_all(&conn_cx, error_response(404));
            }
        },
        PeerHttpRequest::NotFound => {
            let _ = conn.send_all(&conn_cx, error_response(404));
        }
        PeerHttpRequest::MethodNotAllowed => {
            let _ = conn.send_all(&conn_cx, error_response(405));
        }
        PeerHttpRequest::BadRequest | PeerHttpRequest::NeedMore => {
            let _ = conn.send_all(&conn_cx, error_response(400));
        }
    }
    let _ = conn.finish(&cx.until(clock.after(CLOSE_WAIT)));
}

#[expect(clippy::too_many_arguments, reason = "peer server loop takes explicit capabilities (rule 75)")]
fn serve_loop(
    cx: &Cx,
    clock: &Clock,
    mut listener: Listener,
    udp: Datagrams,
    tcp_port: u16,
    first_conn: Option<Conn>,
    registry: &Dir<ReadOnly>,
    index: CrateTable<'_>,
    segments_dir: Option<&Dir<ReadOnly>>,
    segz_index: SegzTable<'_>,
    idle_expiry: Duration,
    stop: Option<&AtomicBool>,
    corrupt_for_test: bool,
) -> Result<(), PeerError> {
    let mut last_active = clock.now();
    if let Some(conn) = first_conn {
        serve_conn(cx, clock, conn, registry, index, segments_dir, segz_index, corrupt_for_test);
        last_active = clock.now();
    }

    let mut recv_arm = RecvArm::new();
    let mut rx_batch = Some(Batch::new());
    let mut tx_batch = Batch::new();
    let slots = Slots { most: 16, size: 1200 };
    let tick = if stop.is_some() { WATCHDOG_STOP_TICK } else { WATCHDOG_IDLE_TICK };

    loop {
        if let Some(flag) = stop
            && flag.load(Ordering::Relaxed)
        {
            break;
        }
        let now = clock.now();
        let idle_for = now.since(last_active).unwrap_or(Duration::ZERO);
        if idle_for >= idle_expiry {
            break;
        }
        let wait_for = tick.min(idle_expiry.saturating_sub(idle_for));
        let wait_cx = cx.until(clock.after(wait_for));
        match recv_arm.wait_or_accept(&wait_cx, &udp, &mut listener, &mut rx_batch, slots) {
            Ok(DatagramsOrAccept::Datagrams(batch)) => {
                tx_batch.clear();
                for (head, raw) in batch.each() {
                    if let Some(have) = decode_have(raw) {
                        let mut mask = 0u32;
                        for (bit, digest) in have.iter() {
                            if lookup_crate(index, &digest).is_some()
                                && let Ok(shift) = u32::try_from(bit)
                                && shift < 32
                            {
                                mask |= 1u32 << shift;
                            }
                        }
                        if mask != 0 {
                            let reply = encode_has(HasPacket {
                                tcp_port,
                                nonce: have.nonce,
                                mask,
                                active_transfers: 0,
                            });
                            tx_batch.push(head.peer, Ecn::NotEct, &reply);
                        }
                        continue;
                    }
                    if let Some(have) = decode_have_segz(raw) {
                        let mut mask = 0u32;
                        for (bit, digest) in have.iter() {
                            if lookup_segz(segz_index, &digest).is_some()
                                && let Ok(shift) = u32::try_from(bit)
                                && shift < 32
                            {
                                mask |= 1u32 << shift;
                            }
                        }
                        if mask != 0 {
                            let reply = encode_has_segz(HasPacket {
                                tcp_port,
                                nonce: have.nonce,
                                mask,
                                active_transfers: 0,
                            });
                            tx_batch.push(head.peer, Ecn::NotEct, &reply);
                        }
                    }
                }
                rx_batch = Some(batch);
                if !tx_batch.is_empty() {
                    last_active = clock.now();
                    let send_cx = cx.until(clock.after(Duration::from_millis(200)));
                    if let Ok(cleared) = udp.send(&send_cx, tx_batch) {
                        tx_batch = cleared;
                    } else {
                        tx_batch = Batch::new();
                    }
                }
            }
            Ok(DatagramsOrAccept::Accepted(conn)) => {
                serve_conn(cx, clock, conn, registry, index, segments_dir, segz_index, corrupt_for_test);
                last_active = clock.now();
            }
            Err(err) if matches!(err.kind, NetKind::Stopped(Stopped::Deadline { .. })) => {}
            Err(_) => break,
        }
    }

    recv_arm.disarm(&udp);
    let close_cx = cx.until(clock.after(CLOSE_WAIT));
    let _ = udp.close(&close_cx);
    let _ = listener.close(&close_cx);
    Ok(())
}

/// Typed failure discovering or fetching a verified `BZS1` (`segz-`) segment from a LAN peer.
#[derive(Debug)]
#[cfg_attr(not(test), allow(dead_code, reason = "LAN P2P BZS1 segment client surface; exercised in peer tests and ready for store take"))]
pub(crate) enum SegzFetchError {
    /// Refused non-LAN peer IP (SSRF protection).
    NotLanPeer(IpAddr),
    EmptyQuery,
    TooManyDigests(usize),
    Net(NetKind),
    InvalidHost,
    HttpStatus(u16),
    BadResponse(&'static str),
    TooLarge { len: u64, most: u64 },
    Wire(WireSegmentError),
    DigestMismatch { wanted: ContentId, got: ContentId },
    NotPublic(Namespace),
}

/// Best LAN peer answering a `HAVE_SEGZ` probe.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[cfg_attr(not(test), allow(dead_code, reason = "LAN P2P BZS1 segment discovery result"))]
pub(crate) struct DiscoveredSegzPeer {
    pub addr: SocketAddr,
    pub mask: u32,
    pub active_transfers: u32,
}

/// Sends a `BLZP` `HAVE_SEGZ` probe over UDP and waits up to `wait` for unicast `HAS_SEGZ`
/// replies from LAN peers (`is_lan_ip` enforced), selecting the best match deterministically.
#[cfg_attr(not(test), allow(dead_code, reason = "LAN P2P BZS1 segment discovery client"))]
pub(crate) fn probe_lan_segz(
    cx: &Cx,
    net: &Net,
    clock: &Clock,
    rng: &mut Rng,
    targets: &[SocketAddr],
    digests: &[ContentId],
    wait: Duration,
) -> Result<Option<DiscoveredSegzPeer>, SegzFetchError> {
    if digests.is_empty() {
        return Err(SegzFetchError::EmptyQuery);
    }
    if digests.len() > HAVE_MAX_HASHES {
        return Err(SegzFetchError::TooManyDigests(digests.len()));
    }
    let nonce = rng.next_u64();
    let mut wire = [0u8; HAVE_MAX_BYTES];
    let len = encode_have_segz(nonce, digests, &mut wire).ok_or(SegzFetchError::EmptyQuery)?;
    let payload = wire.get(..len).ok_or(SegzFetchError::BadResponse("encoded HAVE_SEGZ slice"))?;

    let bind_ip = if targets.iter().all(|t| t.ip().is_loopback()) { "127.0.0.1" } else { "0.0.0.0" };
    let start_offset = u16::try_from(rng.next_u64() % u64::from(EPHEMERAL_SPAN)).unwrap_or(0);
    let mut sock = None;
    let mut last_err = NetKind::NotAllowed;
    for attempt in 0..PORT_TRIES {
        let candidate = EPHEMERAL_START.wrapping_add((start_offset.wrapping_add(attempt)) % EPHEMERAL_SPAN);
        let Ok(hp) = HostPort::new(bind_ip, candidate) else { continue };
        let opts = DgramOptions {
            tuning: UdpOptions {
                reuse_address: Ask::WhereAvailable(true),
                ..UdpOptions::DEFAULT
            },
            ..DgramOptions::default()
        };
        match net.bind_datagrams(cx, &hp, &opts) {
            Ok(bound) => {
                sock = Some(bound);
                break;
            }
            Err(err) => last_err = err.kind,
        }
    }
    let udp = sock.ok_or(SegzFetchError::Net(last_err))?;

    let mut tx = Batch::new();
    for &target in targets {
        tx.push(target, Ecn::NotEct, payload);
    }
    let send_cx = cx.until(clock.after(wait));
    if let Err(err) = udp.send(&send_cx, tx) {
        let _ = udp.close(&cx.until(clock.after(CLOSE_WAIT)));
        return Err(SegzFetchError::Net(err.kind));
    }

    let deadline = clock.after(wait);
    let slots = Slots { most: 16, size: 64 };
    let mut best: Option<DiscoveredSegzPeer> = None;
    let recv_cx = cx.until(deadline);
    if let Ok(batch) = udp.recv(&recv_cx, Batch::new(), slots) {
        for (head, raw) in batch.each() {
            if !is_lan_ip(head.peer.ip()) {
                continue;
            }
            let Some(has) = decode_has_segz(raw) else { continue };
            if has.nonce != nonce {
                continue;
            }
            let candidate = DiscoveredSegzPeer {
                addr: SocketAddr::new(head.peer.ip(), has.tcp_port),
                mask: has.mask,
                active_transfers: has.active_transfers,
            };
            let better = match best {
                None => true,
                Some(cur) => {
                    (candidate.mask.count_ones(), core::cmp::Reverse(candidate.active_transfers), core::cmp::Reverse(candidate.addr))
                        > (cur.mask.count_ones(), core::cmp::Reverse(cur.active_transfers), core::cmp::Reverse(cur.addr))
                }
            };
            if better {
                best = Some(candidate);
            }
        }
    }
    let _ = udp.close(&cx.until(clock.after(CLOSE_WAIT)));
    Ok(best)
}

/// Fetches a `BZS1` compressed segment (`GET /v1/segz/<64-lower-hex>`) from a LAN peer and
/// cryptographically verifies its full canonical `BLPS` BLAKE3 digest and `Namespace::PUBLIC`
/// BEFORE returning any bytes (zero-trust P2P, rule Trust).
#[cfg_attr(not(test), allow(dead_code, reason = "LAN P2P BZS1 verified segment fetcher"))]
pub(crate) fn fetch_verified_segz_from_peer<'w, A: Allocator + Copy>(
    cx: &Cx,
    net: &Net,
    clock: &Clock,
    peer: SocketAddr,
    expected: ContentId,
    max_raw_len: u64,
    wire_out: &'w mut OwnVec<u8, A>,
    alloc: A,
) -> Result<VerifiedWireSegment<'w, A>, SegzFetchError> {
    if !is_lan_ip(peer.ip()) {
        return Err(SegzFetchError::NotLanPeer(peer.ip()));
    }
    let host_str = ip_to_host_str(peer.ip(), cx.frame());
    let hp = HostPort::new(host_str.as_str(), peer.port()).map_err(|_| SegzFetchError::InvalidHost)?;
    let conn_cx = cx.until(clock.after(CONN_WAIT));
    let conn = net.connect(&conn_cx, &Addr::Tcp(hp)).map_err(|e| SegzFetchError::Net(e.kind))?;

    let hex_bytes = lower_hex_64(&expected.0);
    let hex_str = core::str::from_utf8(&hex_bytes).map_err(|_| SegzFetchError::BadResponse("digest hex"))?;
    let req = bstd::mem::format_in!(
        cx.frame(),
        "GET {SEGZ_ROUTE_PREFIX}{hex_str} HTTP/1.1\r\nHost: {peer}\r\nConnection: close\r\n\r\n"
    );
    if let Err(err) = conn.send_all(&conn_cx, req.as_str().as_bytes()) {
        let _ = conn.finish(&cx.until(clock.after(CLOSE_WAIT)));
        return Err(SegzFetchError::Net(err.kind));
    }

    let mut head_buf = [0u8; MAX_REQUEST_HEAD];
    let mut head_got = 0usize;
    let head_end = loop {
        let so_far = head_buf.get(..head_got).unwrap_or(&[]);
        if let Some(pos) = so_far.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        let Some(tail) = head_buf.get_mut(head_got..) else {
            let _ = conn.finish(&cx.until(clock.after(CLOSE_WAIT)));
            return Err(SegzFetchError::BadResponse("response head exceeded bound"));
        };
        if tail.is_empty() {
            let _ = conn.finish(&cx.until(clock.after(CLOSE_WAIT)));
            return Err(SegzFetchError::BadResponse("response head exceeded bound"));
        }
        match conn.read_into(tail).wait(&conn_cx) {
            Ok(0) => {
                let _ = conn.finish(&cx.until(clock.after(CLOSE_WAIT)));
                return Err(SegzFetchError::BadResponse("truncated response head"));
            }
            Ok(n) => head_got += n,
            Err(err) => {
                let _ = conn.finish(&cx.until(clock.after(CLOSE_WAIT)));
                return Err(SegzFetchError::Net(err.kind));
            }
        }
    };

    let head_str = head_buf
        .get(..head_end)
        .and_then(|b| core::str::from_utf8(b).ok())
        .ok_or(SegzFetchError::BadResponse("non-UTF-8 response head"))?;
    let status = head_str
        .lines()
        .next()
        .and_then(|l| l.split_ascii_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or(SegzFetchError::BadResponse("missing HTTP status"))?;
    if status != 200 {
        let _ = conn.finish(&cx.until(clock.after(CLOSE_WAIT)));
        return Err(SegzFetchError::HttpStatus(status));
    }
    let content_len = head_str
        .lines()
        .skip(1)
        .find_map(|line| {
            let (name, val) = line.split_once(':')?;
            name.trim().eq_ignore_ascii_case("content-length").then(|| val.trim().parse::<u64>().ok())?
        })
        .ok_or(SegzFetchError::BadResponse("missing Content-Length"))?;
    if content_len > MAX_SEGZ_BYTES {
        let _ = conn.finish(&cx.until(clock.after(CLOSE_WAIT)));
        return Err(SegzFetchError::TooLarge { len: content_len, most: MAX_SEGZ_BYTES });
    }
    let target_len = usize::try_from(content_len).map_err(|_| SegzFetchError::TooLarge { len: content_len, most: MAX_SEGZ_BYTES })?;

    wire_out.clear();
    wire_out.resize(target_len, 0);
    let body_start = head_end + 4;
    let initial = head_buf.get(body_start..head_got).unwrap_or(&[]);
    if initial.len() > target_len {
        let _ = conn.finish(&cx.until(clock.after(CLOSE_WAIT)));
        return Err(SegzFetchError::BadResponse("extra bytes past Content-Length"));
    }
    if let Some(dst) = wire_out.as_mut_slice().get_mut(..initial.len()) {
        dst.copy_from_slice(initial);
    }
    let mut filled = initial.len();
    let mut reads = 0usize;
    while filled < target_len {
        if reads >= SEGZ_FETCH_READS_MOST {
            let _ = conn.finish(&cx.until(clock.after(CLOSE_WAIT)));
            return Err(SegzFetchError::BadResponse("exceeded bounded read count"));
        }
        reads += 1;
        let Some(tail) = wire_out.as_mut_slice().get_mut(filled..target_len) else {
            let _ = conn.finish(&cx.until(clock.after(CLOSE_WAIT)));
            return Err(SegzFetchError::BadResponse("body slice"));
        };
        match conn.read_into(tail).wait(&conn_cx) {
            Ok(0) => {
                let _ = conn.finish(&cx.until(clock.after(CLOSE_WAIT)));
                return Err(SegzFetchError::BadResponse("truncated BZS1 body"));
            }
            Ok(n) => filled += n,
            Err(err) => {
                let _ = conn.finish(&cx.until(clock.after(CLOSE_WAIT)));
                return Err(SegzFetchError::Net(err.kind));
            }
        }
    }
    let _ = conn.finish(&cx.until(clock.after(CLOSE_WAIT)));

    // Verify full BZS1 structure AND streaming BLPS BLAKE3 digest BEFORE returning any record!
    let verified = decode_wire_segment_verified(wire_out.as_slice(), max_raw_len, alloc).map_err(SegzFetchError::Wire)?;
    let described = verified.described();
    if described.namespace != Namespace::PUBLIC {
        return Err(SegzFetchError::NotPublic(described.namespace));
    }
    if described.digest != expected {
        return Err(SegzFetchError::DigestMismatch { wanted: expected, got: described.digest });
    }
    Ok(verified)
}

/// Entry point for `blaze peer [--bind <ip>:0] [--udp-port 41729] [--idle-secs 1800] [--segments <dir>]`.
#[expect(clippy::too_many_arguments, reason = "top-level subcommand entry takes explicit capabilities (rule 75)")]
pub(crate) fn run(
    cx: &Cx,
    args: &Args,
    fs: &Fs,
    env: Option<&Env>,
    net: Option<&Net>,
    clock: &Clock,
    rng: &mut Rng,
    out: &Out,
) -> Result<Status, CliError> {
    let cfg = parse_args(args).map_err(CliError::Peer)?;
    let net = net.ok_or(CliError::Peer(PeerError::NoNet))?;
    let frame = cx.frame();

    let env_var = |name: &'static str| -> Option<&str> {
        let key = VarName::new(name).ok()?;
        env?.get(&key).ok().flatten()
    };
    let cargo_home = match (env_var("CARGO_HOME"), env_var("HOME")) {
        (Some(cargo), _) => bstd::mem::format_in!(frame, "{cargo}"),
        (None, Some(home)) => bstd::mem::format_in!(frame, "{}/.cargo", home.trim_end_matches('/')),
        (None, None) => return Err(CliError::Peer(PeerError::NoCargoHome)),
    };

    let (registry, index) = scan_cargo_cache(cx, fs, cargo_home.as_str(), frame).map_err(CliError::Peer)?;
    let (segments_dir, segz_index) = match segments_dir_arg(args, &cfg) {
        Some(dir_arg) => {
            let opened = fs
                .open_dir(cx, &Path::new(dir_arg).map_err(PeerError::Path).map_err(CliError::Peer)?)
                .map_err(PeerError::Fs)
                .map_err(CliError::Peer)?
                .read_only();
            let segs = scan_segz_dir(cx, &opened, frame).map_err(CliError::Peer)?;
            (Some(opened), segs)
        }
        None => (None, OwnVec::new_in(frame)),
    };
    let (listener, tcp_port, first_conn) =
        open_listener(cx, net, clock, rng, cfg.bind_ip, cfg.bind_port, frame).map_err(CliError::Peer)?;
    let (udp, joined_mcast) =
        open_udp(cx, net, cfg.udp_ip, cfg.udp_port, frame).map_err(CliError::Peer)?;

    let mcast_note = if joined_mcast { "multicast 239.255.77.77 + broadcast" } else { "unicast/broadcast" };
    out.line(
        cx,
        format_args!(
            "blaze peer: serving {} crate(s) and {} BZS1 segment(s) from {}/{CARGO_CACHE} on http://{}:{tcp_port} (udp {}:{}, {mcast_note}, idle expiry {}s)",
            index.len(),
            segz_index.len(),
            cargo_home.as_str(),
            cfg.bind_ip,
            cfg.udp_ip,
            cfg.udp_port,
            cfg.idle_expiry.as_secs(),
        ),
    )?;

    serve_loop(
        cx,
        clock,
        listener,
        udp,
        tcp_port,
        first_conn,
        &registry,
        index.as_slice(),
        segments_dir.as_ref(),
        segz_index.as_slice(),
        cfg.idle_expiry,
        None,
        false,
    )
    .map_err(CliError::Peer)?;
    out.line(cx, format_args!("blaze peer: idle expiry reached, exiting"))?;
    Ok(Status::OK)
}

#[cfg(test)]
mod tests {
    use core::net::{IpAddr, Ipv4Addr, SocketAddr};
    use core::num::NonZeroU32;
    use core::sync::atomic::{AtomicBool, Ordering};
    use core::time::Duration;

    use blaze_drive::fetch_task;
    use blaze_fetch::pack::PackView;
    use blaze_fetch::plan::Unit;
    use blaze_own::{Alloc, String as OwnString};
    use blaze_tar::{Make, Writer};
    use blaze_text::Writes as _;
    use bstd::fs::{Content, Durable};
    use bstd::io::Io;
    use bstd::mem::{Bytes, TASK_FRAME};

    use super::*;
    use crate::fetch::{FetchEnv, LanPeerConfig, Lockfile};
    use crate::work::Did;

    fn make_crate_bytes(name: &str, version: &str, lib_rs: &[u8]) -> OwnVec<u8, Alloc<'static>> {
        let alloc = blaze_own::testing::heap().alloc();
        let mut tar = OwnVec::new_in(alloc);
        let mut w = Writer::new();
        let mut dir = OwnString::new_in(alloc);
        dir.str(name).str("-").str(version);
        w.entry(dir.as_str(), Make::Dir, &[], &mut tar).expect("dir");

        let mut manifest_path = OwnString::new_in(alloc);
        manifest_path.str(dir.as_str()).str("/Cargo.toml");
        let mut manifest = OwnString::new_in(alloc);
        manifest.str("[package]\nname = \"").str(name).str("\"\nversion = \"").str(version).str("\"\n");
        w.entry(manifest_path.as_str(), Make::File { exec: false }, manifest.as_str().as_bytes(), &mut tar)
            .expect("manifest");

        let mut lib_path = OwnString::new_in(alloc);
        lib_path.str(dir.as_str()).str("/src/lib.rs");
        w.entry(lib_path.as_str(), Make::File { exec: false }, lib_rs, &mut tar).expect("lib");
        w.finish(&mut tar).expect("tar");
        let mut gz = OwnVec::new_in(alloc);
        blaze_deflate::gzip(tar.as_slice(), blaze_deflate::Level::Fast, alloc, &mut gz).expect("gzip");
        gz
    }

    fn lock_for_crate(name: &str, version: &str, digest: &[u8; 32]) -> OwnString<Alloc<'static>> {
        let mut s = OwnString::new_in(blaze_own::testing::heap().alloc());
        s.str("version = 4\n\n[[package]]\nname = \"app\"\nversion = \"0.1.0\"\ndependencies = [\n \"")
            .str(name)
            .str("\",\n]\n\n[[package]]\nname = \"")
            .str(name)
            .str("\"\nversion = \"")
            .str(version)
            .str("\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"")
            .hex_bytes(digest)
            .str("\"\n");
        s
    }

    /// Outcome of a loopback peer fetch test (flattened so `Did<'f>` doesn't escape the frame).
    enum LoopbackDid {
        DoneClean,
        DoneWithFindings,
        NotDone,
    }

    /// Runs a real loopback `blaze peer` server on a bstd worker thread and drives
    /// `crate::fetch::run` from the parent thread against it (zero `std::fs` / `std::thread`).
    fn run_loopback_case(
        label: &str,
        crate_name: &str,
        lib_rs: &[u8],
        udp_port: u16,
        corrupt: bool,
    ) -> (LoopbackDid, Option<OwnVec<u8, Alloc<'static>>>, [u8; 32]) {
        let io: Io = bstd::test::native("/tmp".as_ref()).expect("native world over /tmp");
        let workers = NonZeroU32::new(2).expect("two workers");
        let archive = make_crate_bytes(crate_name, "0.1.0", lib_rs);
        let digest = sha256(archive.as_slice());

        io.with_test_workers(workers, |cx| {
            let fs = io.need_fs().expect("fs");
            let clock = io.need_clock().expect("clock");
            let mut rng = io.need_rng().expect("rng");
            let tag = rng.next_u64() ^ u64::from(udp_port);

            let sub = bstd::mem::format_in!(cx.frame(), "blaze-peer-test-{label}-{tag:x}");
            let peer_cargo_abs = bstd::mem::format_in!(cx.frame(), "/tmp/{}/peer-cargo", sub.as_str());
            let client_home_abs = bstd::mem::format_in!(cx.frame(), "/tmp/{}/client-home", sub.as_str());
            let client_cargo_abs = bstd::mem::format_in!(cx.frame(), "/tmp/{}/client-cargo", sub.as_str());
            let crate_rel = bstd::mem::format_in!(
                cx.frame(),
                "{}/peer-cargo/registry/cache/index.crates.io-1949cf8c6b5b557f/{crate_name}-0.1.0.crate",
                sub.as_str(),
            );
            let client_marker_rel = bstd::mem::format_in!(cx.frame(), "{}/client-home/.keep", sub.as_str());
            let cargo_marker_rel = bstd::mem::format_in!(cx.frame(), "{}/client-cargo/.keep", sub.as_str());
            fs.root()
                .publish(cx, &Path::new(crate_rel.as_str()).expect("crate path"), Content::bytes(archive.as_slice()), Durable::No)
                .expect("write cached crate");
            fs.root()
                .publish(cx, &Path::new(client_marker_rel.as_str()).expect("home marker"), Content::bytes(b""), Durable::No)
                .expect("mkdir client home");
            fs.root()
                .publish(cx, &Path::new(cargo_marker_rel.as_str()).expect("cargo marker"), Content::bytes(b""), Durable::No)
                .expect("mkdir client cargo");

            let short_cx = cx.until(clock.after(Duration::from_secs(5)));
            let cx = &short_cx;
            let stop = AtomicBool::new(false);
            let (ready_tx, ready_rx) = blaze_core::chan::bounded::<SocketAddr>(1);
            let scope = cx.scope();
            let peer_cargo_str = peer_cargo_abs.as_str();
            let stop_ref = &stop;
            let peer_task = scope
                .must_overlap("blaze-peer-loopback", TASK_FRAME, move |_| -> Result<(), PeerError> {
                    // Own `Io` world for the peer (mirrors a separate `blaze peer` process so the
                    // client's `Io` and the peer's `Io` never share a driver mutex).
                    let peer_io: Io = bstd::test::native("/tmp".as_ref()).expect("peer native world");
                    let peer_cx = peer_io.cx(Duration::from_secs(30), TASK_FRAME).expect("peer cx");
                    let peer_fs = peer_io.need_fs().expect("peer fs");
                    let peer_net = peer_io.need_net().expect("peer net");
                    let peer_clock = peer_io.need_clock().expect("peer clock");
                    let mut peer_rng = peer_io.need_rng().expect("peer rng");
                    let (registry, index) = scan_cargo_cache(&peer_cx, &peer_fs, peer_cargo_str, peer_cx.frame())?;
                    assert_eq!(index.len(), 1, "peer must have indexed the cached .crate");
                    let ip = IpAddr::V4(Ipv4Addr::LOCALHOST);
                    let (listener, tcp_port, first_conn) =
                        open_listener(&peer_cx, &peer_net, &peer_clock, &mut peer_rng, ip, 0, peer_cx.frame())?;
                    let (udp, _) = open_udp(&peer_cx, &peer_net, ip, udp_port, peer_cx.frame())?;
                    _ = ready_tx.send(SocketAddr::new(ip, udp_port));
                    serve_loop(
                        &peer_cx,
                        &peer_clock,
                        listener,
                        udp,
                        tcp_port,
                        first_conn,
                        &registry,
                        index.as_slice(),
                        None,
                        &[],
                        Duration::from_secs(30),
                        Some(stop_ref),
                        corrupt,
                    )
                })
                .expect("spawn peer task on worker pool");

            let udp_addr = ready_rx.recv_timeout(Duration::from_secs(5)).expect("peer started");

            let lock = lock_for_crate(crate_name, "0.1.0", &digest);
            let mut lockfile = Lockfile::read(lock.as_str().as_bytes(), blaze_own::testing::heap(), cx.frame()).expect("plan");
            let planned_unit = {
                let planned = blaze_fetch::plan::plan(lockfile.lock, blaze_own::testing::heap().alloc()).expect("plan");
                planned.units.into_iter().find(|u| matches!(u, Unit::Crate { .. })).expect("unit")
            };
            let task_name = fetch_task::name(&planned_unit);
            let env = FetchEnv {
                clock: io.need_clock().expect("clock"),
                checkout: "",
                served: crate::engine_script_reads::Served::Simulated,
                cert_file: None,
                cert_dir: None,
                rustflags: blaze_manifest::rustflags::RustflagsEnv::default(),
                home: Some(client_home_abs.as_str()),
                cargo_home: Some(client_cargo_abs.as_str()),
                start: None,
                toolchain: blaze_toolchain::PINNED,
                fs: io.need_fs().expect("fs"),
                sdk: crate::sdk::NONE_IN_TESTS,
                exe_dir: Err(crate::fetch::ExeDirWhy::NotRead),
                lan_peer: LanPeerConfig { enabled: true, udp_port, unicast_target: Some(udp_addr) },
                heap: blaze_own::testing::heap(),
            };

            let did = crate::fetch::run(cx, &mut lockfile, &env, task_name, cx.frame()).expect("fetch run");
            stop.store(true, Ordering::Relaxed);
            let _ = peer_task.join(cx);
            let _ = scope.join(cx);
            let outcome = match did {
                Did::Done(record) if record.findings.is_empty() => LoopbackDid::DoneClean,
                Did::Done(_) => LoopbackDid::DoneWithFindings,
                Did::Failed(failure) => {
                    if !corrupt {
                        panic!("loopback honest peer fetch failed: {failure:?}");
                    }
                    LoopbackDid::NotDone
                }
                Did::FailedSaying(failure, words) | Did::Hole(failure, words) => {
                    if !corrupt {
                        panic!("loopback honest peer fetch failed: {failure:?}: {words}");
                    }
                    LoopbackDid::NotDone
                }
                Did::Need(_) => {
                    if !corrupt {
                        panic!("loopback honest peer fetch returned Did::Need");
                    }
                    LoopbackDid::NotDone
                }
            };

            let key_bytes = crate::fetch::key_hex(&planned_unit.key());
            let key_str = core::str::from_utf8(&key_bytes).expect("hex");
            let stored_rel = bstd::mem::format_in!(cx.frame(), "{}/client-home/.blaze/v2/sources/{key_str}", sub.as_str());
            let stored = {
                let mut rec = Bytes::new_in(cx.frame(), "read stored BLZFPK03");
                if fs.root().read_all(cx, &Path::new(stored_rel.as_str()).expect("stored path"), &mut rec).is_ok() {
                    let mut copy = OwnVec::new_in(blaze_own::testing::heap().alloc());
                    copy.extend_from_slice(rec.as_slice());
                    Some(copy)
                } else {
                    None
                }
            };

            // Best-effort cleanup of the temporary files under /tmp.
            let _ = fs.root().remove(cx, &Path::new(crate_rel.as_str()).expect("p"), Durable::No);
            let _ = fs.root().remove(cx, &Path::new(stored_rel.as_str()).expect("p"), Durable::No);
            let _ = fs.root().remove(cx, &Path::new(client_marker_rel.as_str()).expect("p"), Durable::No);
            let _ = fs.root().remove(cx, &Path::new(cargo_marker_rel.as_str()).expect("p"), Durable::No);

            (outcome, stored, digest)
        })
    }

    /// End-to-end loopback test:
    /// - A real `blaze peer` scans a peer `CARGO_HOME` containing `lan_demo-0.1.0.crate`,
    ///   responds to a `BLZP` `HAVE` probe with `HAS`, and serves `GET /v1/crate/<sha256>`.
    /// - A client with an empty sources store and empty local cargo cache fetches `lan_demo`
    ///   through `crate::fetch::run`, receiving the verified `.crate` from the loopback peer,
    ///   verifying SHA-256, and writing a valid `BLZFPK03` record into `<HOME>/.blaze/v2/sources`.
    #[test]
    fn loopback_peer_serves_verified_crate_to_client_fetch() {
        let lib = b"pub fn from_lan() -> u32 { 7 }\n";
        let (did, stored, digest) = run_loopback_case("honest", "lan_demo", lib, 43729, false);
        assert!(matches!(did, LoopbackDid::DoneClean), "expected clean Did::Done from loopback peer");
        let rec = stored.expect("valid BLZFPK03 stored by client fetch");
        let mut ver = OwnString::new_in(blaze_own::testing::heap().alloc());
        ver.str("0.1.0");
        let unit = Unit::Crate { package: blaze_manifest::lock::LockId(0), name: "lan_demo", version: ver, sha256: digest };
        let view = PackView::open_source(&unit.source(), rec.as_slice()).expect("valid BLZFPK04");
        let file = view.read("src/lib.rs", blaze_own::testing::heap().alloc()).expect("read").expect("src/lib.rs");
        assert_eq!(file.bytes.as_slice(), &lib[..]);
    }

    /// End-to-end loopback security test:
    /// - A corrupted loopback peer flips a byte of the `.crate` before serving it.
    /// - The client's SHA-256 verification catches the corruption, bans the peer, NEVER writes
    ///   corrupted bytes into `<HOME>/.blaze/v2/sources`, and falls through cleanly.
    #[test]
    fn corrupted_loopback_peer_is_rejected_and_never_stored() {
        let name = "blaze_nonexistent_corrupt_peer_test_crate_99999";
        let (did, stored, _digest) = run_loopback_case("corrupt", name, b"pub fn honest() {}\n", 44729, true);
        assert!(
            matches!(did, LoopbackDid::NotDone),
            "corrupted peer bytes MUST NEVER produce Did::Done"
        );
        assert!(
            stored.is_none(),
            "corrupted peer bytes MUST NEVER be written into the sources store"
        );
    }

    #[test]
    fn lan_peer_config_honors_opt_out_env() {
        assert!(LanPeerConfig::from_env(None).enabled);
        assert!(LanPeerConfig::from_env(Some("")).enabled);
        assert!(LanPeerConfig::from_env(Some("0")).enabled);
        assert!(LanPeerConfig::from_env(Some("false")).enabled);
        assert!(!LanPeerConfig::from_env(Some("1")).enabled);
        assert!(!LanPeerConfig::from_env(Some("true")).enabled);
    }

    #[test]
    fn segz_wire_protocol_round_trip_and_http_routing() {
        let d1 = ContentId::of("peer segz test", b"one");
        let d2 = ContentId::of("peer segz test", b"two");
        let mut buf = [0u8; HAVE_MAX_BYTES];
        assert!(encode_have_segz(1, &[], &mut buf).is_none());
        let len = encode_have_segz(0xfeed_face_cafe_babe, &[d1, d2], &mut buf).expect("encode HAVE_SEGZ");
        assert_eq!(len, HAVE_HEADER_BYTES + 64);
        assert!(decode_have(&buf[..len]).is_none(), "HAVE_SEGZ must not decode as crate HAVE");
        let have = decode_have_segz(&buf[..len]).expect("decode HAVE_SEGZ");
        assert_eq!(have.nonce, 0xfeed_face_cafe_babe);
        assert_eq!(have.len(), 2);
        assert_eq!(have.hash(0), Some(d1));
        assert_eq!(have.hash(1), Some(d2));

        let has_wire = encode_has_segz(HasPacket { tcp_port: 41800, nonce: 77, mask: 0b10, active_transfers: 1 });
        let has = decode_has_segz(&has_wire).expect("decode HAS_SEGZ");
        assert_eq!(has.tcp_port, 41800);
        assert_eq!(has.nonce, 77);
        assert_eq!(has.mask, 0b10);
        assert_eq!(has.active_transfers, 1);

        let hex = lower_hex_64(&d1.0);
        let hex_str = core::str::from_utf8(&hex).expect("hex");
        let alloc = blaze_own::testing::heap().alloc();
        let req = blaze_own::format_in!(alloc, "GET /v1/segz/{hex_str} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n");
        assert_eq!(parse_peer_http_request(req.as_bytes()), PeerHttpRequest::GetSegz(d1));
        assert_eq!(
            parse_peer_http_request(b"GET /v1/segz/../../etc/passwd HTTP/1.1\r\nHost: x\r\n\r\n"),
            PeerHttpRequest::NotFound
        );
        assert_eq!(
            parse_peer_http_request(b"POST /v1/segz/0000000000000000000000000000000000000000000000000000000000000000 HTTP/1.1\r\n\r\n"),
            PeerHttpRequest::MethodNotAllowed
        );
    }

    fn make_test_segment(namespace: Namespace, alloc: Alloc<'static>) -> (OwnVec<u8, Alloc<'static>>, OwnVec<u8, Alloc<'static>>, ContentId, OwnVec<(ContentId, blaze_keyed::kind::Tag, OwnVec<u8, Alloc<'static>>), Alloc<'static>>) {
        let mut records: OwnVec<(ContentId, blaze_keyed::kind::Tag, OwnVec<u8, Alloc<'static>>), Alloc<'static>> = OwnVec::new_in(alloc);
        for i in 0..8u8 {
            let key = ContentId::of("peer bzs1 segment record", &[i]);
            let mut payload = OwnVec::new_in(alloc);
            for _ in 0..32 {
                payload.extend_from_slice(b"repetitive compiler record bytes ");
                payload.push(i);
            }
            records.push((key, blaze_keyed::kind::Tag(3), payload));
        }
        records.sort_by_key(|(k, _, _)| *k);
        let mut refs: OwnVec<(blaze_keyed::exec::RecordRef, &[u8]), Alloc<'static>> = OwnVec::with_capacity_in(records.len(), alloc);
        for (k, t, p) in records.iter() {
            refs.push((blaze_keyed::exec::RecordRef { key: *k, tag: *t }, p.as_slice()));
        }
        let mut raw_buf = Default::default();
        blaze_cas::plain::seal_segment(namespace, &refs, &mut raw_buf).expect("seal segment");
        let desc = check_segment(&raw_buf).expect("check sealed");
        let mut raw = OwnVec::new_in(alloc);
        raw.extend_from_slice(&raw_buf);
        let mut wire = OwnVec::new_in(alloc);
        encode_wire_segment(raw.as_slice(), &mut wire, alloc).expect("encode BZS1");
        (raw, wire, desc.digest, records)
    }

    fn run_segz_loopback(label: &str, udp_port: u16, corrupt: bool) {
        let alloc = blaze_own::testing::heap().alloc();
        let (raw, wire, digest, records) = make_test_segment(Namespace::PUBLIC, alloc);
        assert!(
            wire.len() < raw.len(),
            "BZS1 wire segment ({} B) must eliminate uncompressed BLPS bloat ({} B)",
            wire.len(),
            raw.len()
        );

        let io: Io = bstd::test::native("/tmp".as_ref()).expect("native world over /tmp");
        let workers = NonZeroU32::new(2).expect("two workers");
        io.with_test_workers(workers, |cx| {
            let fs = io.need_fs().expect("fs");
            let net = io.need_net().expect("net");
            let clock = io.need_clock().expect("clock");
            let mut rng = io.need_rng().expect("rng");
            let tag = rng.next_u64() ^ u64::from(udp_port);

            let sub = bstd::mem::format_in!(cx.frame(), "blaze-peer-segz-{label}-{tag:x}");
            let peer_cargo_abs = bstd::mem::format_in!(cx.frame(), "/tmp/{}/peer-cargo", sub.as_str());
            let cargo_marker_rel = bstd::mem::format_in!(cx.frame(), "{}/peer-cargo/registry/.keep", sub.as_str());
            let hex = lower_hex_64(&digest.0);
            let hex_str = core::str::from_utf8(&hex).expect("hex");
            let segz_rel = bstd::mem::format_in!(cx.frame(), "{}/segs/segz-{hex_str}", sub.as_str());
            let segs_dir_rel = bstd::mem::format_in!(cx.frame(), "{}/segs", sub.as_str());

            fs.root()
                .publish(cx, &Path::new(cargo_marker_rel.as_str()).expect("cargo marker"), Content::bytes(b""), Durable::No)
                .expect("mkdir cargo");
            fs.root()
                .publish(cx, &Path::new(segz_rel.as_str()).expect("segz path"), Content::bytes(wire.as_slice()), Durable::No)
                .expect("write segz file");

            let short_cx = cx.until(clock.after(Duration::from_secs(5)));
            let cx = &short_cx;
            let stop = AtomicBool::new(false);
            let (ready_tx, ready_rx) = blaze_core::chan::bounded::<SocketAddr>(1);
            let scope = cx.scope();
            let peer_cargo_str = peer_cargo_abs.as_str();
            let segs_rel_str = segs_dir_rel.as_str();
            let stop_ref = &stop;

            let peer_task = scope
                .must_overlap("blaze-peer-segz-loopback", TASK_FRAME, move |_| -> Result<(), PeerError> {
                    let peer_io: Io = bstd::test::native("/tmp".as_ref()).expect("peer native world");
                    let peer_cx = peer_io.cx(Duration::from_secs(30), TASK_FRAME).expect("peer cx");
                    let peer_fs = peer_io.need_fs().expect("peer fs");
                    let peer_net = peer_io.need_net().expect("peer net");
                    let peer_clock = peer_io.need_clock().expect("peer clock");
                    let mut peer_rng = peer_io.need_rng().expect("peer rng");
                    let (registry, index) = scan_cargo_cache(&peer_cx, &peer_fs, peer_cargo_str, peer_cx.frame())?;
                    let seg_dir = peer_fs.open_dir(&peer_cx, &Path::new(segs_rel_str)?)?.read_only();
                    let segz_index = scan_segz_dir(&peer_cx, &seg_dir, peer_cx.frame())?;
                    assert_eq!(segz_index.len(), 1, "peer must advertise the BZS1 segment");
                    let ip = IpAddr::V4(Ipv4Addr::LOCALHOST);
                    let (listener, tcp_port, first_conn) =
                        open_listener(&peer_cx, &peer_net, &peer_clock, &mut peer_rng, ip, 0, peer_cx.frame())?;
                    let (udp, _) = open_udp(&peer_cx, &peer_net, ip, udp_port, peer_cx.frame())?;
                    _ = ready_tx.send(SocketAddr::new(ip, udp_port));
                    serve_loop(
                        &peer_cx,
                        &peer_clock,
                        listener,
                        udp,
                        tcp_port,
                        first_conn,
                        &registry,
                        index.as_slice(),
                        Some(&seg_dir),
                        segz_index.as_slice(),
                        Duration::from_secs(30),
                        Some(stop_ref),
                        corrupt,
                    )
                })
                .expect("spawn peer segz task");

            let udp_addr = ready_rx.recv_timeout(Duration::from_secs(5)).expect("peer started");
            let found = probe_lan_segz(cx, &net, &clock, &mut rng, &[udp_addr], &[digest], Duration::from_secs(2))
                .expect("probe_lan_segz")
                .expect("peer must answer HAS_SEGZ");
            assert_eq!(found.mask, 1);

            let mut wire_buf = OwnVec::new_in(alloc);
            let fetched = fetch_verified_segz_from_peer(
                cx,
                &net,
                &clock,
                found.addr,
                digest,
                u64::try_from(raw.len()).unwrap_or(MAX_SEGZ_BYTES),
                &mut wire_buf,
                alloc,
            );
            stop.store(true, Ordering::Relaxed);
            let _ = peer_task.join(cx);
            let _ = scope.join(cx);

            if corrupt {
                assert!(fetched.is_err(), "corrupted BZS1 peer response MUST be rejected");
            } else {
                let verified = fetched.expect("verified BZS1 segment from loopback peer");
                assert_eq!(verified.described().digest, digest);
                assert_eq!(verified.len(), records.len());
                for ((got_k, got_t, got_p), (want_k, want_t, want_p)) in verified.iter().zip(records.iter()) {
                    assert_eq!((got_k, got_t, got_p), (*want_k, *want_t, want_p.as_slice()));
                }
                drop(verified);
                assert_eq!(wire_buf.len(), wire.len(), "transfer must be compressed BZS1, not raw BLPS");
            }

            let _ = fs.root().remove(cx, &Path::new(segz_rel.as_str()).expect("p"), Durable::No);
            let _ = fs.root().remove(cx, &Path::new(cargo_marker_rel.as_str()).expect("p"), Durable::No);
        });
    }

    #[test]
    fn loopback_peer_advertises_serves_and_fetches_verified_bzs1_segment() {
        run_segz_loopback("honest", 45729, false);
    }

    #[test]
    fn corrupted_loopback_peer_bzs1_segment_is_rejected() {
        run_segz_loopback("corrupt", 46729, true);
    }

    #[test]
    fn scan_segz_dir_refuses_private_local_segments() {
        let alloc = blaze_own::testing::heap().alloc();
        let (raw, wire, digest, _) = make_test_segment(Namespace::LOCAL, alloc);
        let io = Io::simulated(bstd::io::SimWorld::new(91, bstd::io::Limits::USUAL));
        let cx = io.cx(bstd::test::BUDGET, TASK_FRAME).expect("cx");
        let fs = io.need_fs().expect("fs");
        let hex = lower_hex_64(&digest.0);
        let hex_str = core::str::from_utf8(&hex).expect("hex");
        let segz_path = bstd::mem::format_in!(cx.frame(), "priv/segz-{hex_str}");
        let raw_path = bstd::mem::format_in!(cx.frame(), "priv/seg-{hex_str}");
        fs.root().publish(&cx, &Path::new(segz_path.as_str()).expect("p"), Content::bytes(wire.as_slice()), Durable::No).expect("w");
        fs.root().publish(&cx, &Path::new(raw_path.as_str()).expect("p"), Content::bytes(raw.as_slice()), Durable::No).expect("w");
        let dir = fs.open_dir(&cx, &Path::new("priv").expect("p")).expect("dir").read_only();
        let indexed = scan_segz_dir(&cx, &dir, cx.frame()).expect("scan");
        assert!(indexed.is_empty(), "Namespace::LOCAL segments must NEVER be advertised over LAN P2P");
    }
}
