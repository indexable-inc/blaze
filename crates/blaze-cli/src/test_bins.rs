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
//! 4. Multiplexes all concurrent child binary connections (capped at [`MAX_CONNS`] peers and
//!    [`MAX_INBOX`]/[`MAX_OUTBOX`] wire buffers) on the command thread via non-blocking `poll`
//!    inside [`BinServer::pump`], propagating host-side serving failures as typed errors and
//!    joining each binary's entered functions and purity back to its spawning test row.

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

struct BinConn<'a> {
    fd: blaze_sys::posix::io::OwnedFd,
    bin_id: Option<usize>,
    owner: Option<(u32, u32)>,
    handshake: [u8; HANDSHAKE_LEN],
    handshake_len: usize,
    needs: Needs<'a>,
    child_sent: ChildSent<'a>,
    inbox: WireOut<Alloc<'a>>,
    outbox: WireOut<Alloc<'a>>,
    out_at: usize,
    tally: Tally,
    keep: Result<(), NotKept>,
    entered_fns: AVec<'a, EnteredFn<'a>>,
    dead: bool,
}

struct OwnerEvidence<'a> {
    suite: u32,
    row: u32,
    keep: Result<(), NotKept>,
    entered_fns: AVec<'a, EnteredFn<'a>>,
}

