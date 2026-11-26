//! Workspace binary launcher stubs (`CARGO_BIN_EXE_<name>`) for `blaze test` (`test.md` piece 17).
//!
//! No executable binary is compiled or written to disk (bottom.md section 11). For each workspace
//! `[[bin]]` exposed via `CARGO_BIN_EXE_<name>` to any selected integration-test/benchmark suite,
//! `blaze test`:
//! 1. Creates a short, user-private (`0o700`) socket directory `/tmp/bzb-<nonce>` (independent of
//!    `$HOME` length, always well within `sockaddr_un::sun_path`'s 104/108-byte limit), binds a
//!    non-blocking `AF_UNIX` listener at `<sock_dir>/s`, and authenticates every accepted peer's
//!    effective UID (`SO_PEERCRED` / `getpeereid`).
//! 2. Places a launcher stub `<mine>/b/<package>/<bin_name>` (hardlink to `blaze-launcher`, with a
//!    copy+`chmod 0o755` fallback across mount points) beside a `mode=run` sidecar
//!    `<stub>.blaze-program` naming `socket`, `bin=<id>`, `store`, `entry`, and optional `native`.
//! 3. On Linux, bind-mounts `<mine>/b` read-only at `/target/debug/bin` inside the sandbox's
//!    private mount namespace (`pivot_root`) so `/target/debug/bin/<package>/<bin_name>` is a real
//!    kernel-visible executable file (`File::open`, `std::fs::read`, `/bin/sh -c`, `Command::new`
//!    all work natively) and sets runtime `CARGO_BIN_EXE_<bin_name>` to that exact path. On macOS,
//!    sets runtime `CARGO_BIN_EXE_<bin_name>=<host_stub>` and installs `BLAZE_LAUNCH_BIN_MAP` for
//!    compile-time literal remapping inside `LauncherOwn`.
//! 4. Multiplexes all concurrent child binary connections through a sans-I/O protocol state machine
//!    ([`BinProtocol`], capped at [`MAX_CONNS`] peers and [`max_inbox`]/[`MAX_OUTBOX`] wire
//!    buffers) driven by non-blocking `poll` inside [`BinServer::pump`], propagating host-side
//!    serving failures with their `(suite, row)` owner and joining each binary's entered functions
//!    and purity back to its spawning test row.

use core::cell::RefCell;
use core::ffi::CStr;
use core::fmt::{self, Write as _};

use blaze_ladder::code_records::Builder;
use blaze_ladder::run::make_one::{MakeWatch, NamesRecords};
use blaze_ladder::run::special::{Roots, run_roots};
use blaze_own::{AString, AVec, Alloc, String, Vec};
use blaze_run::plan::NotKept;
use blaze_screen::Watch;
use blaze_sys::{Errno, Timespec};
use bstd::Cx;
use bstd::fs::{Content, Dir, Durable, FsError, FsKind, ReadWrite};
use bstd::mem::Frame;
use bstd::path::{Path, PathError};

use crate::code_store::HomeCodeStore;
use crate::run_ids::RunIds;
use crate::run_parent::{Codes, Needs, ParentError, Tally, WireOut, wire_out};
use crate::store::CommandLent;
use crate::test_cache::EnteredFn;
use crate::test_drive::DriveError;
use crate::test_drive::suite_cache::{ChildAnswer, ChildResolver, ChildSent, SuiteCache};

/// Maximum poll/serve bursts per `BinServer::pump` tick so active binaries stream at full speed
/// without starving test child turns in `OneEach`.
const PUMP_BURSTS: usize = 64;

/// Short poll window (250 µs) used ONLY after a burst already made progress, so a binary child
/// placing a code record and immediately firing its next `Up::Need` wakes `poll` in ~30 µs instead
/// of falling back to `OneEach`'s 1 ms idle interval.
const FOLLOW_UP_POLL: Timespec = Timespec { sec: 0, nsec: 250_000 };

/// Maximum bytes read from one binary connection per burst.
const READ_CHUNK: usize = 16 * 1024;

/// Maximum concurrent connected workspace binary peers (rule 18: bounded parent memory).
const MAX_CONNS: usize = 64;

/// Maximum unread/unparsed bytes buffered per peer (one max wire frame plus read headroom).
fn max_inbox() -> usize {
    usize::try_from(blaze_launch::wire::LONGEST_FRAME)
        .unwrap_or(64 * 1024)
        .saturating_add(64 * 1024)
}

/// Maximum queued outbound wire bytes buffered per peer.
const MAX_OUTBOX: usize = 64 * 1024 * 1024;

/// Compaction threshold for already-written outbound prefix bytes.
const OUTBOX_COMPACT_AT: usize = 64 * 1024;

/// Fixed handshake sent by `blaze-launcher` `mode=run` (`[bin_id: u32, suite: u32, row: u32]` LE).
const HANDSHAKE_LEN: usize = 12;

/// A workspace binary target ready to be served to tests via a `CARGO_BIN_EXE_<name>` stub.
pub(crate) struct BinSuite<'s, 'h> {
    pub package: &'s str,
    pub bin_name: &'s str,
    pub key: blaze_id::CrateItemKey,
    pub names: NamesRecords<'h>,
    pub builder: Builder,
    pub native: Option<crate::engine_native::NativeSend<'s>>,
}

/// Why binary launcher stubs could not be prepared before running tests.
#[derive(Debug)]
pub(crate) enum BinStubError {
    Roots { package: AString<'static>, bin: AString<'static>, why: blaze_ladder::run::special::RootsError },
    Parent { package: AString<'static>, bin: AString<'static>, why: ParentError },
    Cache(blaze_own::ABox<'static, DriveError>),
    Path(PathError),
    Fs(FsError),
    Socket { path: AString<'static>, errno: Errno },
    Stub { path: AString<'static>, errno: Errno },
    PathInvalid { path: AString<'static> },
}

/// Typed host-side failure while serving code to a spawned workspace binary during `blaze test`.
#[derive(Debug)]
pub(crate) struct BinServeError {
    /// Owning `(suite, row)` from the completed 12-byte handshake, or `None` if the failure
    /// occurred before a valid owner handshake completed.
    pub owner: Option<(u32, u32)>,
    pub package: AString<'static>,
    pub bin: AString<'static>,
    pub why: BinServeWhy,
}

#[derive(Debug)]
pub(crate) enum BinServeWhy {
    Parent(ParentError),
    Protocol(&'static str),
}

impl fmt::Display for BinServeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.why {
            BinServeWhy::Parent(err) => write!(f, "binary `{}/{}` code serving failed: {err}", self.package, self.bin),
            BinServeWhy::Protocol(msg) => write!(f, "binary `{}/{}` protocol limit exceeded: {msg}", self.package, self.bin),
        }
    }
}

fn owned_text(s: &str, alloc: Alloc<'static>) -> AString<'static> {
    String::from_str_in(s, alloc)
}

impl fmt::Display for BinStubError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BinStubError::Roots { package, bin, why } => {
                write!(f, "CARGO_BIN_EXE stub for `{package}/{bin}`: computing program roots: {why}")
            }
            BinStubError::Parent { package, bin, why } => {
                write!(f, "CARGO_BIN_EXE stub for `{package}/{bin}`: {why}")
            }
            BinStubError::Cache(why) => write!(f, "CARGO_BIN_EXE stub verified cache: {why}"),
            BinStubError::Path(why) => write!(f, "CARGO_BIN_EXE stub path: {why}"),
            BinStubError::Fs(why) => write!(f, "CARGO_BIN_EXE stub folder/file: {why}"),
            BinStubError::Socket { path, errno } => {
                write!(f, "CARGO_BIN_EXE binary server socket `{path}`: {errno}")
            }
            BinStubError::Stub { path, errno } => {
                write!(f, "CARGO_BIN_EXE launcher stub `{path}`: {errno}")
            }
            BinStubError::PathInvalid { path } => {
                write!(f, "CARGO_BIN_EXE path `{path}` is too long or contains a NUL byte")
            }
        }
    }
}

/// RAII cleanup for the private `/tmp/bzb-<nonce>` socket directory so partial setup failures
/// and normal drops never leave stale directories or sockets behind.
struct PrivateSocketDir {
    dir: AString<'static>,
    socket: AString<'static>,
    tables: Alloc<'static>,
}

impl Drop for PrivateSocketDir {
    fn drop(&mut self) {
        use blaze_sys::posix::io::CWD;
        use blaze_sys::posix::proc::{Unlink, unlink_at};
        let _ = with_cstr(self.socket.as_str(), self.tables, |c| {
            let _ = unlink_at(CWD, blaze_sys::Path::new(c), Unlink::File);
            Ok(())
        });
        let _ = with_cstr(self.dir.as_str(), self.tables, |c| {
            let _ = unlink_at(CWD, blaze_sys::Path::new(c), Unlink::Dir);
            Ok(())
        });
    }
}

struct BinRuntime<'a, 'h> {
    package: &'a str,
    bin_name: &'a str,
    cache_id: blaze_ids::ContentId,
    env_name: AString<'a>,
    guest_path: AString<'a>,
    host_path: AString<'a>,
    roots: Roots<'static>,
    entry_sym: [u8; 32],
    native: Option<crate::engine_native::NativeSend<'a>>,
    suite_cache: RefCell<SuiteCache<'h>>,
}

struct RealSession<'a> {
    needs: Needs<'a>,
    child_sent: ChildSent<'a>,
    tally: Tally,
}

