//! Resident per-checkout `blaze` daemon (`blaze daemon [folder]`, Wave 7, rule 70).
//!
//! Keeps one checkout's CAS store mappings (`Opened`), `HotGraph` crate list, `HotAnswer`
//! metadata, synchronous `KqueueVnode` / passive `HeldChanges` watcher, and `bstd::rt` worker
//! pool warm across `blaze check` invocations while holding zero CAS leases while idle.
//!
//! **Soundness & lifecycle (rule 70).**
//! - Governed by `blaze_watch::daemon::DaemonCore`: exact binary `Version`, exact `CheckoutKey`,
//!   30-minute idle timeout, 12-hour max wall lifetime, `MAX_PHASE_REBUILDS` (32), 1-second tick
//!   checking `OwnExe` replacement and checkout existence, and clean `ClientFrame::Shutdown`.
//! - Before EVERY request, verifies `opened.lent.head_generation_on_disk()? == state.store_gen`
//!   (~2 µs); any external commit, `blaze gc`, or `blaze clean --store` immediately invalidates
//!   cached state and reopens the store fresh.
//! - Never installs per-request `bstd::procs::Asked::hold()` signal handlers (which would clobber
//!   daemon signal handling).
//! - Applies each client request's exact `env`, `argv`, `stdout_tty`, `stderr_tty`, and `cols`
//!   without mutating process globals or leaking state between requests.
//! - Sends `ServerFrame::Done { status }` to the client IMMEDIATELY after output + commit +
//!   `.blaze/v2/profile.txt`, then runs `reclaim_between_requests()`, watch refresh, and detached
//!   background GC AFTER the client has exited.

use core::sync::atomic::{AtomicBool, Ordering};
use core::time::Duration;

use blaze_drive::Key;
use blaze_own::{AString, Alloc, Arc, Share, SharedAlloc, Vec};
use blaze_watch::Nanos;
use blaze_watch::daemon::{self, CannotServe, CheckReq, ClientFrame, DaemonConfig, DaemonCore, Death, Hello, MOST_CHUNK, ServerFrame};
use blaze_watch::helper::{CheckoutKey, Version};
use bstd::args::{ArgList, Args};
use bstd::clock::{Clock, Instant};
use bstd::cx::Cx;
use bstd::entry::Status;
use bstd::env::{Env, VarName, Vars};
use bstd::fs::{Dir, Fs, ReadWrite};
use bstd::host::Host;
use bstd::local::Local;
use bstd::mem::{Bytes, Frame, Tag};
use bstd::net::{Conn, Listener, Net, NetKind};
use bstd::out::{Out, OutSink, Stream};
use bstd::path::Path;
use bstd::procs::{By, Requests};
use bstd::term::{Size, Tty};
use bstd::Stopped;

use crate::CliError;
use crate::daemon_watch::{CheckoutWatcher, PollOutcome};
use crate::engine_check::{DaemonCheckCtx, DaemonCheckOut, HotGraph};
use crate::journal_helper::OwnExe;
use crate::phases::Phases;

/// Accept poll interval for liveness ticks (`OwnExe`, checkout existence, idle/max-life expiry).
const TICK: Duration = Duration::from_secs(1);

/// Deadline for reading a request frame or sending a short control frame (`Hello`/`Refused`/`Done`).
const CTRL_WAIT: Duration = Duration::from_secs(5);

/// Per-chunk deadline for streaming stdout/stderr to a connected client so a wedged reader cannot
/// stall the resident daemon indefinitely.
const WRITE_WAIT: Duration = Duration::from_secs(10);

/// Poll interval for the concurrent busy-reject and client-signal watcher tasks during a build.
const WATCH_POLL: Duration = Duration::from_millis(50);

/// Deadline for closing the socket cleanly on shutdown.
const END_WAIT: Duration = Duration::from_secs(2);

/// Maximum bytes read per `recv` call when framing a client request.
const RECV_STEP: usize = 64 << 10;

/// Socket folder beneath `<HOME>/.blaze`: `v2/daemon`.
pub(crate) const DAEMON_FOLDER_BENEATH_BLAZE: &str = "v2/daemon";

/// Formats the daemon's socket path relative to `<HOME>/.blaze`: `v2/daemon/<16-hex>.sock`.
pub(crate) fn socket_beneath_blaze<'f>(key: &CheckoutKey, frame: &'f Frame) -> Result<Path, CliError> {
    let rel = daemon::socket_path(key);
    let rel_str = core::str::from_utf8(&rel).map_err(|_| CliError::Text { what: "daemon socket path is not UTF-8" })?;
    let full = bstd::mem::format_in!(frame, "v2/{rel_str}");
    Ok(Path::new(full.as_str())?)
}

/// Checks whether `<HOME>/.blaze/v2/daemon/<16-hex>.sock` (including NUL) fits `sun_path`.
pub(crate) fn socket_fits(home: Option<&str>) -> bool {
    let Some(home) = home else { return false };
    let home_len = home.trim_end_matches('/').len();
    // `/<HOME_FOLDER>/v2/<SOCKET_PATH_BYTES>\0`
    let total = home_len + 1 + crate::store::HOME_FOLDER.len() + 1 + 3 + daemon::SOCKET_PATH_BYTES + 1;
    total <= bstd::local::SOCKET_PATH_MOST
}

