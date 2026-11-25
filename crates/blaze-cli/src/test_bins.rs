//! Workspace binary launcher stubs (`CARGO_BIN_EXE_<name>`) for `blaze test` (`test.md` piece 17).
//!
//! No executable binary is compiled or written to disk (bottom.md section 11). For each workspace
//! `[[bin]]` exposed via `CARGO_BIN_EXE_<name>` to any selected test suite, `blaze test`:
//! 1. Opens a single non-blocking `AF_UNIX` listener socket `<mine>/b/s` (kept short to stay well
//!    within `sockaddr_un::sun_path`'s 104/108-byte limit).
//! 2. Places a launcher stub `<mine>/b/<package>/<bin_name>` (hardlink to `blaze-launcher`, with a
//!    copy+`chmod 0o755` fallback across mount points) beside a `mode=run` sidecar
//!    `<stub>.blaze-program` naming `socket`, `bin=<id>`, `store`, `entry`, and optional `native`.
//! 3. Sets runtime `CARGO_BIN_EXE_<bin_name>=<host_stub>` and `BLAZE_LAUNCH_BIN_MAP` (mapping
//!    compile-time `/target/debug/bin/<package>/<bin_name>` literals to `<host_stub>` inside
//!    `LauncherOwn`), plus `BLAZE_LAUNCH_BINS_DIR` (Linux `keep_ro`) / Seatbelt `control` + native
//!    dylib grants (macOS).
//! 4. Multiplexes all concurrent child binary connections on the command thread via non-blocking
//!    `poll(0)` inside [`BinServer::pump`], reusing [`SuiteCache`] and [`ChildResolver`] so repeat
//!    spawns pre-publish warm functions in one burst with zero recompilation.

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
    Path(PathError),
    Fs(FsError),
    Socket { path: AString<'static>, errno: Errno },
    Stub { path: AString<'static>, errno: Errno },
    PathInvalid { path: AString<'static> },
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
    handshake: [u8; 4],
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

/// Active binary stub server owned by `run_dirty` for the duration of one `blaze test` command.
pub(crate) struct BinServer<'a, 'h> {
    bins_dir: AString<'a>,
    #[cfg(target_os = "macos")]
    socket_path: AString<'a>,
    listener: blaze_sys::posix::io::OwnedFd,
    bins: AVec<'a, BinRuntime<'a, 'h>>,
    #[cfg(target_os = "macos")]
    natives: AVec<'a, crate::engine_native::NativeSend<'a>>,
    conns: AVec<'a, BinConn<'a>>,
    pollfds: AVec<'a, blaze_sys::posix::io::PollFd>,
    alloc: Alloc<'a>,
}

impl<'a, 'h> BinServer<'a, 'h> {
    /// Creates `<mine>/b/`, binds `<mine>/b/s`, writes launcher stubs + `mode=run` sidecars for
    /// each `BinSuite`, and initializes per-binary `SuiteCache`s.
    #[expect(clippy::too_many_arguments, reason = "prepares the command's binary stub server from run_dirty's capabilities")]
    pub(crate) fn start(
        cx: &Cx,
        mine: &Dir<ReadWrite>,
        mine_abs: &str,
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
        let mut socket_path = String::new_in(alloc);
        let _ = write!(socket_path, "{}/s", bins_dir.as_str());
        let listener = blaze_sys::posix::net::listen_unix(socket_path.as_bytes(), 128).map_err(|errno| {
            BinStubError::Socket { path: owned_text(socket_path.as_str(), tables), errno }
        })?;

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
            let _ = crate::test_cache::load_verified_by_id(lent, cache_id, certificate, sc.verified_mut());
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
            let _ = writeln!(sidecar, "socket={}", socket_path.as_str());
            let _ = writeln!(sidecar, "bin={id}");
            let _ = writeln!(sidecar, "store={store}");
            let _ = writeln!(sidecar, "entry={}", head.line);
            if let Some(send) = bin.native {
                let _ = writeln!(sidecar, "native={}", send.hex);
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
            #[cfg(target_os = "macos")]
            socket_path,
            listener,
            bins: runtimes,
            #[cfg(target_os = "macos")]
            natives,
            conns: Vec::new_in(alloc),
            pollfds: Vec::new_in(alloc),
            alloc,
        })
    }

    pub(crate) fn bins_dir(&self) -> &str {
        self.bins_dir.as_str()
    }

    /// Persists any newly verified `(instance_key, content_key)` pairs (`BZTV`) for each binary
    /// served during this command under today's `certificate`.
    pub(crate) fn save_verified(
        &self,
        lent: &CommandLent,
        certificate: &crate::start_list::Certificate,
    ) -> Result<(), crate::test_drive::DriveError> {
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
        self.socket_path.as_str()
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn bin_natives(&self) -> &[crate::engine_native::NativeSend<'a>] {
        self.natives.as_slice()
    }

    /// Appends runtime `CARGO_BIN_EXE_<bin_name>=<host_stub>` pairs for `package` and returns the
    /// `BLAZE_LAUNCH_BIN_MAP` text (`<guest>\t<host>\n` lines) when `package` has any binaries.
    pub(crate) fn fill_suite_env<'s, 'f>(
        &'s self,
        package: &str,
        vars: &mut Vec<(&'s str, &'s str), &'f Frame>,
        frame: &'f Frame,
    ) -> Option<String<&'f Frame>> {
        let mut map = String::new_in(frame);
        let mut any = false;
        for bin in self.bins.iter() {
            if bin.package != package {
                continue;
            }
            any = true;
            vars.push((bin.env_name.as_str(), bin.host_path.as_str()));
            let _ = writeln!(map, "{}\t{}", bin.guest_path.as_str(), bin.host_path.as_str());
        }
        any.then_some(map)
    }

    /// Non-blocking poll/accept/read/serve/write pump across the listener and every connected
    /// workspace binary process. Returns `true` if any connection was accepted or made progress.
    pub(crate) fn pump(
        &mut self,
        cx: &Cx,
        ids: &RunIds<'h>,
        lent: &CommandLent,
        watch: &dyn Watch,
        err: &bstd::out::Out,
    ) -> bool {
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
                            self.conns.push(BinConn {
                                fd,
                                bin_id: None,
                                handshake: [0u8; 4],
                                handshake_len: 0,
                                needs: Needs::new(self.alloc),
                                child_sent: ChildSent::new(self.alloc),
                                inbox: wire_out(self.alloc),
                                outbox: wire_out(self.alloc),
                                out_at: 0,
                                tally: Tally::default(),
                                keep: Err(NotKept::Foreign),
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
                    if service_conn_read(conn, self.bins.as_slice(), cx, ids, lent, watch, err, self.alloc) {
                        burst_progress = true;
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
                if self.conns[i].dead {
                    self.conns.remove_swap(i);
                } else {
                    i += 1;
                }
            }
            if burst_progress {
                any_progress = true;
                timeout = Some(FOLLOW_UP_POLL);
            } else {
                break;
            }
        }
        any_progress
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
    err: &bstd::out::Out,
    alloc: Alloc<'a>,
) -> bool {
    let mut progress = false;
    // 1. Complete 4-byte LE `bin_id` handshake if not yet identified.
    if conn.bin_id.is_none() {
        while conn.handshake_len < 4 {
            let Some(slot) = conn.handshake.get_mut(conn.handshake_len..4) else { break };
            match blaze_sys::posix::io::read(conn.fd.fd(), slot) {
                Ok(0) => {
                    conn.dead = true;
                    return progress;
                }
                Ok(n) => {
                    conn.handshake_len += n;
                    progress = true;
                }
                Err(Errno::Again | Errno::Intr) => return progress,
                Err(_) => {
                    conn.dead = true;
                    return progress;
                }
            }
        }
        let id = usize::try_from(u32::from_le_bytes(conn.handshake)).unwrap_or(usize::MAX);
        let Some(bin) = bins.get(id) else {
            conn.dead = true;
            return progress;
        };
        conn.bin_id = Some(id);
        let head = bin.roots.head();
        if let Some(send) = bin.native
            && let Err(why) = conn.needs.tell_native(send.library, send.words, &mut conn.outbox)
        {
            let _ = err.line(cx, format_args!("blaze test: binary `{}/{}` stopped: {why}", bin.package, bin.bin_name));
            conn.dead = true;
            return progress;
        }
        if let Err(why) = conn.needs.name_personality(head.personality, &mut conn.outbox) {
            let _ = err.line(cx, format_args!("blaze test: binary `{}/{}` stopped: {why}", bin.package, bin.bin_name));
            conn.dead = true;
            return progress;
        }
        // Pre-publish warm functions already compiled by earlier spawns of this binary.
        if let Err(why) = bin.suite_cache.borrow().prepublish_warm(
            bin.entry_sym,
            head,
            &mut conn.child_sent,
            &mut conn.needs,
            &mut conn.outbox,
            &mut conn.tally,
            alloc,
        ) {
            let _ = err.line(cx, format_args!("blaze test: binary `{}/{}` stopped: {why}", bin.package, bin.bin_name));
            conn.dead = true;
            return progress;
        }
    }

    // 2. Read incoming `Up` frames from the binary launcher.
    let mut buf = [0u8; READ_CHUNK];
    let mut eof = false;
    for _ in 0..4 {
        match blaze_sys::posix::io::read(conn.fd.fd(), &mut buf) {
            Ok(0) => {
                eof = true;
                break;
            }
            Ok(n) => {
                if let Some(chunk) = buf.get(..n) {
                    conn.inbox.extend_from_slice(chunk);
                    progress = true;
                }
                if n < buf.len() {
                    break;
                }
            }
            Err(Errno::Again | Errno::Intr) => break,
            Err(_) => {
                conn.dead = true;
                return progress;
            }
        }
    }

    // 3. Serve complete frames through `SuiteCache` + `ChildResolver` (`Codes::Serial`).
    if let Some(bin_id) = conn.bin_id
        && let Some(bin) = bins.get(bin_id)
        && !conn.inbox.is_empty()
    {
        let mut codes = HomeCodeStore::new(lent);
        loop {
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
                        // `Needs::serve` already queued `Down::Failed`; flush will deliver it.
                        break;
                    }
                    let warm_len = bin.suite_cache.borrow().warm_len();
                    if warm_len.saturating_sub(conn.child_sent.warm_cursor) >= 256
                        && let Err(why) = bin.suite_cache.borrow().prepublish_warm(
                            bin.entry_sym,
                            bin.roots.head(),
                            &mut conn.child_sent,
                            &mut conn.needs,
                            &mut conn.outbox,
                            &mut conn.tally,
                            alloc,
                        )
                    {
                        let _ = err.line(cx, format_args!("blaze test: binary `{}/{}` stopped: {why}", bin.package, bin.bin_name));
                        conn.dead = true;
                        return progress;
                    }
                }
                Err(why) => {
                    let _ = err.line(cx, format_args!("blaze test: binary `{}/{}` stopped: {why}", bin.package, bin.bin_name));
                    conn.dead = true;
                    return progress;
                }
            }
            if conn.inbox.is_empty() {
                break;
            }
        }
    }
    if eof && conn.out_at >= conn.outbox.len() {
        conn.dead = true;
    }
    progress
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
    chmod_executable(stub_abs, tables)?;
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

fn chmod_executable(path: &str, tables: Alloc<'static>) -> Result<(), BinStubError> {
    use blaze_sys::posix::io::{CWD, OpenFlags, openat};
    use blaze_sys::posix::proc::change_mode;

    with_cstr(path, tables, |cstr| {
        let fd = openat(CWD, blaze_sys::Path::new(cstr), OpenFlags::READ.with(OpenFlags::CLOSE_ON_EXEC), 0)
            .map_err(|errno| BinStubError::Stub { path: owned_text(path, tables), errno })?;
        change_mode(fd.fd(), 0o755)
            .map_err(|errno| BinStubError::Stub { path: owned_text(path, tables), errno })?;
        Ok(())
    })
}