/// Sans-I/O backend seam driven by [`BinProtocol`] (production: [`RealBackend`] over
/// [`SuiteCache`] + `Needs`; DST: deterministic simulated backend with fault injection).
trait BinBackend<'a> {
    type Session;

    fn bin_count(&self) -> usize;
    fn bin_names(&self, bin_id: usize) -> (&str, &str);

    /// Initializes per-peer state once the 12-byte handshake completes, writing initial frames
    /// (`Down::Native`, `Down::Personality`, bounded warm prepublish) into `outbox`.
    fn on_handshake(
        &mut self,
        bin_id: usize,
        owner: Option<(u32, u32)>,
        keep: &mut Result<(), NotKept>,
        outbox: &mut WireOut<Alloc<'a>>,
        out_at: usize,
        max_outbox: usize,
    ) -> Result<Self::Session, BinServeError>;

    /// Serves one complete frame from `inbox` into `outbox`, returning `(bytes_consumed, stop_loop)`.
    /// Returns `(0, true)` when `inbox` holds an incomplete frame waiting for more bytes.
    #[expect(clippy::too_many_arguments, reason = "pure protocol step over one peer's buffers and evidence")]
    fn serve_frame(
        &mut self,
        bin_id: usize,
        owner: Option<(u32, u32)>,
        session: &mut Self::Session,
        inbox: &[u8],
        outbox: &mut WireOut<Alloc<'a>>,
        out_at: usize,
        max_outbox: usize,
        keep: &mut Result<(), NotKept>,
        entered_fns: &mut AVec<'a, EnteredFn<'a>>,
    ) -> Result<(usize, bool), BinServeError>;
}

struct PeerConn<'a, S, T> {
    id: u64,
    transport: T,
    bin_id: Option<usize>,
    owner: Option<(u32, u32)>,
    handshake: [u8; HANDSHAKE_LEN],
    handshake_len: usize,
    session: Option<S>,
    inbox: WireOut<Alloc<'a>>,
    outbox: WireOut<Alloc<'a>>,
    out_at: usize,
    keep: Result<(), NotKept>,
    entered_fns: AVec<'a, EnteredFn<'a>>,
    eof: bool,
    dead: bool,
}

struct OwnerEvidence<'a> {
    suite: u32,
    row: u32,
    keep: Result<(), NotKept>,
    entered_fns: AVec<'a, EnteredFn<'a>>,
}

/// Sans-I/O multi-peer binary stub protocol state machine (rule 14: sans-io-cores).
///
/// Owns peer admission (`same_uid` + `max_conns`), incremental 12-byte handshake assembly,
/// bounded per-peer inbox/outbox buffers, outbound flush compaction, connection reaping, and
/// per-owner `(suite, row)` evidence joining (marking any row with a still-live helper connection
/// unkeepable so post-exit helper work can never be silently omitted from a cached test).
struct BinProtocol<'a, S, T> {
    next_id: u64,
    max_conns: usize,
    max_inbox: usize,
    max_outbox: usize,
    conns: AVec<'a, PeerConn<'a, S, T>>,
    evidence: AVec<'a, OwnerEvidence<'a>>,
    tables: Alloc<'static>,
    alloc: Alloc<'a>,
}

impl<'a, S, T> BinProtocol<'a, S, T> {
    fn new(
        max_conns: usize,
        max_inbox: usize,
        max_outbox: usize,
        tables: Alloc<'static>,
        alloc: Alloc<'a>,
    ) -> Self {
        BinProtocol {
            next_id: 1,
            max_conns,
            max_inbox,
            max_outbox,
            conns: Vec::new_in(alloc),
            evidence: Vec::new_in(alloc),
            tables,
            alloc,
        }
    }

    fn conns(&self) -> &[PeerConn<'a, S, T>] {
        self.conns.as_slice()
    }

    fn conn(&self, idx: usize) -> Option<&PeerConn<'a, S, T>> {
        self.conns.get(idx)
    }

    /// Admits a newly accepted peer iff `same_uid` holds and active connections are below
    /// `max_conns` (rule 18). Returns the monotonic peer id when accepted, or `None` after
    /// dropping `transport`.
    fn accept(&mut self, same_uid: bool, transport: T) -> Option<u64> {
        if !same_uid || self.conns.len() >= self.max_conns {
            drop(transport);
            return None;
        }
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        self.conns.push(PeerConn {
            id,
            transport,
            bin_id: None,
            owner: None,
            handshake: [0u8; HANDSHAKE_LEN],
            handshake_len: 0,
            session: None,
            inbox: wire_out(self.alloc),
            outbox: wire_out(self.alloc),
            out_at: 0,
            keep: Ok(()),
            entered_fns: Vec::new_in(self.alloc),
            eof: false,
            dead: false,
        });
        Some(id)
    }

    fn pending_out(&self, idx: usize) -> &[u8] {
        self.conns
            .get(idx)
            .and_then(|c| c.outbox.get(c.out_at..))
            .unwrap_or(&[])
    }

    fn inbox_room(&self, idx: usize) -> usize {
        let Some(conn) = self.conns.get(idx) else { return 0 };
        if conn.bin_id.is_none() {
            // Allow reading the remaining handshake bytes plus up to `max_inbox + 1` so an
            // immediate overflow after the handshake is detected cleanly on the next `feed_bytes`.
            let hs_left = HANDSHAKE_LEN.saturating_sub(conn.handshake_len);
            hs_left.saturating_add(self.max_inbox).saturating_add(1)
        } else {
            self.max_inbox.saturating_sub(conn.inbox.len()).saturating_add(1)
        }
    }

    fn ack_out(&mut self, idx: usize, wrote: usize) -> bool {
        let Some(conn) = self.conns.get_mut(idx) else { return false };
        if wrote == 0 {
            return false;
        }
        conn.out_at = conn.out_at.saturating_add(wrote).min(conn.outbox.len());
        if conn.out_at >= conn.outbox.len() {
            conn.outbox.clear();
            conn.out_at = 0;
        } else if conn.out_at > OUTBOX_COMPACT_AT {
            let left = conn.outbox.len() - conn.out_at;
            conn.outbox.copy_within(conn.out_at.., 0);
            conn.outbox.truncate(left);
            conn.out_at = 0;
        }
        if conn.eof && conn.out_at >= conn.outbox.len() {
            conn.dead = true;
        }
        true
    }

    fn on_eof(&mut self, idx: usize) {
        if let Some(conn) = self.conns.get_mut(idx) {
            conn.eof = true;
            if conn.out_at >= conn.outbox.len() {
                conn.dead = true;
            }
        }
    }

    fn on_io_error(&mut self, idx: usize) {
        if let Some(conn) = self.conns.get_mut(idx) {
            conn.dead = true;
        }
    }

    /// Feeds `chunk` bytes received from peer `idx` through handshake assembly, bounded inbox
    /// buffering, and backend frame serving. Marks the peer dead on unknown `bin_id` or any
    /// `BinServeError`.
    fn feed_bytes<B: BinBackend<'a, Session = S>>(
        &mut self,
        idx: usize,
        chunk: &[u8],
        backend: &mut B,
    ) -> Result<bool, BinServeError> {
        let max_inbox = self.max_inbox;
        let max_outbox = self.max_outbox;
        let tables = self.tables;
        let Some(conn) = self.conns.get_mut(idx) else { return Ok(false) };
        if conn.dead {
            return Ok(false);
        }
        let mut rest = chunk;
        let mut progress = false;

        // 1. Complete 12-byte LE `[bin_id, owner_suite, owner_row]` handshake.
        if conn.bin_id.is_none() && !rest.is_empty() {
            let need = HANDSHAKE_LEN.saturating_sub(conn.handshake_len);
            let take = need.min(rest.len());
            if let (Some(dst), Some(src)) = (
                conn.handshake.get_mut(conn.handshake_len..conn.handshake_len + take),
                rest.get(..take),
            ) {
                dst.copy_from_slice(src);
                conn.handshake_len += take;
                rest = rest.get(take..).unwrap_or(&[]);
                progress = true;
            }
            if conn.handshake_len == HANDSHAKE_LEN {
                let [b0, b1, b2, b3, s0, s1, s2, s3, r0, r1, r2, r3] = conn.handshake;
                let id = usize::try_from(u32::from_le_bytes([b0, b1, b2, b3])).unwrap_or(usize::MAX);
                let owner_suite = u32::from_le_bytes([s0, s1, s2, s3]);
                let owner_row = u32::from_le_bytes([r0, r1, r2, r3]);
                if id >= backend.bin_count() {
                    conn.dead = true;
                    return Ok(progress);
                }
                conn.bin_id = Some(id);
                if owner_suite != u32::MAX && owner_row != u32::MAX {
                    conn.owner = Some((owner_suite, owner_row));
                }
                let session = match backend.on_handshake(
                    id,
                    conn.owner,
                    &mut conn.keep,
                    &mut conn.outbox,
                    conn.out_at,
                    max_outbox,
                ) {
                    Ok(s) => s,
                    Err(err) => {
                        conn.dead = true;
                        return Err(err);
                    }
                };
                conn.session = Some(session);
                if conn.outbox.len().saturating_sub(conn.out_at) > max_outbox {
                    conn.dead = true;
                    let (pkg, bin) = backend.bin_names(id);
                    return Err(BinServeError {
                        owner: conn.owner,
                        package: owned_text(pkg, tables),
                        bin: owned_text(bin, tables),
                        why: BinServeWhy::Protocol("peer outbox exceeded MAX_OUTBOX"),
                    });
                }
            }
        }

        // 2. Append remaining payload bytes into the bounded per-peer inbox.
        if !rest.is_empty() && !conn.dead {
            if conn.inbox.len().saturating_add(rest.len()) > max_inbox {
                conn.dead = true;
                let (pkg, bin) = conn
                    .bin_id
                    .filter(|&id| id < backend.bin_count())
                    .map_or(("<unknown>", "<unknown>"), |id| backend.bin_names(id));
                return Err(BinServeError {
                    owner: conn.owner,
                    package: owned_text(pkg, tables),
                    bin: owned_text(bin, tables),
                    why: BinServeWhy::Protocol("peer inbox exceeded MAX_INBOX"),
                });
            }
            conn.inbox.extend_from_slice(rest);
            progress = true;
        }

        // 3. Serve complete frames from `inbox`.
        if !conn.dead
            && !conn.inbox.is_empty()
            && let (Some(bin_id), Some(session)) = (conn.bin_id, conn.session.as_mut())
        {
            loop {
                match backend.serve_frame(
                    bin_id,
                    conn.owner,
                    session,
                    conn.inbox.as_slice(),
                    &mut conn.outbox,
                    conn.out_at,
                    max_outbox,
                    &mut conn.keep,
                    &mut conn.entered_fns,
                ) {
                    Ok((0, _)) => break,
                    Ok((taken, stop)) => {
                        let left = conn.inbox.len().saturating_sub(taken);
                        conn.inbox.copy_within(taken.., 0);
                        conn.inbox.truncate(left);
                        progress = true;
                        if conn.outbox.len().saturating_sub(conn.out_at) > max_outbox {
                            conn.dead = true;
                            let (pkg, bin) = backend.bin_names(bin_id);
                            return Err(BinServeError {
                                owner: conn.owner,
                                package: owned_text(pkg, tables),
                                bin: owned_text(bin, tables),
                                why: BinServeWhy::Protocol("peer outbox exceeded MAX_OUTBOX"),
                            });
                        }
                        if stop || conn.inbox.is_empty() {
                            break;
                        }
                    }
                    Err(err) => {
                        conn.dead = true;
                        return Err(err);
                    }
                }
            }
        }
        Ok(progress)
    }

    /// Reaps every dead peer, moving its accumulated `(suite, row)` evidence into `self.evidence`.
    fn reap_dead(&mut self) {
        let mut i = 0;
        while i < self.conns.len() {
            if self.conns.get(i).is_some_and(|c| c.dead) {
                let dead = self.conns.remove_swap(i);
                if let Some((suite, row)) = dead.owner {
                    self.evidence.push(OwnerEvidence {
                        suite,
                        row,
                        keep: dead.keep,
                        entered_fns: dead.entered_fns,
                    });
                }
            } else {
                i += 1;
            }
        }
    }

    /// Joins entered functions and purity recorded by workspace binaries spawned under
    /// `(suite, row)` into the spawning test's `entered_fns` and `keep` verdict.
    ///
    /// Conservatively marks the row unkeepable (`Err(NotKept::Untraced)` unless already impure)
    /// whenever ANY helper connection might still produce post-exit work for this row:
    /// - a live connection owned by `(suite, row)` (`!conn.dead && conn.owner == Some((suite, row))`),
    /// - a live connection whose 12-byte handshake has not completed yet (`!conn.dead && conn.bin_id.is_none()`), or
    /// - an unaccepted peer queued on the listening socket (`has_queued_accept == true`).
    /// (Any test process that actually calls `posix_spawn`/`fork`/`clone` is also already marked
    /// `Err(NotKept::Foreign)` at the syscall boundary before this join runs.)
    fn drain_owner_evidence(
        &mut self,
        suite: u32,
        row: u32,
        has_queued_accept: bool,
        entered_fns: &mut AVec<'a, EnteredFn<'a>>,
        keep: &mut Result<(), NotKept>,
    ) {
        if has_queued_accept && keep.is_ok() {
            *keep = Err(NotKept::Untraced);
        }
        for conn in self.conns.iter_mut() {
            if !conn.dead && conn.bin_id.is_none() {
                if keep.is_ok() {
                    *keep = Err(NotKept::Untraced);
                }
            } else if conn.owner == Some((suite, row)) {
                if !conn.dead {
                    if keep.is_ok() {
                        *keep = Err(conn.keep.err().unwrap_or(NotKept::Untraced));
                    }
                } else if let Err(why) = conn.keep
                    && keep.is_ok()
                {
                    *keep = Err(why);
                }
                while let Some(entry) = conn.entered_fns.pop() {
                    entered_fns.push(entry);
                }
            }
        }
        let mut i = 0;
        while i < self.evidence.len() {
            let matches = self.evidence.get(i).is_some_and(|ev| ev.suite == suite && ev.row == row);
            if matches {
                let mut ev = self.evidence.remove_swap(i);
                if let Err(why) = ev.keep
                    && keep.is_ok()
                {
                    *keep = Err(why);
                }
                while let Some(entry) = ev.entered_fns.pop() {
                    entered_fns.push(entry);
                }
            } else {
                i += 1;
            }
        }
    }
}