/// `blaze daemon [folder]`: runs the resident per-checkout daemon until [`DaemonCore`] decides
/// to exit (idle, max-life, phase-limit, binary replaced, checkout gone, or explicit shutdown).
#[expect(clippy::too_many_arguments, reason = "the daemon edge: each argument one specific capability")]
pub(crate) fn run(
    cx: &Cx,
    args: &Args,
    fs: &Fs,
    env: Option<&Env>,
    net: Option<&Net>,
    host: &Host,
    clock: Clock,
    rng: &mut bstd::rng::Rng,
    out: &Out,
    err: &Out,
    phases: &mut Phases,
    heap: &'static blaze_own::Heap,
) -> Result<Status, CliError> {
    let mut dir = None;
    for word in args.each().skip(2) {
        if word.starts_with('-') || dir.is_some() {
            return Err(CliError::Usage("`blaze daemon` takes at most one checkout folder"));
        }
        dir = Some(word);
    }
    let frame = cx.frame();
    let checkout = crate::checkout::checkout(dir, frame)?;
    let located = crate::run_package::locate(cx, fs, &checkout, frame)?;
    let build = host.build_id()?;
    let version = Version::of(build.as_bytes()).ok_or(CliError::Text { what: "daemon build id length invalid" })?;
    let served = crate::engine_script_reads::Served::at_edge(cx.is_simulated(), located.at, frame)
        .map_err(crate::engine_check::EngineCheckError::ServedRoot)?;
    let Some(root_abs) = served.root_text() else {
        return Err(CliError::Text { what: "`blaze daemon` requires a real checkout path on disk" });
    };
    let home_text = env.and_then(|e| e.get(&VarName::new("HOME").ok()?).ok().flatten());
    let Some(home_str) = home_text else {
        return Err(CliError::Text { what: "`blaze daemon`: HOME is not set" });
    };
    if !socket_fits(Some(home_str)) {
        return Err(CliError::Text { what: "`blaze daemon`: socket path exceeds sun_path limit" });
    }
    let home = crate::store::Home::of(cx, fs, Some(home_str))?;
    let checkout_key = crate::journal_ask::checkout_key_of(root_abs);
    let folder_path = Path::new(DAEMON_FOLDER_BENEATH_BLAZE)?;
    let socket_path = socket_beneath_blaze(&checkout_key, frame)?;

    let exe = host.exe()?;
    let exe_dir = exe.rsplit_once('/').map(|(dir, _)| dir);
    let mut own_exe = OwnExe::open(cx, fs, &exe, build).ok();
    let instance = rng.next_u64();

    // Ensure `<HOME>/.blaze/v2/daemon` exists with 0700 permissions before binding.
    let _folder = home
        .dir()
        .make_private(cx, &folder_path)
        .map_err(|_| CliError::Text { what: "`blaze daemon`: socket folder is not private (0700)" })?;

    let born = clock.now();
    let clock = Arc::new_in(clock, heap.shared());
    let mut core = DaemonCore::new(DaemonConfig::SHIPPED, version, checkout_key, instance, Nanos(0));

    let max_store_bytes = crate::gc::budget_from_env(env)?;
    let opened = crate::store::open_for_command(
        cx,
        fs,
        host,
        rng,
        Some(home_str),
        max_store_bytes,
        &located.root,
        root_abs,
        &clock,
        err,
        heap,
    )?;
    let store_head = opened.lent.head_identity_on_disk().ok().flatten();

    let alloc = heap.alloc();
    let home_blaze_abs = bstd::mem::format_in!(frame, "{}/{}", home_str.trim_end_matches('/'), crate::store::HOME_FOLDER);
    let watcher = CheckoutWatcher::new(cx, &located.root, &clock, root_abs, Some(home_blaze_abs.as_str()), exe_dir, alloc);

    let producers = crate::producers::table(build.as_bytes());
    let tools = crate::engine_store::tools(&crate::home::tools(&producers, crate::lints::Lints::NONE.rules));
    let answer_root_str = crate::engine_answer::root_name(root_abs, frame);

    let mut state = CheckoutState {
        opened,
        store_head,
        clock: clock.share(),
        checkout_root_abs: AString::from_str_in(root_abs, alloc),
        checkout_dir_rel: AString::from_str_in(located.at, alloc),
        home_str: AString::from_str_in(home_str, alloc),
        max_store_bytes,
        answer_root: AString::from_str_in(answer_root_str.as_str(), alloc),
        tools,
        toolchain: blaze_toolchain::PINNED,
        hot_graph: None,
        hot_answer: None,
        sdk_cache: None,
        watcher,
        heap,
    };

    // Pre-arm `KqueueVnode` on the existing stored Pointer (if any) before accepting the first
    // connection so the VERY FIRST client request can hit the `< 0.5 ms` certified fast path!
    state.try_arm_from_store(cx, &located.root, None, frame);

    let mut listener = Local::beneath(home.dir())
        .listen(cx, &socket_path)
        .map_err(|_| CliError::Text { what: "`blaze daemon`: failed to bind Unix socket (another daemon may be active)" })?;

    let mut conns = Frame::new(Tag::Other).map_err(|_| CliError::Text { what: "`blaze daemon`: connection frame allocation failed" })?;
    let lived = serve_loop(
        cx,
        &mut listener,
        &located.root,
        fs,
        net,
        host,
        &clock,
        rng,
        born,
        &mut core,
        &mut state,
        own_exe.as_mut(),
        &mut conns,
        phases,
        err,
    );

    let _ = listener.close(&cx.until(clock.after(END_WAIT)));
    state.watcher.shutdown(cx, &clock);

    let death = lived?;
    let _ = out.line(cx, format_args!("blaze daemon: exited: {death}"));
    Ok(Status::OK)
}

/// Build-affecting environment fingerprint: if any of these change between requests, we bypass
/// the certified fast path and let `engine_check` evaluate the updated environment.
#[derive(PartialEq, Eq)]
struct EnvFingerprint {
    sdkroot: Option<AString<'static>>,
    path: Option<AString<'static>>,
    developer_dir: Option<AString<'static>>,
    toolchains: Option<AString<'static>>,
    encoded_rustflags: Option<AString<'static>>,
    rustflags: Option<AString<'static>>,
    build_rustflags: Option<AString<'static>>,
    target_rustflags: Option<AString<'static>>,
    cargo_home: Option<AString<'static>>,
    home: Option<AString<'static>>,
    cert_file: Option<AString<'static>>,
    cert_dir: Option<AString<'static>>,
    max_store_bytes: Option<AString<'static>>,
    max_store_gb: Option<AString<'static>>,
    no_lan_peer: Option<AString<'static>>,
}

impl EnvFingerprint {
    fn from_env(env: &Env, alloc: Alloc<'static>) -> EnvFingerprint {
        let get = |name: &str| -> Option<AString<'static>> {
            let var = VarName::new(name).ok()?;
            let val = env.get(&var).ok().flatten()?;
            Some(AString::from_str_in(val, alloc))
        };
        let target_var = blaze_manifest::rustflags::target_variable(crate::graph::TRIPLE, alloc);
        EnvFingerprint {
            sdkroot: get("SDKROOT"),
            path: get("PATH"),
            developer_dir: get("DEVELOPER_DIR"),
            toolchains: get("TOOLCHAINS"),
            encoded_rustflags: get("CARGO_ENCODED_RUSTFLAGS"),
            rustflags: get("RUSTFLAGS"),
            build_rustflags: get("CARGO_BUILD_RUSTFLAGS"),
            target_rustflags: get(target_var.as_str()),
            cargo_home: get("CARGO_HOME"),
            home: get("HOME"),
            cert_file: get("SSL_CERT_FILE"),
            cert_dir: get("SSL_CERT_DIR"),
            max_store_bytes: get("BLAZE_STORE_MAX_BYTES"),
            max_store_gb: get("BLAZE_STORE_MAX_GB"),
            no_lan_peer: get("BLAZE_NO_LAN_PEER"),
        }
    }
}

/// Cached macOS SDK resolution keyed by the environment variables `sdk::resolve_how` reads.
/// Stores the resolved folder and minimal manifest snippets so `blaze_apple_sdk::resolve`
/// reconstructs an identical `Sdk<'_>` in ~0.1 µs with zero filesystem I/O or `xcrun` spawn.
struct SdkCache {
    sdkroot: Option<AString<'static>>,
    path: Option<AString<'static>>,
    developer_dir: Option<AString<'static>>,
    home: Option<AString<'static>>,
    toolchains: Option<AString<'static>>,
    folder: AString<'static>,
    found: blaze_apple_sdk::Found,
    settings_json: AString<'static>,
    system_version_plist: AString<'static>,
}

impl SdkCache {
    fn matches(&self, env: &Env) -> bool {
        let same = |cached: &Option<AString<'static>>, name: &str| -> bool {
            let current = VarName::new(name).ok().and_then(|v| env.get(&v).ok().flatten());
            cached.as_deref() == current
        };
        same(&self.sdkroot, "SDKROOT")
            && same(&self.path, "PATH")
            && same(&self.developer_dir, "DEVELOPER_DIR")
            && same(&self.home, "HOME")
            && same(&self.toolchains, "TOOLCHAINS")
    }

    fn as_sdk(&self) -> Option<blaze_apple_sdk::Sdk<'_>> {
        let (sdkroot, xcrun_out) = match self.found {
            blaze_apple_sdk::Found::SdkRoot => (Some(self.folder.as_str()), ""),
            blaze_apple_sdk::Found::Xcrun => (None, self.folder.as_str()),
        };
        blaze_apple_sdk::resolve(
            sdkroot,
            || Ok(xcrun_out),
            |_| Ok(self.settings_json.as_str()),
            |_| Ok(self.system_version_plist.as_str()),
        )
        .ok()
    }
}