/// Active binary stub server owned by `run_dirty` for the duration of one `blaze test` command.
pub(crate) struct BinServer<'a, 'h> {
    bins_dir: AString<'a>,
    sock_dir_guard: PrivateSocketDir,
    listener: blaze_sys::posix::io::OwnedFd,
    bins: AVec<'a, BinRuntime<'a, 'h>>,
    #[cfg(target_os = "macos")]
    natives: AVec<'a, crate::engine_native::NativeSend<'a>>,
    conns: AVec<'a, BinConn<'a>>,
    pollfds: AVec<'a, blaze_sys::posix::io::PollFd>,
    evidence: AVec<'a, OwnerEvidence<'a>>,
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
            conns: Vec::new_in(alloc),
            pollfds: Vec::new_in(alloc),
            evidence: Vec::new_in(alloc),
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
        // Also flush any still-live connections owned by this finishing test row.
        for conn in self.conns.iter_mut() {
            if conn.owner == Some((suite, row)) {
                if let Err(why) = conn.keep
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

        let mut any_progress = false;
        let zero = Some(Timespec { sec: 0, nsec: 0 });
        let mut timeout = zero;
        for _ in 0..PUMP_BURSTS {
            self.pollfds.clear();
            self.pollfds.push(PollFd::new(self.listener.fd(), PollEvents::IN));
            for conn in self.conns.iter() {
                let mut ev = PollEvents::IN;
                if conn.out_at < conn.outbox.len() {
                    ev = ev.with(PollEvents::OUT);
                }
                self.pollfds.push(PollFd::new(conn.fd.fd(), ev));
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
                            // Enforce peer-UID authentication and bounded connection count (rule 18).
                            let same_uid = blaze_sys::posix::net::peer_is_self_uid(fd.fd()).unwrap_or(false);
                            if !same_uid || self.conns.len() >= MAX_CONNS {
                                drop(fd);
                                continue;
                            }
                            self.conns.push(BinConn {
                                fd,
                                bin_id: None,
                                owner: None,
                                handshake: [0u8; HANDSHAKE_LEN],
                                handshake_len: 0,
                                needs: Needs::new(self.alloc),
                                child_sent: ChildSent::new(self.alloc),
                                inbox: wire_out(self.alloc),
                                outbox: wire_out(self.alloc),
                                out_at: 0,
                                tally: Tally::default(),
                                keep: Ok(()),
                                entered_fns: Vec::new_in(self.alloc),
                                dead: false,
                            });
                            burst_progress = true;
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
                let Some(conn) = self.conns.get_mut(idx) else { continue };
                if found.contains(PollEvents::OUT) && conn.out_at < conn.outbox.len() {
                    if flush_conn(conn) {
                        burst_progress = true;
                    }
                }
                if !conn.dead && (found.contains(PollEvents::IN) || found.contains(PollEvents::HUP)) {
                    match service_conn_read(
                        conn,
                        self.bins.as_slice(),
                        cx,
                        ids,
                        lent,
                        watch,
                        self.tables,
                        self.alloc,
                    ) {
                        Ok(prog) => {
                            if prog {
                                burst_progress = true;
                            }
                        }
                        Err(err) => {
                            conn.dead = true;
                            if serve_err.is_none() {
                                serve_err = Some(err);
                            }
                        }
                    }
                    if !conn.dead && conn.out_at < conn.outbox.len() && flush_conn(conn) {
                        burst_progress = true;
                    }
                }
                if found.contains(PollEvents::ERR) || found.contains(PollEvents::NVAL) {
                    conn.dead = true;
                }
            }

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

fn flush_conn(conn: &mut BinConn<'_>) -> bool {
    let mut wrote_any = false;
    while conn.out_at < conn.outbox.len() {
        let slice = conn.outbox.get(conn.out_at..).unwrap_or(&[]);
        match blaze_sys::posix::io::write(conn.fd.fd(), slice) {
            Ok(0) => {
                conn.dead = true;
                break;
            }
            Ok(n) => {
                conn.out_at += n;
                wrote_any = true;
            }
            Err(Errno::Again | Errno::Intr) => break,
            Err(_) => {
                conn.dead = true;
                break;
            }
        }
    }
    if conn.out_at >= conn.outbox.len() {
        conn.outbox.clear();
        conn.out_at = 0;
    } else if conn.out_at > 64 * 1024 {
        let left = conn.outbox.len() - conn.out_at;
        conn.outbox.copy_within(conn.out_at.., 0);
        conn.outbox.truncate(left);
        conn.out_at = 0;
    }
    wrote_any
}

#[expect(clippy::too_many_arguments, reason = "serves one binary connection using the command's runtime handles")]
fn service_conn_read<'a, 'h>(
    conn: &mut BinConn<'a>,
    bins: &[BinRuntime<'a, 'h>],
    cx: &Cx,
    ids: &RunIds<'h>,
    lent: &CommandLent,
    watch: &dyn Watch,
    tables: Alloc<'static>,
    alloc: Alloc<'a>,
) -> Result<bool, BinServeError> {
    let mut progress = false;
    // 1. Complete 12-byte LE `[bin_id, owner_suite, owner_row]` handshake.
    if conn.bin_id.is_none() {
        while conn.handshake_len < HANDSHAKE_LEN {
            let Some(slot) = conn.handshake.get_mut(conn.handshake_len..HANDSHAKE_LEN) else { break };
            match blaze_sys::posix::io::read(conn.fd.fd(), slot) {
                Ok(0) => {
                    conn.dead = true;
                    return Ok(progress);
                }
                Ok(n) => {
                    conn.handshake_len += n;
                    progress = true;
                }
                Err(Errno::Again | Errno::Intr) => return Ok(progress),
                Err(_) => {
                    conn.dead = true;
                    return Ok(progress);
                }
            }
        }
        let [b0, b1, b2, b3, s0, s1, s2, s3, r0, r1, r2, r3] = conn.handshake;
        let id = usize::try_from(u32::from_le_bytes([b0, b1, b2, b3])).unwrap_or(usize::MAX);
        let owner_suite = u32::from_le_bytes([s0, s1, s2, s3]);
        let owner_row = u32::from_le_bytes([r0, r1, r2, r3]);
        let Some(bin) = bins.get(id) else {
            conn.dead = true;
            return Ok(progress);
        };
        conn.bin_id = Some(id);
        if owner_suite != u32::MAX && owner_row != u32::MAX {
            conn.owner = Some((owner_suite, owner_row));
        }
        let eager_native = bin
            .native
            .is_some_and(|send| send.load_mode == crate::engine_native::NativeLoadMode::Eager);
        conn.keep = if eager_native { Err(NotKept::Foreign) } else { Ok(()) };
        let head = bin.roots.head();
        let make_err = |why| BinServeError {
            package: owned_text(bin.package, tables),
            bin: owned_text(bin.bin_name, tables),
            why: BinServeWhy::Parent(why),
        };
        if let Some(send) = bin.native {
            if eager_native {
                conn.needs.tell_native(send.library, send.words, &mut conn.outbox).map_err(make_err)?;
            } else {
                conn.needs.arm_lazy_native(send.library, send.words);
            }
        }
        conn.needs.name_personality(head.personality, &mut conn.outbox).map_err(make_err)?;
        // Pre-publish warm functions already compiled by earlier spawns ONLY when this binary is
        // already impure (native image linked); bounded strictly by `MAX_OUTBOX` (rule 18).
        if conn.keep.is_err() {
            let cap = conn.out_at.saturating_add(MAX_OUTBOX);
            bin.suite_cache
                .borrow()
                .prepublish_warm_bounded(
                    bin.entry_sym,
                    head,
                    &mut conn.child_sent,
                    &mut conn.needs,
                    &mut conn.outbox,
                    &mut conn.tally,
                    cap,
                    alloc,
                )
                .map_err(make_err)?;
        }
    }

    // 2. Read incoming `Up` frames from the binary launcher (bounded by `max_inbox()`).
    let limit = max_inbox();
    let mut buf = [0u8; READ_CHUNK];
    let mut eof = false;
    for _ in 0..4 {
        if conn.inbox.len() >= limit {
            let (pkg, bin_name) = conn
                .bin_id
                .and_then(|id| bins.get(id))
                .map_or(("<unknown>", "<unknown>"), |b| (b.package, b.bin_name));
            return Err(BinServeError {
                package: owned_text(pkg, tables),
                bin: owned_text(bin_name, tables),
                why: BinServeWhy::Protocol("peer inbox exceeded MAX_INBOX"),
            });
        }
        let room = (limit - conn.inbox.len()).min(buf.len());
        let Some(slice) = buf.get_mut(..room) else { break };
        match blaze_sys::posix::io::read(conn.fd.fd(), slice) {
            Ok(0) => {
                eof = true;
                break;
            }
            Ok(n) => {
                if let Some(chunk) = buf.get(..n) {
                    conn.inbox.extend_from_slice(chunk);
                    progress = true;
                }
                if n < room {
                    break;
                }
            }
            Err(Errno::Again | Errno::Intr) => break,
            Err(_) => {
                conn.dead = true;
                return Ok(progress);
            }
        }
    }

    // 3. Serve complete frames through `SuiteCache` + `ChildResolver` (`Codes::Serial`).
    if let Some(bin_id) = conn.bin_id
        && let Some(bin) = bins.get(bin_id)
        && !conn.inbox.is_empty()
    {
        let mut codes = HomeCodeStore::new(lent);
        let make_err = |why| BinServeError {
            package: owned_text(bin.package, tables),
            bin: owned_text(bin.bin_name, tables),
            why: BinServeWhy::Parent(why),
        };
        loop {
            let was_keep_ok = conn.keep.is_ok();
            let mut resolver = ChildResolver::new(
                &bin.suite_cache,
                &mut conn.child_sent,
                &bin.roots,
                bin.entry_sym,
                Codes::Serial,
                ids,
                &mut codes,
                cx,
                watch,
                &mut conn.tally,
                &mut conn.keep,
                &mut conn.entered_fns,
                alloc,
                alloc,
            );
            let served = conn.needs.serve(&mut resolver, conn.inbox.as_slice(), &mut conn.outbox);
            let answer = resolver.requested.take();
            drop(resolver);
            match served {
                Ok(0) => break,
                Ok(taken) => {
                    let left = conn.inbox.len().saturating_sub(taken);
                    conn.inbox.copy_within(taken.., 0);
                    conn.inbox.truncate(left);
                    progress = true;
                    if matches!(answer, Some(ChildAnswer::Unresolved(_))) {
                        break;
                    }
                    let warm_len = bin.suite_cache.borrow().warm_len();
                    if conn.keep.is_err()
                        && conn.child_sent.warm_cursor < warm_len
                        && (was_keep_ok || warm_len.saturating_sub(conn.child_sent.warm_cursor) >= 256)
                    {
                        conn.needs.flush_pending_native(&mut conn.outbox).map_err(make_err)?;
                        let cap = conn.out_at.saturating_add(MAX_OUTBOX);
                        bin.suite_cache
                            .borrow()
                            .prepublish_warm_bounded(
                                bin.entry_sym,
                                bin.roots.head(),
                                &mut conn.child_sent,
                                &mut conn.needs,
                                &mut conn.outbox,
                                &mut conn.tally,
                                cap,
                                alloc,
                            )
                            .map_err(make_err)?;
                    }
                    if conn.outbox.len().saturating_sub(conn.out_at) > MAX_OUTBOX {
                        return Err(BinServeError {
                            package: owned_text(bin.package, tables),
                            bin: owned_text(bin.bin_name, tables),
                            why: BinServeWhy::Protocol("peer outbox exceeded MAX_OUTBOX"),
                        });
                    }
                }
                Err(why) => return Err(make_err(why)),
            }
            if conn.inbox.is_empty() {
                break;
            }
        }
    }
    if eof && conn.out_at >= conn.outbox.len() {
        conn.dead = true;
    }
    Ok(progress)
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