struct RealBackend<'r, 'a, 'h> {
    bins: &'r [BinRuntime<'a, 'h>],
    cx: &'r Cx,
    ids: &'r RunIds<'h>,
    lent: &'r CommandLent,
    watch: &'r dyn Watch,
    tables: Alloc<'static>,
    alloc: Alloc<'a>,
}

impl<'a, 'h> BinBackend<'a> for RealBackend<'_, 'a, 'h> {
    type Session = RealSession<'a>;

    fn bin_count(&self) -> usize {
        self.bins.len()
    }

    fn bin_names(&self, bin_id: usize) -> (&str, &str) {
        self.bins
            .get(bin_id)
            .map_or(("<unknown>", "<unknown>"), |b| (b.package, b.bin_name))
    }

    fn on_handshake(
        &mut self,
        bin_id: usize,
        owner: Option<(u32, u32)>,
        keep: &mut Result<(), NotKept>,
        outbox: &mut WireOut<Alloc<'a>>,
        out_at: usize,
        max_outbox: usize,
    ) -> Result<Self::Session, BinServeError> {
        let Some(bin) = self.bins.get(bin_id) else {
            return Err(BinServeError {
                owner,
                package: owned_text("<unknown>", self.tables),
                bin: owned_text("<unknown>", self.tables),
                why: BinServeWhy::Protocol("invalid binary id"),
            });
        };
        let mut session = RealSession {
            needs: Needs::new(self.alloc),
            child_sent: ChildSent::new(self.alloc),
            tally: Tally::default(),
        };
        let eager_native = bin
            .native
            .is_some_and(|send| send.load_mode == crate::engine_native::NativeLoadMode::Eager);
        *keep = if eager_native { Err(NotKept::Foreign) } else { Ok(()) };
        let head = bin.roots.head();
        let make_err = |why| BinServeError {
            owner,
            package: owned_text(bin.package, self.tables),
            bin: owned_text(bin.bin_name, self.tables),
            why: BinServeWhy::Parent(why),
        };
        if let Some(send) = bin.native {
            if eager_native {
                session.needs.tell_native(send.library, send.words, outbox).map_err(make_err)?;
            } else {
                session.needs.arm_lazy_native(send.library, send.words);
            }
        }
        session.needs.name_personality(head.personality, outbox).map_err(make_err)?;
        // Pre-publish warm functions already compiled by earlier spawns ONLY when this binary is
        // already impure (native image linked); bounded strictly by `max_outbox` (rule 18).
        if keep.is_err() {
            let cap = out_at.saturating_add(max_outbox);
            bin.suite_cache
                .borrow()
                .prepublish_warm_bounded(
                    bin.entry_sym,
                    head,
                    &mut session.child_sent,
                    &mut session.needs,
                    outbox,
                    &mut session.tally,
                    cap,
                    self.alloc,
                )
                .map_err(make_err)?;
        }
        Ok(session)
    }

    fn serve_frame(
        &mut self,
        bin_id: usize,
        owner: Option<(u32, u32)>,
        session: &mut Self::Session,
        inbox: &[u8],
        outbox: &mut WireOut<Alloc<'a>>,
        out_at: usize,
        max_outbox: usize,
        keep: &mut Result<(), NotKept>,
        entered_fns: &mut AVec<'a, EnteredFn<'a>>,
    ) -> Result<(usize, bool), BinServeError> {
        let Some(bin) = self.bins.get(bin_id) else {
            return Ok((0, true));
        };
        let mut codes = HomeCodeStore::new(self.lent);
        let make_err = |why| BinServeError {
            owner,
            package: owned_text(bin.package, self.tables),
            bin: owned_text(bin.bin_name, self.tables),
            why: BinServeWhy::Parent(why),
        };
        let was_keep_ok = keep.is_ok();
        let mut resolver = ChildResolver::new(
            &bin.suite_cache,
            &mut session.child_sent,
            &bin.roots,
            bin.entry_sym,
            Codes::Serial,
            self.ids,
            &mut codes,
            self.cx,
            self.watch,
            &mut session.tally,
            keep,
            entered_fns,
            self.alloc,
            self.alloc,
        );
        let served = session.needs.serve(&mut resolver, inbox, outbox);
        let answer = resolver.requested.take();
        drop(resolver);
        match served {
            Ok(0) => Ok((0, true)),
            Ok(taken) => {
                if matches!(answer, Some(ChildAnswer::Unresolved(_))) {
                    return Ok((taken, true));
                }
                let warm_len = bin.suite_cache.borrow().warm_len();
                if keep.is_err()
                    && session.child_sent.warm_cursor < warm_len
                    && (was_keep_ok || warm_len.saturating_sub(session.child_sent.warm_cursor) >= 256)
                {
                    session.needs.flush_pending_native(outbox).map_err(make_err)?;
                    let cap = out_at.saturating_add(max_outbox);
                    bin.suite_cache
                        .borrow()
                        .prepublish_warm_bounded(
                            bin.entry_sym,
                            bin.roots.head(),
                            &mut session.child_sent,
                            &mut session.needs,
                            outbox,
                            &mut session.tally,
                            cap,
                            self.alloc,
                        )
                        .map_err(make_err)?;
                }
                Ok((taken, false))
            }
            Err(why) => Err(make_err(why)),
        }
    }
}

/// Active binary stub server owned by `run_dirty` for the duration of one `blaze test` command.
pub(crate) struct BinServer<'a, 'h> {
    bins_dir: AString<'a>,
    sock_dir_guard: PrivateSocketDir,
    listener: blaze_sys::posix::io::OwnedFd,
    bins: AVec<'a, BinRuntime<'a, 'h>>,
    #[cfg(target_os = "macos")]
    natives: AVec<'a, crate::engine_native::NativeSend<'a>>,
    proto: BinProtocol<'a, RealSession<'a>, blaze_sys::posix::io::OwnedFd>,
    pollfds: AVec<'a, blaze_sys::posix::io::PollFd>,
    tables: Alloc<'static>,
    alloc: Alloc<'a>,
}