/// Lightweight metadata for the `< 0.5 ms` certified warm hit (Rule 51: rendered stdout bytes
/// stay in CAS mmap via `ViewSlot::load`, zero duplicate buffers in daemon heap).
struct HotAnswer {
    pointer_key: Key,
    lines_key: Key,
    facts: crate::engine_answer::Facts,
    lint_step: blaze_lint::Step,
    env_fp: Option<EnvFingerprint>,
}

/// Persistent checkout state held in the resident daemon across requests.
struct CheckoutState<'d> {
    opened: crate::store::Opened,
    store_head: Option<blaze_cas::plain::HeadIdentity>,
    clock: Arc<Clock, SharedAlloc<'static>>,
    checkout_root_abs: AString<'static>,
    checkout_dir_rel: AString<'static>,
    home_str: AString<'static>,
    max_store_bytes: Option<u64>,
    answer_root: AString<'static>,
    tools: blaze_engine::Tools,
    toolchain: blaze_toolchain::ToolchainId,
    hot_graph: Option<HotGraph>,
    hot_answer: Option<HotAnswer>,
    sdk_cache: Option<SdkCache>,
    watcher: CheckoutWatcher<'d>,
    heap: &'static blaze_own::Heap,
}

impl CheckoutState<'_> {
    /// Checks `head_identity_on_disk()` (~2 µs) and per-request store budget; if an external
    /// writer, `blaze gc`, `blaze clean --store` + recreate, or a changed `BLAZE_STORE_MAX_*`
    /// budget differs from the currently open store, drops cached answer/watch state and reopens
    /// the store fresh. Keeps the content-keyed `hot_graph` so reopens do not re-leak the same
    /// immutable crate list.
    #[expect(clippy::too_many_arguments, reason = "store freshness check: explicit capabilities")]
    fn ensure_store_fresh(
        &mut self,
        cx: &Cx,
        fs: &Fs,
        host: &Host,
        rng: &mut bstd::rng::Rng,
        root: &Dir<ReadWrite>,
        req_max_store_bytes: Option<u64>,
        clock: &Clock,
        err: &Out,
    ) -> Result<bool, CliError> {
        let budget_changed = req_max_store_bytes != self.max_store_bytes;
        let disk_head = self.opened.lent.head_identity_on_disk().ok().flatten();
        if !budget_changed && disk_head.is_some() && disk_head == self.store_head {
            return Ok(false);
        }
        // External write, GC compaction, store wipe/recreate, or budget change -> reopen fresh.
        self.max_store_bytes = req_max_store_bytes;
        self.hot_answer = None;
        self.watcher.disarm();
        self.opened = crate::store::open_for_command(
            cx,
            fs,
            host,
            rng,
            Some(self.home_str.as_str()),
            self.max_store_bytes,
            root,
            self.checkout_root_abs.as_str(),
            clock,
            err,
            self.heap,
        )?;
        self.store_head = self.opened.lent.head_identity_on_disk().ok().flatten();
        Ok(true)
    }

    /// Reads the current `Pointer` from the already-open store mmap (~5 µs) and arms `watcher`
    /// + populates `hot_answer`.
    fn try_arm_from_store(&mut self, cx: &Cx, root: &Dir<ReadWrite>, env_fp: Option<EnvFingerprint>, frame: &Frame) {
        let erased = blaze_own::Erased::new(frame);
        let clock_dummy_err = crate::store::PlainPlace::new(&self.opened.lent, false);
        let root_bytes = match clock_dummy_err.root(self.answer_root.as_str()) {
            Ok(Some(held)) => held.bytes,
            _ => {
                self.hot_answer = None;
                self.watcher.disarm();
                return;
            }
        };
        let Ok(key_bytes) = <[u8; 32]>::try_from(root_bytes) else {
            self.hot_answer = None;
            self.watcher.disarm();
            return;
        };
        let pointer_key = Key(blaze_ids::ContentId(key_bytes));
        let Ok(Some(held)) = clock_dummy_err.get(pointer_key) else {
            self.hot_answer = None;
            self.watcher.disarm();
            return;
        };
        let Ok(Ok(pointer)) = crate::engine_answer::decode_pointer_bytes(pointer_key, held.bytes) else {
            self.hot_answer = None;
            self.watcher.disarm();
            return;
        };
        if !pointer.certifies_under(&self.tools) {
            self.hot_answer = None;
            self.watcher.disarm();
            return;
        }
        let _ = erased;
        self.watcher.arm(cx, root, pointer.key, &pointer.leaves, frame);
        let lint_step = crate::lints::of_root(cx, root, frame).map_or(blaze_lint::Step::Rust, |l| l.step);
        self.hot_answer = Some(HotAnswer {
            pointer_key: pointer.key,
            lines_key: pointer.lines,
            facts: pointer.facts,
            lint_step,
            env_fp,
        });
    }

    /// Resolves and caches macOS SDK for `env` so incremental edits pay 0 µs for SDK lookup.
    fn resolve_sdk<'s>(
        &'s mut self,
        cx: &Cx,
        fs: &Fs,
        env: &Env,
        clock: &Clock,
        frame: &Frame,
    ) -> Option<blaze_apple_sdk::Sdk<'s>> {
        if let Some(cached) = &self.sdk_cache
            && cached.matches(env)
        {
            return cached.as_sdk();
        }
        let var = |name: &str| -> Option<&str> { env.get(&VarName::new(name).ok()?).ok().flatten() };
        let (sdk, _) = crate::sdk::resolve_how(
            cx,
            fs,
            var("SDKROOT"),
            &crate::sdk::XcrunEnv {
                path: var("PATH"),
                developer_dir: var("DEVELOPER_DIR"),
                home: var("HOME"),
                toolchains: var("TOOLCHAINS"),
            },
            crate::sdk::SHIPPED,
            clock,
            frame,
        );
        let Ok(s) = sdk else {
            return None;
        };
        let alloc = self.heap.alloc();
        let own = |name: &str| var(name).map(|v| AString::from_str_in(v, alloc));
        let settings_text = bstd::mem::format_in!(frame, "{{\"Version\":\"{}\"}}", s.version_text());
        let build_text = bstd::mem::format_in!(
            frame,
            "<key>ProductBuildVersion</key><string>{}</string>",
            s.identity().build,
        );
        let cache = self.sdk_cache.insert(SdkCache {
            sdkroot: own("SDKROOT"),
            path: own("PATH"),
            developer_dir: own("DEVELOPER_DIR"),
            home: own("HOME"),
            toolchains: own("TOOLCHAINS"),
            folder: AString::from_str_in(s.folder(), alloc),
            found: s.found(),
            settings_json: AString::from_str_in(settings_text.as_str(), alloc),
            system_version_plist: AString::from_str_in(build_text.as_str(), alloc),
        });
        cache.as_sdk()
    }
}

