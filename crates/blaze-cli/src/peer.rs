//! `blaze peer [--bind <ip>:0] [--udp-port 41729] [--idle-secs 1800]`: zero-trust LAN `.crate`
//! cache peer server.
//!
//! On startup, scans `<CARGO_HOME>/registry/cache/<crates.io-folder>/*.crate` once, hashes each
//! archive with `blaze_fetch::peer::sha256` into an in-memory `[u8; 32] -> Path` index (zero path
//! traversal — HTTP lookup is strictly by exact 32-byte SHA-256), listens on UDP for `BLZP`
//! `HAVE` probes (replying unicast `HAS` with its TCP port and match bitmask), and serves raw
//! `.crate` bytes at `GET /v1/crate/<64-lower-hex-sha256>` over `bstd::net::Listener`.
//!
//! Holds no state between restarts and exits cleanly after its idle expiry (rule 70).

use core::fmt;
use core::net::{IpAddr, Ipv4Addr, SocketAddr};
use core::sync::atomic::{AtomicBool, Ordering};
use core::time::Duration;

use blaze_fetch::local::{CARGO_CACHE, crates_io_folder};
use blaze_fetch::peer::{
    DEFAULT_UDP_PORT, HasPacket, MAX_REQUEST_HEAD, MULTICAST_GROUP, PeerRequest, decode_have, encode_has,
    error_response, parse_http_request, sha256, write_ok_head,
};
use blaze_own::Vec as OwnVec;
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
            PeerError::Usage(msg) => write!(f, "usage: blaze peer [--bind <ip>:0] [--udp-port 41729] [--idle-secs 1800]: {msg}"),
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
}

fn parse_args(args: &Args) -> Result<PeerConfig, PeerError> {
    let mut bind_ip = IpAddr::V4(Ipv4Addr::UNSPECIFIED);
    let mut bind_port: u16 = 0;
    let mut udp_ip: Option<IpAddr> = None;
    let mut udp_port: u16 = DEFAULT_UDP_PORT;
    let mut idle_secs: u64 = DEFAULT_IDLE_SECS;

    let mut at = 2;
    while let Some(arg) = args.get(at) {
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
    })
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

/// Serves one HTTP/1.1 connection (`GET /v1/crate/<64-lower-hex>`), enforcing bounded header
/// length (`MAX_REQUEST_HEAD`) and bounded per-connection deadline (`CONN_WAIT`).
fn serve_conn(
    cx: &Cx,
    clock: &Clock,
    conn: Conn,
    registry: &Dir<ReadOnly>,
    index: CrateTable<'_>,
    corrupt_for_test: bool,
) {
    let conn_cx = cx.until(clock.after(CONN_WAIT));
    let mut head_buf = [0u8; MAX_REQUEST_HEAD];
    let mut got = 0usize;
    let parsed = loop {
        let so_far = head_buf.get(..got).unwrap_or(&[]);
        match parse_http_request(so_far) {
            PeerRequest::NeedMore => {
                let Some(tail) = head_buf.get_mut(got..) else {
                    break PeerRequest::BadRequest;
                };
                if tail.is_empty() {
                    break PeerRequest::BadRequest;
                }
                match conn.read_into(tail).wait(&conn_cx) {
                    Ok(0) => break PeerRequest::BadRequest,
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
        PeerRequest::GetCrate(digest) => match lookup_crate(index, &digest) {
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
        PeerRequest::NotFound => {
            let _ = conn.send_all(&conn_cx, error_response(404));
        }
        PeerRequest::MethodNotAllowed => {
            let _ = conn.send_all(&conn_cx, error_response(405));
        }
        PeerRequest::BadRequest | PeerRequest::NeedMore => {
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
    idle_expiry: Duration,
    stop: Option<&AtomicBool>,
    corrupt_for_test: bool,
) -> Result<(), PeerError> {
    let mut last_active = clock.now();
    if let Some(conn) = first_conn {
        serve_conn(cx, clock, conn, registry, index, corrupt_for_test);
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
                    let Some(have) = decode_have(raw) else { continue };
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
                serve_conn(cx, clock, conn, registry, index, corrupt_for_test);
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

/// Entry point for `blaze peer [--bind <ip>:0] [--udp-port 41729] [--idle-secs 1800]`.
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
    let (listener, tcp_port, first_conn) =
        open_listener(cx, net, clock, rng, cfg.bind_ip, cfg.bind_port, frame).map_err(CliError::Peer)?;
    let (udp, joined_mcast) =
        open_udp(cx, net, cfg.udp_ip, cfg.udp_port, frame).map_err(CliError::Peer)?;

    let mcast_note = if joined_mcast { "multicast 239.255.77.77 + broadcast" } else { "unicast/broadcast" };
    out.line(
        cx,
        format_args!(
            "blaze peer: serving {} crate(s) from {}/{CARGO_CACHE} on http://{}:{tcp_port} (udp {}:{}, {mcast_note}, idle expiry {}s)",
            index.len(),
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
        let view = PackView::open_source(&unit.source(), rec.as_slice()).expect("valid BLZFPK03");
        assert_eq!(view.find("src/lib.rs").map(|f| f.bytes), Some(&lib[..]));
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
}