impl<'a, 'h> BinServer<'a, 'h> {
    /// Creates `<mine>/b/`, creates a short private (`0o700`) `/tmp/bzb-<nonce>` socket dir and
    /// binds `<sock_dir>/s`, writes launcher stubs + `mode=run` sidecars for each `BinSuite`, and
    /// initializes per-binary `SuiteCache`s.
    #[expect(clippy::too_many_arguments, reason = "prepares the command's binary stub server from run_dirty's capabilities")]
    pub(crate) fn start(
        cx: &Cx,
        mine: &Dir<ReadWrite>,
        mine_abs: &str,
        sock_nonce: u64,
        launcher: &str,
        store: &str,
        bins: &[BinSuite<'a, 'h>],
        lent: &CommandLent,
        certificate: &crate::start_list::Certificate,
        typed: crate::engine_body_record::TypedFrom<'h>,
        tables: Alloc<'static>,
        make_watch: MakeWatch<'static>,
        frame: &Frame,
        alloc: Alloc<'a>,
    ) -> Result<BinServer<'a, 'h>, BinStubError> {
        let bins_rel = Path::new("b").map_err(BinStubError::Path)?;
        let bins_dir_handle = make_or_open_dir(cx, mine, &bins_rel)?;
        let mut bins_dir = String::new_in(alloc);
        let _ = write!(bins_dir, "{mine_abs}/b");

        let sock_dir_guard = create_private_socket_dir(sock_nonce, tables)?;
        let listener = blaze_sys::posix::net::listen_unix(sock_dir_guard.socket.as_bytes(), 128).map_err(|errno| {
            BinStubError::Socket { path: owned_text(sock_dir_guard.socket.as_str(), tables), errno }
        })?;
        // Lock the socket node itself to owner read/write only (`0o600`) inside the `0o700` dir.
        let _ = chmod_path(sock_dir_guard.socket.as_str(), 0o600, tables);

        let mut launcher_bytes: Option<AVec<'_, u8>> = None;
        let mut runtimes = Vec::with_capacity_in(bins.len(), alloc);
        #[cfg(target_os = "macos")]
        let mut natives = Vec::new_in(alloc);

        for (id, bin) in bins.iter().enumerate() {
            let exports = bin.native.map_or(&[][..], |send| send.items);
            let roots = run_roots(
                bin.names.records,
                bin.names.lang_crates,
                bin.builder.settings.panic,
                exports,
                bin.names.frame,
                tables,
                tables,
            )
            .map_err(|why| BinStubError::Roots {
                package: owned_text(bin.package, tables),
                bin: owned_text(bin.bin_name, tables),
                why,
            })?;
            let head = roots.head();
            let cache_id = crate::test_cache::bin_cache_id(bin.package, bin.bin_name, bin.key, &bin.builder);
            let mut sc = SuiteCache::new(bin.names, bin.builder, typed, tables, make_watch);
            crate::test_cache::load_verified_by_id(lent, cache_id, certificate, sc.verified_mut())
                .map_err(|why| BinStubError::Cache(blaze_own::Box::new_in(why, tables)))?;
            let entry_sym = sc.root_test(&roots).map_err(|why| BinStubError::Parent {
                package: owned_text(bin.package, tables),
                bin: owned_text(bin.bin_name, tables),
                why,
            })?;

            let pkg_rel = Path::new(bin.package).map_err(BinStubError::Path)?;
            let pkg_dir = make_or_open_dir(cx, &bins_dir_handle, &pkg_rel)?;
            let bin_rel = Path::new(bin.bin_name).map_err(BinStubError::Path)?;
            let mut host_path = String::new_in(alloc);
            let _ = write!(host_path, "{}/{}/{}", bins_dir.as_str(), bin.package, bin.bin_name);
            place_launcher_stub(cx, &pkg_dir, &bin_rel, launcher, host_path.as_str(), &mut launcher_bytes, tables, alloc)?;

            let sidecar_name = bstd::mem::format_in!(frame, "{}.blaze-program", bin.bin_name);
            let sidecar_rel = Path::new(sidecar_name.as_str()).map_err(BinStubError::Path)?;
            let mut sidecar = String::new_in(alloc);
            let _ = writeln!(sidecar, "mode=run");
            let _ = writeln!(sidecar, "socket={}", sock_dir_guard.socket.as_str());
            let _ = writeln!(sidecar, "bin={id}");
            let _ = writeln!(sidecar, "store={store}");
            let _ = writeln!(sidecar, "entry={}", head.line);
            if let Some(send) = bin.native {
                match send.load_mode {
                    crate::engine_native::NativeLoadMode::Eager => {
                        let _ = writeln!(sidecar, "native={}", send.hex);
                    }
                    crate::engine_native::NativeLoadMode::Lazy => {
                        let _ = writeln!(sidecar, "native=lazy:{}", send.hex);
                    }
                }
                #[cfg(target_os = "macos")]
                if !natives.iter().any(|existing: &crate::engine_native::NativeSend<'_>| existing.hex == send.hex) {
                    natives.push(send);
                }
            }
            pkg_dir
                .publish(cx, &sidecar_rel, Content::bytes(sidecar.as_bytes()), Durable::No)
                .map_err(BinStubError::Fs)?;

            let mut env_name = String::new_in(alloc);
            let _ = write!(env_name, "CARGO_BIN_EXE_{}", bin.bin_name);
            let mut guest_path = String::new_in(alloc);
            let _ = write!(
                guest_path,
                "{}{}/{}",
                blaze_scope::crate_env::BIN_EXE_ROOT,
                bin.package,
                bin.bin_name
            );
            runtimes.push(BinRuntime {
                package: bin.package,
                bin_name: bin.bin_name,
                cache_id,
                env_name,
                guest_path,
                host_path,
                roots,
                entry_sym,
                native: bin.native,
                suite_cache: RefCell::new(sc),
            });
        }

        Ok(BinServer {
            bins_dir,
            sock_dir_guard,
            listener,
            bins: runtimes,
            #[cfg(target_os = "macos")]
            natives,
            proto: BinProtocol::new(MAX_CONNS, max_inbox(), MAX_OUTBOX, tables, alloc),
            pollfds: Vec::new_in(alloc),
            tables,
            alloc,
        })
    }