/// Streams `Out` or `Err` chunks as framed `ServerFrame::Out` / `ServerFrame::Err` over the
/// client's Unix socket using `Conn::send_vectored` (zero intermediate frame buffer allocation).
struct ConnOutSink {
    conn: Arc<Conn, SharedAlloc<'static>>,
    clock: Arc<Clock, SharedAlloc<'static>>,
    stream: Stream,
}

impl OutSink for ConnOutSink {
    fn write_chunk(&self, cx: &Cx, bytes: &[u8]) -> Result<(), &'static str> {
        let is_err = self.stream == Stream::Err;
        for chunk in bytes.chunks(MOST_CHUNK) {
            let (head, head_len) = daemon::encode_chunk_header(is_err, chunk.len());
            let head_slice = head.get(..head_len).unwrap_or(&[]);
            let write_cx = cx.until(self.clock.after(WRITE_WAIT));
            self.conn
                .send_vectored(&write_cx, &[head_slice, chunk])
                .map_err(|_| "daemon client socket write failed")?;
        }
        Ok(())
    }
}

/// Main accept loop driven by [`DaemonCore`].
#[expect(clippy::too_many_arguments, reason = "the daemon accept loop: each capability passed explicitly")]
fn serve_loop(
    cx: &Cx,
    listener: &mut Listener,
    checkout_root: &Dir<ReadWrite>,
    fs: &Fs,
    net: Option<&Net>,
    host: &Host,
    clock: &Clock,
    rng: &mut bstd::rng::Rng,
    born: Instant,
    core: &mut DaemonCore,
    state: &mut CheckoutState<'_>,
    mut own_exe: Option<&mut OwnExe>,
    conns: &mut Frame,
    phases: &mut Phases,
    daemon_err: &Out,
) -> Result<Death, CliError> {
    let mut accept_errs = 0u32;
    loop {
        if let Some(death) = core.death() {
            return Ok(death);
        }
        let own_replaced = match own_exe.as_deref_mut() {
            Some(exe) => conns.scope(|frame| exe.look(cx, frame).is_some()),
            None => false,
        };
        let checkout_gone = checkout_is_gone(state.checkout_root_abs.as_str());
        if let Some(death) = core.on_tick(nanos(clock, born), own_replaced, checkout_gone) {
            return Ok(death);
        }
        let accepted = {
            let cx = cx.span("blaze daemon: waiting for client connection (1s tick)");
            listener.accept(&cx.until(clock.after(TICK)))
        };
        let conn = match accepted {
            Ok(conn) => {
                accept_errs = 0;
                conn
            }
            Err(error) if matches!(error.kind, NetKind::Stopped(Stopped::Deadline { .. })) => {
                accept_errs = 0;
                continue;
            }
            Err(error) if matches!(error.kind, NetKind::Driver(_)) && accept_errs < 16 => {
                // Transient per-connection accept failure (e.g. client closed mid-handshake
                // causing ECONNABORTED/ECONNRESET/ENOTCONN). Bounded at 16 in a row (rule 18).
                accept_errs += 1;
                continue;
            }
            Err(_) => return Ok(Death::Shutdown),
        };

        let post = conns.scope(|frame| {
            handle_connection(cx, listener, conn, checkout_root, fs, net, host, clock, rng, born, core, state, phases, frame)
        })?;
        conns.trim(2 << 20);

        // POST-DONE work: client has ALREADY received `ServerFrame::Done` and closed!
        // Note: any required watcher re-arm ALREADY ran BEFORE `ServerFrame::Done` so the client
        // can never race an edit into an unarmed `kqueue`.
        if let Some(post) = post {
            crate::store::post_done_for_daemon(cx, &mut state.opened.lent, &state.opened.home, post.spawn_bg, daemon_err);
            state.store_head = state.opened.lent.head_identity_on_disk().ok().flatten();
            if let Some(death) = core.on_request_done(nanos(clock, born), post.was_phase_rebuild) {
                return Ok(death);
            }
        }
        if let Some(death) = core.death() {
            return Ok(death);
        }
    }
}

/// Deferred maintenance executed AFTER `ServerFrame::Done` has been flushed to the client.
struct PostDone {
    was_phase_rebuild: bool,
    spawn_bg: bool,
}

/// Serves one accepted Unix-socket client connection.
#[expect(clippy::too_many_arguments, reason = "one client connection: each capability passed explicitly")]
fn handle_connection(
    cx: &Cx,
    listener: &mut Listener,
    conn: Conn,
    checkout_root: &Dir<ReadWrite>,
    fs: &Fs,
    net: Option<&Net>,
    host: &Host,
    clock: &Clock,
    rng: &mut bstd::rng::Rng,
    born: Instant,
    core: &mut DaemonCore,
    state: &mut CheckoutState<'_>,
    phases: &mut Phases,
    frame: &Frame,
) -> Result<Option<PostDone>, CliError> {
    let ctrl_cx = cx.until(clock.after(CTRL_WAIT));
    // 1. Send Hello greeting immediately.
    let mut hello_bytes = Vec::new_in(frame);
    core.hello().encode(&mut hello_bytes);
    if conn.send_all(&ctrl_cx, &hello_bytes).is_err() {
        let _ = conn.finish(&cx.until(clock.after(END_WAIT)));
        return Ok(None);
    }

    // 2. Read one ClientFrame.
    let mut req_buf = Bytes::new_in(frame, "daemon client frame");
    let body = match read_daemon_frame(&ctrl_cx, &conn, &mut req_buf) {
        Ok(body) => body,
        Err(()) => {
            let _ = conn.finish(&cx.until(clock.after(END_WAIT)));
            return Ok(None);
        }
    };
    let client_frame = match ClientFrame::decode_in(body, frame) {
        Ok(f) => f,
        Err(_) => {
            let _ = conn.finish(&cx.until(clock.after(END_WAIT)));
            return Ok(None);
        }
    };

    match client_frame {
        ClientFrame::Shutdown => {
            core.on_shutdown();
            let mut done = Vec::new_in(frame);
            ServerFrame::Done { status: 0 }.encode(&mut done);
            let _ = conn.send_all(&ctrl_cx, &done);
            let _ = conn.finish(&cx.until(clock.after(END_WAIT)));
            Ok(None)
        }
        ClientFrame::Signal(_) => {
            let _ = conn.finish(&cx.until(clock.after(END_WAIT)));
            Ok(None)
        }
        ClientFrame::Check(req) => {
            if let Err(why) = core.on_hello_check(req.version, req.checkout) {
                let mut refused = Vec::new_in(frame);
                ServerFrame::Refused { why }.encode(&mut refused);
                let _ = conn.send_all(&ctrl_cx, &refused);
                let _ = conn.finish(&cx.until(clock.after(END_WAIT)));
                return Ok(None);
            }
            core.on_request_start(nanos(clock, born));
            let hello = core.hello();
            let saw_shutdown = AtomicBool::new(false);
            let saw_replaced = AtomicBool::new(false);
            let shared = state.heap.shared();
            let conn_arc = Arc::new_in(conn, shared);
            let (status, post) = execute_check_request(
                cx,
                listener,
                hello,
                &saw_shutdown,
                &saw_replaced,
                &conn_arc,
                &req,
                checkout_root,
                fs,
                net,
                host,
                clock,
                rng,
                state,
                phases,
                frame,
            );
            // Send ServerFrame::Done IMMEDIATELY before any post-Done cleanup!
            let mut done = Vec::new_in(frame);
            ServerFrame::Done { status: status.0 }.encode(&mut done);
            let _ = conn_arc.send_all(&cx.until(clock.after(CTRL_WAIT)), &done);
            if let Ok(conn) = Arc::try_unwrap(conn_arc) {
                let _ = conn.finish(&cx.until(clock.after(END_WAIT)));
            }
            if saw_shutdown.load(Ordering::Acquire) {
                core.on_shutdown();
            } else if saw_replaced.load(Ordering::Acquire) {
                let _ = core.on_tick(nanos(clock, born), true, false);
            }
            Ok(Some(post))
        }
    }
}

/// Executes one validated `CheckReq` over `conn_arc`, returning `(Status, PostDone)`.
#[expect(clippy::too_many_arguments, reason = "one check request: each capability passed explicitly")]
fn execute_check_request(
    cx: &Cx,
    listener: &mut Listener,
    hello: Hello,
    saw_shutdown: &AtomicBool,
    saw_replaced: &AtomicBool,
    conn_arc: &Arc<Conn, SharedAlloc<'static>>,
    req: &CheckReq<'_, &Frame>,
    checkout_root: &Dir<ReadWrite>,
    fs: &Fs,
    net: Option<&Net>,
    host: &Host,
    clock: &Clock,
    rng: &mut bstd::rng::Rng,
    state: &mut CheckoutState<'_>,
    phases: &mut Phases,
    frame: &Frame,
) -> (Status, PostDone) {
    let alloc = state.heap.alloc();
    let shared = state.heap.shared();
    let out_tty = tty_of(req.stdout_tty, req.cols);
    let err_tty = tty_of(req.stderr_tty, req.cols);
    let out = Out::with_sink_in(
        Stream::Out,
        out_tty,
        ConnOutSink { conn: conn_arc.share(), clock: state.clock.share(), stream: Stream::Out },
        shared,
    );
    let err = Out::with_sink_in(
        Stream::Err,
        err_tty,
        ConnOutSink { conn: conn_arc.share(), clock: state.clock.share(), stream: Stream::Err },
        shared,
    );

    let (args, env) = match build_args_env(req, state.checkout_root_abs.as_str(), state.checkout_dir_rel.as_str(), shared, frame) {
        Ok(pair) => pair,
        Err(msg) => {
            let _ = err.line(cx, format_args!("error: {msg}"));
            return (Status::FAILED, PostDone { was_phase_rebuild: false, spawn_bg: false });
        }
    };
    let flags = match crate::flags(&args).and_then(|f| {
        crate::check_command::refuse_not_taken(&f)?;
        Ok(f)
    }) {
        Ok(f) => f,
        Err(error) => {
            let _ = err.line(cx, format_args!("error: {error}"));
            return (Status::FAILED, PostDone { was_phase_rebuild: false, spawn_bg: false });
        }
    };
    let req_max_store_bytes = match crate::gc::budget_from_env(Some(&env)) {
        Ok(budget) => budget,
        Err(error) => {
            let _ = err.line(cx, format_args!("error: {error}"));
            return (Status::FAILED, PostDone { was_phase_rebuild: false, spawn_bg: false });
        }
    };

    phases.reset();
    let began = phases.now();

    // 1. O(1) external-writer / GC / wipe / budget check (~2 µs).
    let reopened = match state.ensure_store_fresh(cx, fs, host, rng, checkout_root, req_max_store_bytes, clock, &err) {
        Ok(r) => r,
        Err(error) => {
            let _ = err.line(cx, format_args!("error: {error}"));
            return (Status::FAILED, PostDone { was_phase_rebuild: false, spawn_bg: false });
        }
    };
    state.opened.lent.rearm_lease();

    // 2. Synchronous kernel vnode poll (~10 µs when clean!).
    let poll = state.watcher.poll(cx, clock, frame);
    let poll_was_clean = matches!(poll, PollOutcome::Clean { .. });
    let poll_needs_refresh = match &poll {
        PollOutcome::Clean { .. } => false,
        PollOutcome::Dirty { needs_refresh, .. } | PollOutcome::FullRescan { needs_refresh, .. } => *needs_refresh,
        PollOutcome::FsEvents { .. } => false,
    };

    let env_fp = EnvFingerprint::from_env(&env, alloc);

    // 3. ULTRA-FAST CERTIFIED WARM HIT (< 0.2 ms daemon time!).
    let eligible_fast = !reopened
        && poll_was_clean
        && flags.fix.is_none()
        && !flags.names_oracle.on()
        && flags.origin.is_none()
        && flags.origin_key.is_none()
        && flags.status_every.is_none()
        && flags.block_scopes == blaze_scope::surface::BlockScopes::Modules;

    if eligible_fast
        && let Some(hot) = state.hot_answer.as_ref()
        && state.watcher.is_armed_for(hot.pointer_key)
        && hot.facts.printed.replayable
        && hot.facts.owed == 0
        && hot.env_fp.as_ref() == Some(&env_fp)
    {
        if let Some((status, spawn_bg)) = try_fast_certified(
            cx,
            checkout_root,
            state,
            hot,
            &flags,
            &env,
            &out,
            &err,
            &mut *phases,
            began,
            frame,
        ) {
            state.store_head = state.opened.lent.head_identity_on_disk().ok().flatten();
            return (
                status,
                PostDone {
                    was_phase_rebuild: false,
                    spawn_bg,
                },
            );
        }
    }

    // 4. Incremental Edit or Full Check through `engine_check::check_in_daemon`.
    let sdk_cached = state.resolve_sdk(cx, fs, &env, clock, frame);
    let sdk_copied = sdk_cached.and_then(|s| {
        let folder: &str = frame.alloc_str(s.folder());
        let settings_text = bstd::mem::format_in!(frame, "{{\"Version\":\"{}\"}}", s.version_text());
        let build_text = bstd::mem::format_in!(
            frame,
            "<key>ProductBuildVersion</key><string>{}</string>",
            s.identity().build,
        );
        let (sdkroot, xcrun_out): (Option<&str>, &str) = match s.found() {
            blaze_apple_sdk::Found::SdkRoot => (Some(folder), ""),
            blaze_apple_sdk::Found::Xcrun => (None, folder),
        };
        blaze_apple_sdk::resolve(
            sdkroot,
            || Ok(xcrun_out),
            |_| Ok(&*frame.alloc_str(settings_text.as_str())),
            |_| Ok(&*frame.alloc_str(build_text.as_str())),
        )
        .ok()
    });

    let dir_handle = if state.checkout_dir_rel.is_empty() {
        fs.root()
    } else {
        match Path::new(state.checkout_dir_rel.as_str()).ok().and_then(|p| fs.open_dir(cx, &p).ok()) {
            Some(dir) => dir,
            None => fs.root(),
        }
    };

    let prev_hot_graph = state.hot_graph.as_ref().map(|g| (g.key, g.out));
    let requests = Requests::new();
    let (status, dout) = {
        let bg_scope = cx.scope();
        let busy_task = bg_scope.must_overlap("daemon busy rejector", 64 << 10, move |bg_cx| {
            reject_concurrent_clients(bg_cx, listener, hello, saw_shutdown, saw_replaced, clock);
            Ok::<(), core::convert::Infallible>(())
        });
        let sig_conn = conn_arc.share();
        let requests_ref = &requests;
        let sig_task = bg_scope.must_overlap("daemon client signal watch", 64 << 10, move |bg_cx| {
            watch_client_signals(bg_cx, &sig_conn, requests_ref, saw_shutdown, clock);
            Ok::<(), core::convert::Infallible>(())
        });

        let mut dctx = DaemonCheckCtx {
            opened: &state.opened,
            watch: Some(poll),
            hot_graph: &mut state.hot_graph,
            requests: Some(&requests),
            out: DaemonCheckOut::default(),
        };
        let status = match crate::engine_check::check_in_daemon(
            cx,
            &args,
            fs,
            Some(&env),
            net,
            host,
            clock,
            rng,
            &out,
            &err,
            phases,
            state.heap,
            dir_handle,
            state.checkout_root_abs.as_str(),
            sdk_copied,
            &mut dctx,
        ) {
            Ok(status) => status,
            Err(CliError::EngineCheck(crate::engine_check::EngineCheckError::Stopped { by, .. })) => Status(128 + by.number()),
            Err(error) => {
                let _ = err.line(cx, format_args!("error: {error}"));
                Status::FAILED
            }
        };
        bg_scope.cancel();
        if let Ok(task) = busy_task {
            let _ = task.join(cx);
        }
        if let Ok(task) = sig_task {
            let _ = task.join(cx);
        }
        let _ = bg_scope.join(cx);
        (status, dctx.out)
    };
    state.store_head = state.opened.lent.head_identity_on_disk().ok().flatten();

    let replaced_hot_graph = prev_hot_graph.is_some()
        && state.hot_graph.as_ref().map(|g| (g.key, g.out)) != prev_hot_graph;

    let certified_update = dout
        .certified_facts
        .and_then(|(pkey, lkey, facts)| dout.lint_step.map(|ls| (pkey, lkey, facts, ls)));

    // Keep the existing `KqueueVnode` alive across in-place `ready` edits (ZERO FDs closed,
    // ZERO FDs reopened, ZERO race gap!). Only re-arm when structural changes occurred (rename,
    // create, delete, phase rebuild, leaf set changed, store reopen, or unarmed watcher) — AND
    // always re-arm BEFORE returning `ServerFrame::Done` so the client cannot race an edit!
    let needs_watch_refresh = poll_needs_refresh
        || reopened
        || dout.was_phase_rebuild
        || dout.leaf_set_changed
        || !state.watcher.is_armed();

    if certified_update.is_none() && !poll_was_clean {
        // Consumed dirty/rescan events without producing a new certified Pointer: clear
        // `hot_answer` AND disarm the watcher so the next request cannot treat the drained
        // kqueue as clean against the stale pre-edit Pointer.
        state.hot_answer = None;
        state.watcher.disarm();
    } else if needs_watch_refresh {
        state.try_arm_from_store(cx, checkout_root, Some(env_fp), frame);
    } else if let Some((pkey, lkey, facts, lint_step)) = certified_update {
        state.watcher.set_armed_pointer(pkey);
        state.hot_answer = Some(HotAnswer {
            pointer_key: pkey,
            lines_key: lkey,
            facts,
            lint_step,
            env_fp: Some(env_fp),
        });
    } else {
        // Clean request errored without producing a pointer: clear `hot_answer` while keeping the live watcher.
        state.hot_answer = None;
    }

    (
        status,
        PostDone {
            was_phase_rebuild: dout.was_phase_rebuild || replaced_hot_graph,
            spawn_bg: dout.spawn_bg,
        },
    )
}