    pub(crate) fn bins_dir(&self) -> &str {
        self.bins_dir.as_str()
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn sock_dir(&self) -> &str {
        self.sock_dir_guard.dir.as_str()
    }

    /// Persists any newly verified `(instance_key, content_key)` pairs (`BZTV`) for each binary
    /// served during this command under today's `certificate`.
    pub(crate) fn save_verified(
        &self,
        lent: &CommandLent,
        certificate: &crate::start_list::Certificate,
    ) -> Result<(), DriveError> {
        for bin in self.bins.iter() {
            let sc = bin.suite_cache.borrow();
            if sc.verified_dirty() {
                crate::test_cache::save_verified_by_id(lent, bin.cache_id, certificate, sc.verified_pairs(), self.alloc)?;
            }
        }
        Ok(())
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn socket_path(&self) -> &str {
        self.sock_dir_guard.socket.as_str()
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn bin_natives(&self) -> &[crate::engine_native::NativeSend<'a>] {
        self.natives.as_slice()
    }

    /// Appends runtime `CARGO_BIN_EXE_<bin_name>` pairs for `package`. On Linux, runtime values
    /// point at `/target/debug/bin/<package>/<bin_name>` (physically bind-mounted read-only inside
    /// the sandbox) and no remap table is needed; on macOS, runtime values point at `<host_stub>`
    /// and a `BLAZE_LAUNCH_BIN_MAP` table is returned for compile-time literal remapping.
    pub(crate) fn fill_suite_env<'s, 'f>(
        &'s self,
        package: &str,
        vars: &mut Vec<(&'s str, &'s str), &'f Frame>,
        frame: &'f Frame,
    ) -> (bool, Option<String<&'f Frame>>) {
        #[cfg(not(target_os = "linux"))]
        let mut map = String::new_in(frame);
        let mut any = false;
        for bin in self.bins.iter() {
            if bin.package != package {
                continue;
            }
            any = true;
            #[cfg(target_os = "linux")]
            {
                let _ = frame;
                vars.push((bin.env_name.as_str(), bin.guest_path.as_str()));
            }
            #[cfg(not(target_os = "linux"))]
            {
                vars.push((bin.env_name.as_str(), bin.host_path.as_str()));
                let _ = writeln!(map, "{}\t{}", bin.guest_path.as_str(), bin.host_path.as_str());
            }
        }
        #[cfg(target_os = "linux")]
        return (any, None);
        #[cfg(not(target_os = "linux"))]
        return (any, any.then_some(map));
    }

    /// Joins entered functions and purity recorded by workspace binaries spawned under
    /// `(suite, row)` into the spawning test's `entered_fns` and `keep` verdict.
    pub(crate) fn drain_owner_evidence(
        &mut self,
        suite: u32,
        row: u32,
        entered_fns: &mut AVec<'a, EnteredFn<'a>>,
        keep: &mut Result<(), NotKept>,
    ) {
        use blaze_sys::posix::io::{PollEvents, PollFd, poll};

        // Check non-blockingly whether any peer is already queued on the listening socket waiting
        // to be accepted (fail-closed on `poll` error).
        let mut pfd = [PollFd::new(self.listener.fd(), PollEvents::IN)];
        let zero = Some(Timespec { sec: 0, nsec: 0 });
        let has_queued_accept = match poll(&mut pfd, zero) {
            Ok(ready) => ready > 0 && pfd[0].found().contains(PollEvents::IN),
            Err(_) => true,
        };
        self.proto.drain_owner_evidence(suite, row, has_queued_accept, entered_fns, keep);
    }

    /// Non-blocking poll/accept/read/serve/write pump across the listener and every connected
    /// workspace binary process. Returns `Ok(true)` if any connection was accepted or made
    /// progress, or `Err(BinServeError)` if host-side code serving failed.
    pub(crate) fn pump(
        &mut self,
        cx: &Cx,
        ids: &RunIds<'h>,
        lent: &CommandLent,
        watch: &dyn Watch,
    ) -> Result<bool, BinServeError> {
        use blaze_sys::posix::io::{PollEvents, PollFd, poll};

        let mut backend = RealBackend {
            bins: self.bins.as_slice(),
            cx,
            ids,
            lent,
            watch,
            tables: self.tables,
            alloc: self.alloc,
        };
        let mut any_progress = false;
        let zero = Some(Timespec { sec: 0, nsec: 0 });
        let mut timeout = zero;
        let mut read_buf = [0u8; READ_CHUNK];
        for _ in 0..PUMP_BURSTS {
            self.pollfds.clear();
            self.pollfds.push(PollFd::new(self.listener.fd(), PollEvents::IN));
            for conn in self.proto.conns() {
                let mut ev = PollEvents::IN;
                if conn.out_at < conn.outbox.len() {
                    ev = ev.with(PollEvents::OUT);
                }
                self.pollfds.push(PollFd::new(conn.transport.fd(), ev));
            }
            let ready = match poll(self.pollfds.as_mut_slice(), timeout) {
                Ok(n) => n,
                Err(_) => break,
            };
            if ready == 0 {
                break;
            }
            let mut burst_progress = false;
            if let Some(listen_pfd) = self.pollfds.first()
                && listen_pfd.found().contains(PollEvents::IN)
            {
                loop {
                    match blaze_sys::posix::net::accept_unix(self.listener.fd()) {
                        Ok(fd) => {
                            let same_uid = blaze_sys::posix::net::peer_is_self_uid(fd.fd()).unwrap_or(false);
                            if self.proto.accept(same_uid, fd).is_some() {
                                burst_progress = true;
                            }
                        }
                        Err(Errno::Again) => break,
                        Err(_) => break,
                    }
                }
            }

            let conn_count = self.pollfds.len().saturating_sub(1);
            let mut serve_err: Option<BinServeError> = None;
            for idx in 0..conn_count {
                let Some(found) = self.pollfds.get(idx + 1).map(PollFd::found) else { continue };
                if found.bits() == 0 {
                    continue;
                }
                if found.contains(PollEvents::OUT) && !self.proto.pending_out(idx).is_empty() {
                    if flush_peer(&mut self.proto, idx) {
                        burst_progress = true;
                    }
                }
                let is_dead = self.proto.conn(idx).is_none_or(|c| c.dead);
                if !is_dead && (found.contains(PollEvents::IN) || found.contains(PollEvents::HUP)) {
                    for _ in 0..4 {
                        let Some(fd) = self.proto.conn(idx).filter(|c| !c.dead).map(|c| c.transport.fd()) else {
                            break;
                        };
                        let room = self.proto.inbox_room(idx).min(read_buf.len());
                        let Some(slice) = read_buf.get_mut(..room) else { break };
                        match blaze_sys::posix::io::read(fd, slice) {
                            Ok(0) => {
                                self.proto.on_eof(idx);
                                break;
                            }
                            Ok(n) => {
                                let Some(chunk) = read_buf.get(..n) else { break };
                                match self.proto.feed_bytes(idx, chunk, &mut backend) {
                                    Ok(prog) => {
                                        if prog {
                                            burst_progress = true;
                                        }
                                    }
                                    Err(err) => {
                                        if serve_err.is_none() {
                                            serve_err = Some(err);
                                        }
                                        break;
                                    }
                                }
                                if n < room {
                                    break;
                                }
                            }
                            Err(Errno::Again | Errno::Intr) => break,
                            Err(_) => {
                                self.proto.on_io_error(idx);
                                break;
                            }
                        }
                    }
                    if self.proto.conn(idx).is_some_and(|c| !c.dead)
                        && !self.proto.pending_out(idx).is_empty()
                        && flush_peer(&mut self.proto, idx)
                    {
                        burst_progress = true;
                    }
                }
                if found.contains(PollEvents::ERR) || found.contains(PollEvents::NVAL) {
                    self.proto.on_io_error(idx);
                }
            }

            self.proto.reap_dead();
            if let Some(err) = serve_err {
                return Err(err);
            }
            if burst_progress {
                any_progress = true;
                timeout = Some(FOLLOW_UP_POLL);
            } else {
                break;
            }
        }
        Ok(any_progress)
    }
}

fn flush_peer<S>(proto: &mut BinProtocol<'_, S, blaze_sys::posix::io::OwnedFd>, idx: usize) -> bool {
    let mut wrote_any = false;
    loop {
        let Some(fd) = proto.conn(idx).filter(|c| !c.dead).map(|c| c.transport.fd()) else { break };
        let pending = proto.pending_out(idx);
        if pending.is_empty() {
            break;
        }
        match blaze_sys::posix::io::write(fd, pending) {
            Ok(0) => {
                proto.on_io_error(idx);
                break;
            }
            Ok(n) => {
                if proto.ack_out(idx, n) {
                    wrote_any = true;
                }
            }
            Err(Errno::Again | Errno::Intr) => break,
            Err(_) => {
                proto.on_io_error(idx);
                break;
            }
        }
    }
    wrote_any
}

fn create_private_socket_dir(nonce: u64, tables: Alloc<'static>) -> Result<PrivateSocketDir, BinStubError> {
    let mut dir = String::new_in(tables);
    let _ = write!(dir, "/tmp/bzb-{nonce:016x}");
    let mut socket = String::new_in(tables);
    let _ = write!(socket, "{}/s", dir.as_str());
    with_cstr(dir.as_str(), tables, |cdir| {
        blaze_sys::posix::proc::make_dir_mode(blaze_sys::Path::new(cdir), 0o700)
            .map_err(|errno| BinStubError::Socket { path: owned_text(dir.as_str(), tables), errno })
    })?;
    let guard = PrivateSocketDir { dir, socket, tables };
    chmod_path(guard.dir.as_str(), 0o700, tables)?;
    Ok(guard)
}

fn make_or_open_dir(cx: &Cx, parent: &Dir<ReadWrite>, path: &Path) -> Result<Dir<ReadWrite>, BinStubError> {
    match parent.make_dir(cx, path, Durable::No) {
        Ok(dir) => Ok(dir),
        Err(err) if err.kind == FsKind::Exists => Ok(parent.subdir(path)),
        Err(err) => Err(BinStubError::Fs(err)),
    }
}

fn with_cstr<T>(
    path: &str,
    tables: Alloc<'static>,
    f: impl FnOnce(&CStr) -> Result<T, BinStubError>,
) -> Result<T, BinStubError> {
    let bytes = path.as_bytes();
    let mut buf = [0u8; 4096];
    if bytes.len() >= buf.len() || bytes.contains(&0) {
        return Err(BinStubError::PathInvalid { path: owned_text(path, tables) });
    }
    if let Some(dst) = buf.get_mut(..bytes.len()) {
        dst.copy_from_slice(bytes);
    }
    let Some(with_nul) = buf.get(..=bytes.len()) else {
        return Err(BinStubError::PathInvalid { path: owned_text(path, tables) });
    };
    let Ok(cstr) = CStr::from_bytes_with_nul(with_nul) else {
        return Err(BinStubError::PathInvalid { path: owned_text(path, tables) });
    };
    f(cstr)
}

#[expect(clippy::too_many_arguments, reason = "places one launcher stub using the command's directory, cache, and allocators")]
fn place_launcher_stub<'a>(
    cx: &Cx,
    pkg_dir: &Dir<ReadWrite>,
    bin_rel: &Path,
    launcher_abs: &str,
    stub_abs: &str,
    cached_bytes: &mut Option<AVec<'a, u8>>,
    tables: Alloc<'static>,
    alloc: Alloc<'a>,
) -> Result<(), BinStubError> {
    #[cfg(target_os = "linux")]
    {
        use blaze_sys::Path as SysPath;
        use blaze_sys::linux::{AtFlags, CWD, linkat};
        let linked = with_cstr(launcher_abs, tables, |from_c| {
            with_cstr(stub_abs, tables, |to_c| {
                Ok(linkat(CWD, SysPath::new(from_c), CWD, SysPath::new(to_c), AtFlags::NONE))
            })
        })?;
        match linked {
            Ok(()) => return Ok(()),
            Err(Errno::XDev | Errno::Perm | Errno::Acces) => {}
            Err(errno) => return Err(BinStubError::Stub { path: owned_text(stub_abs, tables), errno }),
        }
    }
    let bytes = match cached_bytes.as_ref() {
        Some(bytes) => bytes.as_slice(),
        None => {
            let loaded = read_file_posix(launcher_abs, tables, alloc)?;
            cached_bytes.insert(loaded).as_slice()
        }
    };
    pkg_dir
        .publish(cx, bin_rel, Content::bytes(bytes), Durable::No)
        .map_err(BinStubError::Fs)?;
    chmod_path(stub_abs, 0o755, tables)?;
    Ok(())
}

fn read_file_posix<'a>(path: &str, tables: Alloc<'static>, alloc: Alloc<'a>) -> Result<AVec<'a, u8>, BinStubError> {
    use blaze_sys::posix::io::{CWD, OpenFlags, openat, read};

    let fd = with_cstr(path, tables, |cstr| {
        openat(CWD, blaze_sys::Path::new(cstr), OpenFlags::READ.with(OpenFlags::CLOSE_ON_EXEC), 0)
            .map_err(|errno| BinStubError::Stub { path: owned_text(path, tables), errno })
    })?;
    let mut out = Vec::new_in(alloc);
    let mut chunk = [0u8; 64 * 1024];
    loop {
        match read(fd.fd(), &mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                if let Some(slice) = chunk.get(..n) {
                    out.extend_from_slice(slice);
                }
            }
            Err(Errno::Intr) => continue,
            Err(errno) => return Err(BinStubError::Stub { path: owned_text(path, tables), errno }),
        }
    }
    Ok(out)
}

fn chmod_path(path: &str, mode: blaze_sys::posix::types::mode_t, tables: Alloc<'static>) -> Result<(), BinStubError> {
    use blaze_sys::posix::io::{CWD, OpenFlags, openat};
    use blaze_sys::posix::proc::change_mode;

    with_cstr(path, tables, |cstr| {
        let fd = openat(CWD, blaze_sys::Path::new(cstr), OpenFlags::READ.with(OpenFlags::CLOSE_ON_EXEC), 0)
            .map_err(|errno| BinStubError::Stub { path: owned_text(path, tables), errno })?;
        change_mode(fd.fd(), mode)
            .map_err(|errno| BinStubError::Stub { path: owned_text(path, tables), errno })?;
        Ok(())
    })
}

#[cfg(test)]
mod dst_tests {
    use super::*;
    use blaze_hash::{FixedState, Set, SetOpts};
    use blaze_launch::wire::run::{Down, Needed, Symbol, Up};
    use blaze_launch::wire::{RecordId, split};
    use blaze_launch::{Slot, Tier};
    use blaze_run::postings::FnName;
    use blaze_typeck::ItemKey;