/// Ultra-fast certified warm hit (< 0.2 ms): streams pre-rendered `ViewSlot` bytes directly from
/// CAS mmap, prints the exact byte-identical closing line, renews roots in O(1), and writes
/// `.blaze/v2/profile.txt`. Returns `None` if presentation misses (falling through cleanly to
/// `check_in_daemon` without having written any bytes).
#[expect(clippy::too_many_arguments, reason = "fast certified path: explicit capabilities")]
fn try_fast_certified(
    cx: &Cx,
    root: &Dir<ReadWrite>,
    state: &CheckoutState<'_>,
    hot: &HotAnswer,
    flags: &crate::Flags<'_>,
    env: &Env,
    out: &Out,
    err: &Out,
    phases: &mut Phases,
    began: Instant,
    frame: &Frame,
) -> Option<(Status, bool)> {
    let var = |name: &str| -> Option<&str> { env.get(&VarName::new(name).ok()?).ok().flatten() };
    let terminal = out.is_terminal();
    let caps = blaze_report::frame::detect(&var, terminal, out.size().map(|s| s.cols), flags.color);
    let view = blaze_report::frame::plain::view(flags.view, terminal);
    let mut shown = crate::print::Shown::new(caps, view, state.checkout_root_abs.as_str(), frame);
    shown.each = flags.each;
    shown.output_only = true;
    shown.lints = hot.lint_step;

    let view_slot = crate::engine_lines::ViewSlot::load(
        &state.opened.lent,
        state.answer_root.as_str(),
        false,
        &shown,
        &state.tools,
        frame,
    )
    .ok()?;

    // Presentation miss writes zero bytes and returns `None` -> clean fallthrough.
    let tally = view_slot.certified(cx, hot.lines_key, &mut shown, frame, out).ok()??;

    let whole = hot.facts.printed.bodies_whole;
    let (ran_stages, not_stages) = crate::check_coverage(hot.facts.printed.names, whole, whole && tally.borrows == 0, frame);
    let coverage = (!not_stages.is_empty()).then_some(blaze_report::frame::Coverage { ran: &ran_stages, not: &not_stages });
    let silent_clean = shown.quiet_when_clean
        && shown.counts.errors == 0
        && shown.counts.warnings == 0
        && tally.unchecked == 0
        && shown.behind.items == 0;
    if !silent_clean {
        if crate::print::stage_notes(cx, &not_stages, &shown, frame, out).is_err() {
            return None;
        }
        let unchecked_only = (tally.unchecked > 0 || shown.behind.items > 0) && shown.counts.errors == 0 && shown.counts.failures == 0;
        if !unchecked_only
            && crate::print::closing(cx, &shown, hot.facts.files, phases.since(began), Some(0), None, coverage, frame, out).is_err()
        {
            return None;
        }
    }
    if let Some(words) = crate::not_checked::items::summary(tally.unchecked, shown.behind, frame)
        && crate::print::note(cx, words.as_str(), &shown, frame, out).is_err()
    {
        return None;
    }
    let status = crate::engine_check::status_of(&tally, 0, &shown, coverage.is_some());

    // Renew roots in O(1) directly through `PlainPlace` (0 disk commits unless past half of ROOT_LIFE_NS).
    let place = crate::store::PlainPlace::new(&state.opened.lent, false);
    let reach = bstd::mem::format_in!(frame, "{}.{}", state.answer_root.as_str(), crate::engine_answer::REACH_ROOT);
    let answer_index = bstd::mem::format_in!(frame, "{}.{}", state.answer_root.as_str(), crate::engine_answer::ANSWER_ROOT);
    let journal_root = crate::journal_checkpoint::name(state.answer_root.as_str(), frame);
    let seeds_root = crate::engine_seeds::root_name(&state.tools, frame);
    let life = blaze_cas::plain::ROOT_LIFE_NS;
    let mut reach_shards = Vec::new_in(frame);
    crate::engine_answer::push_reach_shards_plain(&place, state.answer_root.as_str(), frame, &mut reach_shards);
    for root_name in [
        state.answer_root.as_str(),
        reach.as_str(),
        answer_index.as_str(),
        journal_root.as_str(),
        view_slot.name.as_str(),
        seeds_root.as_str(),
    ] {
        let _ = place.renew(root_name, life);
    }
    for shard in &reach_shards {
        let _ = place.renew(shard.as_str(), life);
    }
    let _ = place.renew(crate::store::roots::RootName::toolchain(state.toolchain).as_str(), crate::store::roots::TOOLCHAIN_LIFE_NS);

    let (cleanup, spawn_bg) = crate::store::after_answer_for_daemon(cx, &state.opened.lent, &state.opened.home).ok()?;
    phases.mark_cx(cx, "daemon certified fast hit", frame);

    // Write `.blaze/v2/profile.txt` cleanly before Done.
    let backend = state.watcher.backend_name();
    let profile = bstd::mem::format_in!(
        frame,
        "certificate stood (resident daemon fast path, {backend}: 0 events; 0 tasks entered, 0 pages read)\n{cleanup}\n{phases}"
    );
    let _ = crate::print::ensure_checkout_ignore(cx, root);
    if let Ok(path) = Path::new(crate::print::PROFILE)
        && let Ok(file) = root.create(cx, &path)
    {
        let _ = file.append(cx, profile.as_bytes(), bstd::fs::Durable::No);
        let _ = file.close(cx, bstd::fs::Durable::No);
    }

    if flags.stats {
        let in_main = phases.in_main();
        let ms = in_main / 1_000_000;
        let micros = (in_main / 1_000) % 1_000;
        let _ = err.line(
            cx,
            format_args!(
                "blaze: 0 of 0 task(s) entered, 0 record(s) put, 0 page(s) written, certificate stood; {ms}.{micros:03} ms in main; {}",
                crate::print::PROFILE,
            ),
        );
    }

    Some((status, spawn_bg))
}

fn tty_of(is_tty: bool, cols: Option<u16>) -> Tty {
    if !is_tty {
        return Tty::NotTerminal;
    }
    Tty::Terminal(cols.map(|c| Size { cols: c, rows: 24 }))
}

#[derive(Debug)]
enum ReqInputError {
    ArgvNotUtf8 { index: usize },
    CwdNotUtf8,
}

impl core::fmt::Display for ReqInputError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ReqInputError::ArgvNotUtf8 { index } => write!(f, "client argv[{index}] is not UTF-8"),
            ReqInputError::CwdNotUtf8 => f.write_str("client cwd is not UTF-8"),
        }
    }
}

fn daemon_fs_root_abs<'a>(checkout_root_abs: &'a str, checkout_dir_rel: &str) -> &'a str {
    if checkout_dir_rel.is_empty() {
        return checkout_root_abs;
    }
    let trimmed_root = checkout_root_abs.trim_end_matches('/');
    let trimmed_rel = checkout_dir_rel.trim_matches('/');
    if let Some(prefix) = trimmed_root.strip_suffix(trimmed_rel)
        && let Some(parent) = prefix.strip_suffix('/')
    {
        return if parent.is_empty() { "/" } else { parent };
    }
    checkout_root_abs
}

fn rebase_relative_path<'f>(req_cwd: &str, raw: &str, fs_root_abs: &str, frame: &'f Frame) -> Option<&'f str> {
    if raw.starts_with("http://") || raw.starts_with("https://") || raw.starts_with('/') {
        return None;
    }
    let mut parts: Vec<&str, &Frame> = Vec::new_in(frame);
    for seg in req_cwd.split('/').chain(raw.split('/')) {
        match seg {
            "" | "." => {}
            ".." => {
                let _ = parts.pop();
            }
            other => parts.push(other),
        }
    }
    let mut root_parts: Vec<&str, &Frame> = Vec::new_in(frame);
    for seg in fs_root_abs.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                let _ = root_parts.pop();
            }
            other => root_parts.push(other),
        }
    }
    if parts.len() < root_parts.len() {
        return None;
    }
    for (r, p) in root_parts.iter().zip(parts.iter()) {
        if r != p {
            return None;
        }
    }
    let tail = parts.get(root_parts.len()..)?;
    if tail.is_empty() {
        return Some(".");
    }
    let mut out = blaze_own::String::new_in(frame);
    for (i, seg) in tail.iter().enumerate() {
        if i > 0 {
            out.push('/');
        }
        out.push_str(seg);
    }
    Some(frame.alloc_str(out.as_str()))
}

fn build_args_env(
    req: &CheckReq<'_, &Frame>,
    checkout_root_abs: &str,
    checkout_dir_rel: &str,
    alloc: SharedAlloc<'static>,
    frame: &Frame,
) -> Result<(Args, Env), ReqInputError> {
    let req_cwd = core::str::from_utf8(req.cwd).map_err(|_| ReqInputError::CwdNotUtf8)?;
    let fs_root_abs = daemon_fs_root_abs(checkout_root_abs, checkout_dir_rel);
    let mut argv: Vec<&str, &Frame> = Vec::with_capacity_in(req.argv.len(), frame);
    let mut next_is_origin = false;
    for (index, raw) in req.argv.iter().enumerate() {
        let s = core::str::from_utf8(raw).map_err(|_| ReqInputError::ArgvNotUtf8 { index })?;
        if next_is_origin {
            next_is_origin = false;
            if let Some(rebased) = rebase_relative_path(req_cwd, s, fs_root_abs, frame) {
                argv.push(rebased);
                continue;
            }
        } else if s == "--origin" {
            next_is_origin = true;
        }
        argv.push(s);
    }
    let mut vars: Vec<(&str, &str), &Frame> = Vec::with_capacity_in(req.env.len(), frame);
    for &(k, v) in req.env.iter() {
        if let (Ok(ks), Ok(vs)) = (core::str::from_utf8(k), core::str::from_utf8(v)) {
            vars.push((ks, vs));
        }
    }
    Ok((Args::from_list(ArgList::from_slices_in(&argv, alloc)), Env::from_vars(Vars::from_pairs_in(&vars, alloc))))
}