    use crate::start_list::Certificate;
    use crate::test_cache::{VerifiedDecode, decode_verified_status, encode_verified};

    /// Deterministic PRNG (xorshift64*) for reproducible DST interleavings.
    struct Rng(u64);

    impl Rng {
        fn new(seed: u64) -> Self {
            Self(if seed == 0 { 0x9e37_79b9_7f4a_7c15 } else { seed })
        }

        fn next_u64(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_f491_4f6c_1dd1)
        }

        fn range(&mut self, upper: usize) -> usize {
            if upper <= 1 {
                return 0;
            }
            usize::try_from(self.next_u64() % (upper as u64)).unwrap_or(0)
        }
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum PlantedBug {
        None,
        /// Re-introduces the bug where a still-live owned connection leaves a pure test keepable.
        LiveConnStillKeepable,
        /// Re-introduces the bug where queued or partially-handshaken peers leave a row keepable.
        UnattributedPeerStillKeepable,
        /// Re-introduces the bug where a binary serving error reports the wrong owner.
        WrongErrorOwner,
        /// Re-introduces the bug where an outbox overflow is silently ignored.
        IgnoreOutboxCap,
    }

    struct SimSession {
        served: u64,
    }

    struct SimBackend<'a> {
        bin_count: usize,
        /// Shared per-binary verified `(instance_key, content_key)` pairs persisted via `BZTV`.
        verified: AVec<'a, Set<([u8; 32], [u8; 32]), FixedState, Alloc<'static>>>,
        /// Shared per-binary warm symbol count populated across concurrent peers.
        warm_counts: AVec<'a, usize>,
        /// Fault injection: trigger a typed `ParentError` on the next serve when set.
        inject_parent_err: bool,
        /// Fault injection: emit an oversized `Down::Bytes` frame past `max_outbox`.
        inject_outbox_overflow: bool,
        bug: PlantedBug,
        tables: Alloc<'static>,
        alloc: Alloc<'a>,
    }

    impl<'a> SimBackend<'a> {
        fn new(bin_count: usize, bug: PlantedBug, tables: Alloc<'static>, alloc: Alloc<'a>) -> Self {
            let mut verified = Vec::with_capacity_in(bin_count, alloc);
            let mut warm_counts = Vec::with_capacity_in(bin_count, alloc);
            for _ in 0..bin_count {
                verified.push(Set::new(SetOpts { hasher: FixedState::default(), alloc: tables, capacity: None }));
                warm_counts.push(0);
            }
            Self {
                bin_count,
                verified,
                warm_counts,
                inject_parent_err: false,
                inject_outbox_overflow: false,
                bug,
                tables,
                alloc,
            }
        }
    }

    impl<'a> BinBackend<'a> for SimBackend<'a> {
        type Session = SimSession;

        fn bin_count(&self) -> usize {
            self.bin_count
        }

        fn bin_names(&self, bin_id: usize) -> (&str, &str) {
            match bin_id {
                0 => ("pkg_a", "helper_pure"),
                1 => ("pkg_b", "helper_impure"),
                _ => ("pkg_c", "helper_extra"),
            }
        }

        fn on_handshake(
            &mut self,
            bin_id: usize,
            owner: Option<(u32, u32)>,
            keep: &mut Result<(), NotKept>,
            outbox: &mut WireOut<Alloc<'a>>,
            _out_at: usize,
            _max_outbox: usize,
        ) -> Result<Self::Session, BinServeError> {
            // Bin 1 models an eager-native binary (impure from start); bin 0 starts pure.
            *keep = if bin_id == 1 { Err(NotKept::Foreign) } else { Ok(()) };
            Down::Personality { slot: Slot(0) }.encode(1, outbox).map_err(|why| BinServeError {
                owner,
                package: owned_text(self.bin_names(bin_id).0, self.tables),
                bin: owned_text(self.bin_names(bin_id).1, self.tables),
                why: BinServeWhy::Parent(ParentError::Wire(why)),
            })?;
            Ok(SimSession { served: 0 })
        }