/// Accepts queued clients while a check is running and rejects them promptly with
/// `CannotServe::Busy` (or `OtherVersion`/`OtherCheckout`), or records `Shutdown`, so concurrent
/// clients fall through to standalone immediately instead of blocking in the socket backlog.
fn reject_concurrent_clients(
    cx: &Cx,
    listener: &mut Listener,
    hello: Hello,
    saw_shutdown: &AtomicBool,
    saw_replaced: &AtomicBool,
    clock: &Clock,
) {
    let mut scratch = match Frame::new(Tag::Other) {
        Ok(f) => f,
        Err(_) => return,
    };
    let mut accept_errs = 0u32;
    while !cx.cancelled() {
        let accepted = listener.accept(&cx.until(clock.after(WATCH_POLL)));
        let conn = match accepted {
            Ok(conn) => {
                accept_errs = 0;
                conn
            }
            Err(error) if matches!(error.kind, NetKind::Stopped(Stopped::Deadline { .. })) => {
                accept_errs = 0;
                continue;
            }
            Err(error) if matches!(error.kind, NetKind::Stopped(Stopped::Cancelled { .. })) => break,
            Err(error) if matches!(error.kind, NetKind::Driver(_)) && accept_errs < 16 => {
                accept_errs += 1;
                continue;
            }
            Err(_) => break,
        };
        scratch.scope(|frame| {
            let ctrl_cx = cx.until(clock.after(CTRL_WAIT));
            let mut hello_bytes = Vec::new_in(frame);
            hello.encode(&mut hello_bytes);
            if conn.send_all(&ctrl_cx, &hello_bytes).is_err() {
                let _ = conn.finish(&cx.until(clock.after(END_WAIT)));
                return;
            }
            let mut req_buf = Bytes::new_in(frame, "daemon busy client frame");
            let Ok(body) = read_daemon_frame(&ctrl_cx, &conn, &mut req_buf) else {
                let _ = conn.finish(&cx.until(clock.after(END_WAIT)));
                return;
            };
            if let Ok(client_frame) = ClientFrame::decode_in(body, frame) {
                match client_frame {
                    ClientFrame::Shutdown => {
                        saw_shutdown.store(true, Ordering::Release);
                        let mut done = Vec::new_in(frame);
                        ServerFrame::Done { status: 0 }.encode(&mut done);
                        let _ = conn.send_all(&ctrl_cx, &done);
                    }
                    ClientFrame::Check(other_req) => {
                        let why = if other_req.version != hello.version {
                            saw_replaced.store(true, Ordering::Release);
                            CannotServe::OtherVersion
                        } else if other_req.checkout != hello.checkout {
                            CannotServe::OtherCheckout
                        } else {
                            CannotServe::Busy
                        };
                        let mut refused = Vec::new_in(frame);
                        ServerFrame::Refused { why }.encode(&mut refused);
                        let _ = conn.send_all(&ctrl_cx, &refused);
                    }
                    ClientFrame::Signal(_) => {}
                }
            }
            let _ = conn.finish(&cx.until(clock.after(END_WAIT)));
        });
    }
}

/// Watches the active client's connection for `ClientFrame::Signal` or peer disconnect (EOF)
/// while a check is running, forwarding stop/status requests into `requests` so Ctrl-C or
/// client termination stops the active check cleanly.
fn watch_client_signals(
    cx: &Cx,
    conn: &Conn,
    requests: &Requests,
    saw_shutdown: &AtomicBool,
    clock: &Clock,
) {
    let scratch = match Frame::new(Tag::Other) {
        Ok(f) => f,
        Err(_) => return,
    };
    let mut decode_scratch = match Frame::new(Tag::Other) {
        Ok(f) => f,
        Err(_) => return,
    };
    let mut buf = Bytes::new_in(&scratch, "daemon signal frame");
    let mut want = 4usize;
    let mut have_len = false;
    while !cx.cancelled() {
        if !have_len && let Some(res) = daemon::frame_len(buf.as_slice()) {
            match res {
                Ok(body_len) => match 4usize.checked_add(body_len) {
                    Some(total) => {
                        want = total;
                        have_len = true;
                    }
                    None => break,
                },
                Err(_) => break,
            }
        }
        if have_len && buf.len() >= want {
            let body = buf.as_slice().get(4..want).unwrap_or(&[]);
            decode_scratch.scope(|frame| {
                if let Ok(client_frame) = ClientFrame::decode_in(body, frame) {
                    match client_frame {
                        ClientFrame::Signal(sig) => {
                            if sig == By::Interrupt.number() {
                                requests.ask_stop(By::Interrupt);
                            } else if sig == By::Terminate.number() {
                                requests.ask_stop(By::Terminate);
                            } else if sig == By::Info.number() {
                                requests.ask_status(By::Info);
                            } else if sig == By::User1.number() {
                                requests.ask_status(By::User1);
                            }
                        }
                        ClientFrame::Shutdown => {
                            saw_shutdown.store(true, Ordering::Release);
                            requests.ask_stop(By::Terminate);
                        }
                        ClientFrame::Check(_) => {}
                    }
                }
            });
            let remaining = buf.len().saturating_sub(want);
            if remaining == 0 {
                buf.clear();
            } else {
                let tail = buf.as_slice().get(want..).unwrap_or(&[]);
                let mut shifted = Vec::with_capacity_in(tail.len(), &scratch);
                shifted.extend_from_slice(tail);
                buf.clear();
                let _ = buf.try_extend(shifted.as_slice());
            }
            want = 4;
            have_len = false;
            continue;
        }
        let need = want.saturating_sub(buf.len()).max(RECV_STEP);
        let recv_cx = cx.until(clock.after(WATCH_POLL));
        match conn.recv(&recv_cx, need, &mut buf) {
            Ok(0) => {
                // Client disconnected mid-request -> treat as SIGINT stop.
                requests.ask_stop(By::Interrupt);
                break;
            }
            Ok(_) => {}
            Err(error) if matches!(error.kind, NetKind::Stopped(Stopped::Deadline { .. })) => continue,
            Err(error) if matches!(error.kind, NetKind::Stopped(Stopped::Cancelled { .. })) => break,
            Err(_) => {
                requests.ask_stop(By::Interrupt);
                break;
            }
        }
    }
}

/// Reads one length-prefixed daemon frame (4-byte LE len + CBOR body, capped at `daemon::MOST_FRAME`).
fn read_daemon_frame<'b>(cx: &Cx, conn: &Conn, buf: &'b mut Bytes<'_>) -> Result<&'b [u8], ()> {
    buf.clear();
    let mut want = 4usize;
    let mut have_len = false;
    loop {
        if !have_len && let Some(res) = daemon::frame_len(buf.as_slice()) {
            let body_len = res.map_err(|_| ())?;
            want = 4usize.checked_add(body_len).ok_or(())?;
            have_len = true;
        }
        if have_len && buf.len() >= want {
            break;
        }
        let need = want.saturating_sub(buf.len()).max(RECV_STEP);
        match conn.recv(cx, need, buf) {
            Ok(0) => return Err(()),
            Ok(_) => {}
            Err(_) => return Err(()),
        }
    }
    buf.as_slice().get(4..want).ok_or(())
}

/// Returns `true` if the checkout directory no longer exists or is no longer a directory.
fn checkout_is_gone(abs_path: &str) -> bool {
    #[cfg(unix)]
    {
        use blaze_sys::posix::io::CWD;
        use blaze_sys::posix::proc::{AT_SYMLINK_NOFOLLOW, S_IFDIR, S_IFMT, stat_at};
        let mut buf = [0u8; 4096];
        let bytes = abs_path.as_bytes();
        let Some(dst) = buf.get_mut(..bytes.len()) else {
            return true;
        };
        dst.copy_from_slice(bytes);
        let Some(nul) = buf.get_mut(bytes.len()) else {
            return true;
        };
        *nul = 0;
        let Ok(cstr) = core::ffi::CStr::from_bytes_with_nul(buf.get(..=bytes.len()).unwrap_or(&[])) else {
            return true;
        };
        match stat_at(CWD, blaze_sys::Path::new(cstr), AT_SYMLINK_NOFOLLOW) {
            Ok(st) => (st.st_mode & S_IFMT) != S_IFDIR,
            Err(_) => true,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = abs_path;
        false
    }
}

fn nanos(clock: &Clock, born: Instant) -> Nanos {
    Nanos(clock.now().since(born).map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)))
}

#[cfg(test)]
#[path = "tests/daemon_tests.rs"]
mod tests;