        fn serve_frame(
            &mut self,
            bin_id: usize,
            owner: Option<(u32, u32)>,
            session: &mut Self::Session,
            inbox: &[u8],
            outbox: &mut WireOut<Alloc<'a>>,
            _out_at: usize,
            max_outbox: usize,
            keep: &mut Result<(), NotKept>,
            entered_fns: &mut AVec<'a, EnteredFn<'a>>,
        ) -> Result<(usize, bool), BinServeError> {
            let Some((item, taken)) = split(inbox).map_err(|why| BinServeError {
                owner,
                package: owned_text(self.bin_names(bin_id).0, self.tables),
                bin: owned_text(self.bin_names(bin_id).1, self.tables),
                why: BinServeWhy::Parent(ParentError::Wire(why)),
            })? else {
                return Ok((0, true));
            };
            let (req_id, up) = Up::decode(item).map_err(|why| BinServeError {
                owner,
                package: owned_text(self.bin_names(bin_id).0, self.tables),
                bin: owned_text(self.bin_names(bin_id).1, self.tables),
                why: BinServeWhy::Parent(ParentError::Wire(why)),
            })?;
            if self.inject_parent_err {
                self.inject_parent_err = false;
                let err_owner = match self.bug {
                    PlantedBug::WrongErrorOwner => Some((999, 999)),
                    _ => owner,
                };
                return Err(BinServeError {
                    owner: err_owner,
                    package: owned_text(self.bin_names(bin_id).0, self.tables),
                    bin: owned_text(self.bin_names(bin_id).1, self.tables),
                    why: BinServeWhy::Parent(ParentError::Full),
                });
            }
            let sym_bytes = match up {
                Up::Need { need: Needed::Symbol(Symbol(s)), .. } => s,
                Up::Need { need: Needed::Slot(Slot(n)), .. } => {
                    let mut b = [0u8; 32];
                    b[0..4].copy_from_slice(&n.to_le_bytes());
                    b
                }
                Up::Fault { .. } => [0xFF; 32],
            };
            session.served += 1;
            // Convention in DST: symbol byte 0 >= 0x80 marks an impure foreign call.
            if sym_bytes[0] >= 0x80 {
                if keep.is_ok() {
                    *keep = Err(NotKept::Foreign);
                }
            } else if keep.is_ok() {
                let mut name = Vec::new_in(self.alloc);
                name.extend_from_slice(&sym_bytes[..4]);
                entered_fns.push(EnteredFn {
                    symbol: FnName(sym_bytes),
                    code_key: sym_bytes,
                    tabled: ItemKey(u128::from(sym_bytes[0])),
                    name,
                });
            }
            if let Some(vset) = self.verified.get_mut(bin_id) {
                let mut content = sym_bytes;
                content[31] ^= u8::try_from(bin_id).unwrap_or(0);
                vset.insert((sym_bytes, content));
            }
            if let Some(warm) = self.warm_counts.get_mut(bin_id) {
                *warm += 1;
            }
            if self.inject_outbox_overflow {
                self.inject_outbox_overflow = false;
                // Push more than `max_outbox` bytes into `outbox` to verify `BinProtocol` catches it.
                let over = max_outbox.saturating_add(128);
                let mut big = Vec::with_capacity_in(over, self.alloc);
                big.resize(over, 0xAB);
                if self.bug != PlantedBug::IgnoreOutboxCap {
                    outbox.extend_from_slice(big.as_slice());
                }
            } else {
                let rec = RecordId(sym_bytes);
                Down::Publish { slot: Slot(0), tier: Tier::Zero, record: rec }
                    .encode(req_id, outbox)
                    .map_err(|why| BinServeError {
                        owner,
                        package: owned_text(self.bin_names(bin_id).0, self.tables),
                        bin: owned_text(self.bin_names(bin_id).1, self.tables),
                        why: BinServeWhy::Parent(ParentError::Wire(why)),
                    })?;
            }
            Ok((taken, false))
        }
    }

    struct SimPeer<'a> {
        id: u64,
        bin_id: usize,
        owner: Option<(u32, u32)>,
        /// Bytes queued from client -> server (split into arbitrary tiny chunks).
        to_send: AVec<'a, u8>,
        send_at: usize,
        /// Bytes received from server -> client.
        received: AVec<'a, u8>,
        /// Expected symbols already processed by the server (oracle truth).
        oracle_keep: Result<(), NotKept>,
        oracle_entered: AVec<'a, [u8; 32]>,
        /// Track whether handshake bytes have all been delivered.
        handshake_sent: bool,
        /// Track frame end offsets in `to_send` with each frame's symbol for exact oracle sync.
        frame_ends: AVec<'a, (usize, [u8; 32])>,
        frame_idx: usize,
        closed: bool,
    }

    fn encode_handshake(bin_id: u32, owner: Option<(u32, u32)>, out: &mut AVec<'_, u8>) {
        out.extend_from_slice(&bin_id.to_le_bytes());
        let (s, r) = owner.unwrap_or((u32::MAX, u32::MAX));
        out.extend_from_slice(&s.to_le_bytes());
        out.extend_from_slice(&r.to_le_bytes());
    }

    fn run_dst(seed: u64, bug: PlantedBug) -> Result<(), &'static str> {
        const MAX_SIM_CONNS: usize = 6;
        const MAX_SIM_INBOX: usize = 512;
        const MAX_SIM_OUTBOX: usize = 2048;
        const BIN_COUNT: usize = 2;

        let alloc = blaze_own::testing::heap().alloc();
        let mut rng = Rng::new(seed);
        let mut proto: BinProtocol<'static, SimSession, ()> =
            BinProtocol::new(MAX_SIM_CONNS, MAX_SIM_INBOX, MAX_SIM_OUTBOX, alloc, alloc);
        let mut backend = SimBackend::new(BIN_COUNT, bug, alloc, alloc);
        let mut peers: AVec<'static, SimPeer<'static>> = Vec::new_in(alloc);

        // Oracle persisted BZTV set per binary (checked across simulated restarts).
        let mut oracle_bztv: AVec<'static, Set<([u8; 32], [u8; 32]), FixedState, Alloc<'static>>> =
            Vec::with_capacity_in(BIN_COUNT, alloc);
        for _ in 0..BIN_COUNT {
            oracle_bztv.push(Set::new(SetOpts { hasher: FixedState::default(), alloc, capacity: None }));
        }
        let cert_today = Certificate::for_tests([0x42; 32]);
        let cert_stale = Certificate::for_tests([0x99; 32]);

        for step in 0..400usize {
            // Invariant 1: active connection count never exceeds `max_conns`.
            if proto.conns().len() > MAX_SIM_CONNS {
                return Err("live connection count exceeded max_conns");
            }
            // Invariant 2: every live peer respects inbox and outbox caps.
            for conn in proto.conns() {
                if conn.inbox.len() > MAX_SIM_INBOX {
                    return Err("live peer inbox exceeded max_inbox");
                }
                if conn.outbox.len().saturating_sub(conn.out_at) > MAX_SIM_OUTBOX {
                    return Err("live peer outbox exceeded max_outbox");
                }
            }

            let op = rng.range(100);
            if op < 22 {
                // Accept a new peer (testing wrong UID, cap overflow, valid/unknown bin_id, owned/unowned).
                let same_uid = rng.range(8) != 0;
                let was_full = proto.conns().len() >= MAX_SIM_CONNS;
                let accepted = proto.accept(same_uid, ());
                if !same_uid || was_full {
                    if accepted.is_some() {
                        return Err("accepted peer despite wrong UID or full table");
                    }
                    continue;
                }
                let Some(id) = accepted else {
                    return Err("failed to accept valid peer below cap");
                };
                let bin_id = if rng.range(12) == 0 { 99 } else { rng.range(BIN_COUNT) };
                let owner = if rng.range(6) == 0 {
                    None
                } else {
                    Some((u32::try_from(rng.range(2)).unwrap_or(0), u32::try_from(rng.range(4)).unwrap_or(0)))
                };
                let mut to_send = Vec::new_in(alloc);
                encode_handshake(u32::try_from(bin_id).unwrap_or(0), owner, &mut to_send);
                let mut frame_ends = Vec::new_in(alloc);
                let frames = 1 + rng.range(4);
                for f in 0..frames {
                    let mut sym = [0u8; 32];
                    let impure = rng.range(5) == 0;
                    sym[0] = if impure {
                        0x80 | u8::try_from(rng.range(16)).unwrap_or(0)
                    } else {
                        u8::try_from(1 + rng.range(32)).unwrap_or(1)
                    };
                    sym[1] = u8::try_from(step & 0xff).unwrap_or(0);
                    sym[2] = u8::try_from(f).unwrap_or(0);
                    let mut frame_buf = wire_out(alloc);
                    Up::Need { need: Needed::Symbol(Symbol(sym)), thread: blaze_launch::Thread(0) }
                        .encode(u64::try_from(f + 1).unwrap_or(1), &mut frame_buf)
                        .map_err(|_| "encode Up::Need failed")?;
                    to_send.extend_from_slice(frame_buf.as_slice());
                    frame_ends.push((to_send.len(), sym));
                }
                peers.push(SimPeer {
                    id,
                    bin_id,
                    owner,
                    to_send,
                    send_at: 0,
                    received: Vec::new_in(alloc),
                    oracle_keep: if bin_id == 1 { Err(NotKept::Foreign) } else { Ok(()) },
                    oracle_entered: Vec::new_in(alloc),
                    handshake_sent: false,
                    frame_ends,
                    frame_idx: 0,
                    closed: false,
                });
            } else if op < 58 && !proto.conns().is_empty() {
                // Feed a random tiny chunk (1..=7 bytes) from a live peer into `BinProtocol`.
                let idx = rng.range(proto.conns().len());
                let Some(peer_id) = proto.conn(idx).map(|c| c.id) else { continue };
                let Some(peer) = peers.iter_mut().find(|p| p.id == peer_id) else { continue };
                if peer.send_at >= peer.to_send.len() {
                    proto.on_eof(idx);
                    peer.closed = true;
                    proto.reap_dead();
                    continue;
                }
                let want_parent_err = rng.range(40) == 0 && peer.handshake_sent && peer.bin_id < BIN_COUNT;
                let want_outbox_over = !want_parent_err && rng.range(45) == 0 && peer.handshake_sent && peer.bin_id < BIN_COUNT;
                let want_inbox_over = !want_parent_err && !want_outbox_over && rng.range(50) == 0 && peer.handshake_sent && peer.bin_id < BIN_COUNT;
                if want_inbox_over {
                    let mut junk = Vec::with_capacity_in(MAX_SIM_INBOX + 16, alloc);
                    junk.resize(MAX_SIM_INBOX + 16, 0xFF);
                    match proto.feed_bytes(idx, junk.as_slice(), &mut backend) {
                        Err(err) => {
                            if err.owner != peer.owner {
                                return Err("inbox overflow error attributed to wrong owner");
                            }
                            peer.closed = true;
                            proto.reap_dead();
                        }
                        Ok(_) => return Err("inbox overflow was not rejected"),
                    }
                    continue;
                }
                // Pick chunk size; when injecting a serve fault, deliver up to the next frame boundary
                // so the fault deterministically fires on this step.
                let next_end = peer
                    .frame_ends
                    .get(peer.frame_idx)
                    .map_or(peer.to_send.len(), |&(end, _)| end);
                let chunk_len = if want_parent_err || want_outbox_over {
                    next_end.saturating_sub(peer.send_at).max(1)
                } else {
                    (1 + rng.range(7)).min(peer.to_send.len() - peer.send_at)
                };
                if want_parent_err {
                    backend.inject_parent_err = true;
                } else if want_outbox_over {
                    backend.inject_outbox_overflow = true;
                }
                let start = peer.send_at;
                let end = (start + chunk_len).min(peer.to_send.len());
                let Some(chunk) = peer.to_send.get(start..end) else { continue };
                let res = proto.feed_bytes(idx, chunk, &mut backend);
                peer.send_at = end;
                if !peer.handshake_sent && peer.send_at >= HANDSHAKE_LEN {
                    peer.handshake_sent = true;
                }
                match res {
                    Ok(_) => {
                        if want_parent_err {
                            return Err("injected parent error did not surface");
                        }
                        if want_outbox_over {
                            return Err("outbox overflow was not rejected");
                        }
                        if peer.bin_id >= BIN_COUNT && peer.handshake_sent {
                            peer.closed = true;
                        } else if peer.bin_id < BIN_COUNT {
                            while let Some(&(f_end, sym)) = peer.frame_ends.get(peer.frame_idx) {
                                if peer.send_at < f_end {
                                    break;
                                }
                                peer.frame_idx += 1;
                                if sym[0] >= 0x80 {
                                    if peer.oracle_keep.is_ok() {
                                        peer.oracle_keep = Err(NotKept::Foreign);
                                    }
                                } else if peer.oracle_keep.is_ok() {
                                    peer.oracle_entered.push(sym);
                                }
                                if let Some(oset) = oracle_bztv.get_mut(peer.bin_id) {
                                    let mut content = sym;
                                    content[31] ^= u8::try_from(peer.bin_id).unwrap_or(0);
                                    oset.insert((sym, content));
                                }
                            }
                        }
                    }
                    Err(err) => {
                        if err.owner != peer.owner {
                            return Err("serve error attributed to wrong owner");
                        }
                        if want_outbox_over && peer.bin_id < BIN_COUNT {
                            if let Some(&(f_end, sym)) = peer.frame_ends.get(peer.frame_idx)
                                && peer.send_at >= f_end
                            {
                                peer.frame_idx += 1;
                                if sym[0] >= 0x80 {
                                    if peer.oracle_keep.is_ok() {
                                        peer.oracle_keep = Err(NotKept::Foreign);
                                    }
                                } else if peer.oracle_keep.is_ok() {
                                    peer.oracle_entered.push(sym);
                                }
                                if let Some(oset) = oracle_bztv.get_mut(peer.bin_id) {
                                    let mut content = sym;
                                    content[31] ^= u8::try_from(peer.bin_id).unwrap_or(0);
                                    oset.insert((sym, content));
                                }
                            }
                        }
                        peer.closed = true;
                    }
                }
                proto.reap_dead();
            } else if op < 76 && !proto.conns().is_empty() {
                // Partial outbound drain (1..=32 bytes) or injected socket write error.
                let idx = rng.range(proto.conns().len());
                let Some(peer_id) = proto.conn(idx).map(|c| c.id) else { continue };
                let Some(peer) = peers.iter_mut().find(|p| p.id == peer_id) else { continue };
                if rng.range(16) == 0 {
                    proto.on_io_error(idx);
                    peer.closed = true;
                    proto.reap_dead();
                } else {
                    let pending = proto.pending_out(idx);
                    if !pending.is_empty() {
                        let take = (1 + rng.range(32)).min(pending.len());
                        if let Some(slice) = pending.get(..take) {
                            peer.received.extend_from_slice(slice);
                        }
                        proto.ack_out(idx, take);
                        if proto.conn(idx).is_some_and(|c| c.dead) {
                            peer.closed = true;
                        }
                        proto.reap_dead();
                    }
                }
            } else if op < 88 && !proto.conns().is_empty() {
                // Disconnect (EOF) a peer mid-handshake, mid-frame, or after completion.
                let idx = rng.range(proto.conns().len());
                let Some(peer_id) = proto.conn(idx).map(|c| c.id) else { continue };
                let Some(peer) = peers.iter_mut().find(|p| p.id == peer_id) else { continue };
                proto.on_eof(idx);
                if proto.conn(idx).is_some_and(|c| c.dead) {
                    peer.closed = true;
                }
                proto.reap_dead();
            } else if op < 95 {
                // Finish an owner `(suite, row)` and check the evidence + live-conn oracle!
                let suite = u32::try_from(rng.range(2)).unwrap_or(0);
                let row = u32::try_from(rng.range(4)).unwrap_or(0);
                let has_queued_accept = rng.range(8) == 0;
                let mut drained_entered = Vec::new_in(alloc);
                let mut drained_keep: Result<(), NotKept> = Ok(());

                if bug == PlantedBug::LiveConnStillKeepable {
                    // Simulate the pre-fix bug: copy live evidence without marking live conn unkeepable.
                    for conn in proto.conns.iter_mut() {
                        if conn.owner == Some((suite, row)) {
                            if let Err(why) = conn.keep
                                && drained_keep.is_ok()
                            {
                                drained_keep = Err(why);
                            }
                            while let Some(e) = conn.entered_fns.pop() {
                                drained_entered.push(e);
                            }
                        }
                    }
                    let mut i = 0;
                    while i < proto.evidence.len() {
                        if proto.evidence.get(i).is_some_and(|ev| ev.suite == suite && ev.row == row) {
                            let mut ev = proto.evidence.remove_swap(i);
                            if let Err(why) = ev.keep
                                && drained_keep.is_ok()
                            {
                                drained_keep = Err(why);
                            }
                            while let Some(e) = ev.entered_fns.pop() {
                                drained_entered.push(e);
                            }
                        } else {
                            i += 1;
                        }
                    }
                } else if bug == PlantedBug::UnattributedPeerStillKeepable {
                    // Simulate the pre-fix bug: mark live owned connections unkeepable, but ignore
                    // queued accepts and partially-handshaken (`bin_id.is_none()`) peers.
                    for conn in proto.conns.iter_mut() {
                        if conn.owner == Some((suite, row)) {
                            if !conn.dead {
                                if drained_keep.is_ok() {
                                    drained_keep = Err(conn.keep.err().unwrap_or(NotKept::Untraced));
                                }
                            } else if let Err(why) = conn.keep
                                && drained_keep.is_ok()
                            {
                                drained_keep = Err(why);
                            }
                            while let Some(e) = conn.entered_fns.pop() {
                                drained_entered.push(e);
                            }
                        }
                    }
                    let mut i = 0;
                    while i < proto.evidence.len() {
                        if proto.evidence.get(i).is_some_and(|ev| ev.suite == suite && ev.row == row) {
                            let mut ev = proto.evidence.remove_swap(i);
                            if let Err(why) = ev.keep
                                && drained_keep.is_ok()
                            {
                                drained_keep = Err(why);
                            }
                            while let Some(e) = ev.entered_fns.pop() {
                                drained_entered.push(e);
                            }
                        } else {
                            i += 1;
                        }
                    }
                } else {
                    proto.drain_owner_evidence(suite, row, has_queued_accept, &mut drained_entered, &mut drained_keep);
                }

                // Oracle: check whether ANY live peer currently has `owner == Some((suite, row))`,
                // OR any live peer is still pre-handshake (`bin_id.is_none()`), OR a peer is queued.
                let has_live_owned = proto
                    .conns()
                    .iter()
                    .any(|c| !c.dead && c.owner == Some((suite, row)));
                let has_unattributed = has_queued_accept
                    || proto.conns().iter().any(|c| !c.dead && c.bin_id.is_none());
                let mut expected_keep: Result<(), NotKept> = Ok(());
                let mut expected_entered: AVec<'static, [u8; 32]> = Vec::new_in(alloc);
                let mut idx = 0;
                while idx < peers.len() {
                    let matches = peers
                        .get(idx)
                        .is_some_and(|p| p.handshake_sent && p.bin_id < BIN_COUNT && p.owner == Some((suite, row)));
                    if matches {
                        let is_closed = peers.get(idx).is_some_and(|p| p.closed);
                        if is_closed {
                            let p = peers.remove_swap(idx);
                            if let Err(why) = p.oracle_keep
                                && expected_keep.is_ok()
                            {
                                expected_keep = Err(why);
                            }
                            for sym in p.oracle_entered.iter() {
                                expected_entered.push(*sym);
                            }
                            continue;
                        } else if let Some(p) = peers.get_mut(idx) {
                            // Still live -> row must be unkeepable, and live entered_fns were drained.
                            expected_keep = Err(NotKept::Untraced);
                            p.oracle_entered.clear();
                        }
                    }
                    idx += 1;
                }
                if has_live_owned && drained_keep.is_ok() {
                    return Err("row with live helper connection was marked keepable");
                }
                if has_unattributed && drained_keep.is_ok() {
                    return Err("row with queued or pre-handshake helper peer was marked keepable");
                }
                if !has_live_owned && !has_unattributed {
                    if drained_keep.is_ok() != expected_keep.is_ok() {
                        return Err("purity verdict diverged from oracle");
                    }
                    if drained_keep.is_ok() {
                        let mut got: AVec<'static, [u8; 32]> = Vec::new_in(alloc);
                        for e in drained_entered.iter() {
                            got.push(e.symbol.0);
                        }
                        got.sort_unstable();
                        expected_entered.sort_unstable();
                        if got.as_slice() != expected_entered.as_slice() {
                            return Err("entered_fns diverged from oracle");
                        }
                    }
                }
            } else {
                // Durability checkpoint: serialize `BZTV` verified pairs, verify exact round-trip
                // under `cert_today`, stale rejection under `cert_stale`, and corruption detection.
                for bin_id in 0..BIN_COUNT {
                    let Some(vset) = backend.verified.get(bin_id) else { continue };
                    let Some(oset) = oracle_bztv.get(bin_id) else { continue };
                    let mut pairs: AVec<'static, ([u8; 32], [u8; 32])> = Vec::new_in(alloc);
                    for &pair in vset.iter() {
                        pairs.push(pair);
                    }
                    pairs.sort_unstable();
                    pairs.dedup();
                    let mut bytes: AVec<'static, u8> = Vec::new_in(alloc);
                    encode_verified(&cert_today, pairs.as_slice(), &mut bytes);
                    let mut reloaded: Set<([u8; 32], [u8; 32]), FixedState, Alloc<'static>> =
                        Set::new(SetOpts { hasher: FixedState::default(), alloc, capacity: None });
                    match decode_verified_status(bytes.as_slice(), &cert_today, &mut reloaded) {
                        VerifiedDecode::Loaded(n) if n == oset.len() => {
                            for pair in oset.iter() {
                                if !reloaded.contains(pair) {
                                    return Err("reloaded BZTV missed an oracle verified pair");
                                }
                            }
                        }
                        _ => return Err("BZTV round-trip failed under matching certificate"),
                    }
                    let mut stale_out: Set<([u8; 32], [u8; 32]), FixedState, Alloc<'static>> =
                        Set::new(SetOpts { hasher: FixedState::default(), alloc, capacity: None });
                    if decode_verified_status(bytes.as_slice(), &cert_stale, &mut stale_out)
                        != VerifiedDecode::StaleCertificate
                    {
                        return Err("BZTV did not reject stale certificate");
                    }
                    if let Some(last) = bytes.last_mut() {
                        *last ^= 0xFF;
                    }
                    bytes.truncate(bytes.len().saturating_sub(1));
                    let mut corrupt_out: Set<([u8; 32], [u8; 32]), FixedState, Alloc<'static>> =
                        Set::new(SetOpts { hasher: FixedState::default(), alloc, capacity: None });
                    if decode_verified_status(bytes.as_slice(), &cert_today, &mut corrupt_out)
                        != VerifiedDecode::Corrupt
                    {
                        return Err("truncated BZTV was not flagged corrupt");
                    }
                }
            }
        }
        Ok(())
    }

    #[test]
    fn dst_concurrent_bin_protocol_matches_oracle_across_32_seeds() {
        for seed in 1u64..=32 {
            if let Err(why) = run_dst(seed, PlantedBug::None) {
                panic!("DST failed on seed {seed}: {why}");
            }
        }
    }

    #[test]
    fn dst_planted_bug_live_conn_keep_is_caught() {
        let mut caught = false;
        for seed in 1u64..=32 {
            if run_dst(seed, PlantedBug::LiveConnStillKeepable)
                == Err("row with live helper connection was marked keepable")
            {
                caught = true;
                break;
            }
        }
        assert!(caught, "planted bug LiveConnStillKeepable must be caught by the DST oracle");
    }

    #[test]
    fn dst_planted_bug_wrong_owner_error_is_caught() {
        let mut caught = false;
        for seed in 1u64..=32 {
            if run_dst(seed, PlantedBug::WrongErrorOwner) == Err("serve error attributed to wrong owner") {
                caught = true;
                break;
            }
        }
        assert!(caught, "planted bug WrongErrorOwner must be caught by the DST oracle");
    }

    #[test]
    fn dst_planted_bug_outbox_cap_bypass_is_caught() {
        let mut caught = false;
        for seed in 1u64..=32 {
            if run_dst(seed, PlantedBug::IgnoreOutboxCap) == Err("outbox overflow was not rejected") {
                caught = true;
                break;
            }
        }
        assert!(caught, "planted bug IgnoreOutboxCap must be caught by the DST oracle");
    }

    #[test]
    fn dst_planted_bug_unattributed_peer_keep_is_caught() {
        let mut caught = false;
        for seed in 1u64..=32 {
            if run_dst(seed, PlantedBug::UnattributedPeerStillKeepable)
                == Err("row with queued or pre-handshake helper peer was marked keepable")
            {
                caught = true;
                break;
            }
        }
        assert!(
            caught,
            "planted bug UnattributedPeerStillKeepable must be caught by the DST oracle"
        );
    }
}
