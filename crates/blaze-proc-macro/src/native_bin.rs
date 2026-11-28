//! Tier-2 confined native proc-macro host (`NativeBinServer` and `NativeBinPool`).
//!
//! Runs Blaze-compiled standalone native proc-macro executables (`native.bin`, zero `rustc`, zero
//! `rust-analyzer-proc-macro-srv`, zero `dlopen`) inside a read-only OS sandbox (Landlock +
//! seccomp on Linux via `bstd::procs::hermetic::start_served_read_only` ->
//! `Spawner::start_served_confined`; Seatbelt on macOS) over inherited `CHILD_FD` (fd 3), speaking
//! the bounded binary protocol in [`crate::native_bin_proto`] and driving
//! [`crate::server::Server`] identically to the wasm64 host (`host.rs`).

#![expect(
    clippy::disallowed_types,
    reason = "a std Mutex guards the warm worker pool across expansion threads, matching hook.rs and host.rs"
)]

use core::{fmt, time::Duration};
use std::{path::Path, sync::Mutex};

use blaze_core::{
    linked::channel::{Channel, ChannelError, Message as ChanMessage, PREFIX},
    wait::{Clock, Deadline},
};
use blaze_expand::{Edition, ProcCall, ProcLimit, Produced, ReadAnswer, ReadKind, Reads};
use blaze_own::{AString, AVec, Alloc, SharedAlloc, String as OwnString, Vec as OwnVec};
use bstd::{
    io::Io,
    mem::TASK_FRAME,
    procs::{
        Input, Output, ParentDeath, RunChild as _, RunError, RunSpec, Served, Streams,
        hermetic::{Budget, Fault, Over, Stop, check_served_budget, start_served_read_only},
    },
    sync::PublishOnce,
};

use crate::{
    host::{CallError, Limits},
    module::Exported,
    native_bin_proto::{
        ChildMsg, HostMsg, MAGIC, MAX_DISPATCHES, MAX_FRAME_BYTES, PROTO_VERSION, ProtoError,
        decode_child_body, encode_host_frame, validate_hello,
    },
    sandbox::{Package, Sandbox},
    server::{Outcome, ProduceError, Server, ServerError, Tracked},
};

/// Default handshake timeout when launching a confined `native.bin` worker.
pub const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Default per-expansion wall-clock timeout.
pub const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// Default maximum warm idle workers kept per `(bin_path, bin_sha256, roots_digest)` key.
pub const DEFAULT_MAX_IDLE_PER_KEY: usize = 4;

/// Default maximum warm idle workers kept across all keys combined.
pub const DEFAULT_MAX_TOTAL_IDLE: usize = 16;

/// Default process-lifetime CPU limit (`RLIMIT_CPU`) for a persistent pooled worker.
pub const DEFAULT_WORKER_CPU_BUDGET: Duration = Duration::from_secs(3600);

/// Default process-lifetime virtual address space limit (`RLIMIT_AS`, 8 GiB) for multi-threaded
/// workers (e.g. `tokio`/`mio` running local offline tasks).
pub const DEFAULT_WORKER_MEMORY_BUDGET: u64 = 8 << 30;

/// Maximum filesystem entries snapshotted into [`Reads`] per expansion across granted read-only
/// roots before marking the expansion uncacheable (rule 18 & rule 21).
pub const MAX_GRANTED_SNAPSHOT_ENTRIES: usize = 16_384;

/// Maximum bytes streamed from any single regular file during a granted-root snapshot before
/// marking the expansion uncacheable (rule 18: bounded memory and I/O).
pub const MAX_GRANTED_SNAPSHOT_FILE_BYTES: u64 = 8 * 1024 * 1024;

/// Maximum cumulative bytes streamed across all files during a granted-root snapshot before
/// marking the expansion uncacheable (rule 18: bounded memory and I/O).
pub const MAX_GRANTED_SNAPSHOT_TOTAL_BYTES: u64 = 64 * 1024 * 1024;

/// Maximum retained capacity of `inbox`/`outbox` on an idle pooled worker; buffers that grew past
/// this threshold during a large expansion are released before returning the worker to the pool.
pub const IDLE_BUFFER_CAP_LIMIT: usize = 64 * 1024;

/// Output of one Tier-2 `native.bin` expansion, together with whether its granted-tree read
/// snapshot completed within bounds and is therefore safe to persist in the expansion cache.
pub struct NativeBinExpand<'a> {
    pub produced: Produced<'a>,
    pub cacheable: bool,
}

/// Configuration bounds for [`NativeBinServer`] and [`NativeBinPool`].
#[derive(Clone, Copy, Debug)]
pub struct NativeBinConfig {
    pub handshake_timeout: Duration,
    pub call_timeout: Duration,
    pub max_frame_bytes: u32,
    pub max_dispatches: u64,
    pub max_idle_per_key: usize,
    pub max_total_idle: usize,
    pub worker_budget: Budget,
    pub snapshot_max_entries: usize,
    pub snapshot_max_file_bytes: u64,
    pub snapshot_max_total_bytes: u64,
}

impl NativeBinConfig {
    pub const STANDARD: Self = Self {
        handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
        call_timeout: DEFAULT_CALL_TIMEOUT,
        max_frame_bytes: MAX_FRAME_BYTES,
        max_dispatches: MAX_DISPATCHES,
        max_idle_per_key: DEFAULT_MAX_IDLE_PER_KEY,
        max_total_idle: DEFAULT_MAX_TOTAL_IDLE,
        worker_budget: Budget {
            cpu: DEFAULT_WORKER_CPU_BUDGET,
            memory: DEFAULT_WORKER_MEMORY_BUDGET,
            steps: None,
        },
        snapshot_max_entries: MAX_GRANTED_SNAPSHOT_ENTRIES,
        snapshot_max_file_bytes: MAX_GRANTED_SNAPSHOT_FILE_BYTES,
        snapshot_max_total_bytes: MAX_GRANTED_SNAPSHOT_TOTAL_BYTES,
    };
}

/// Real host read-only paths corresponding to the virtual package mount, used to answer
/// `CARGO_MANIFEST_DIR` / `CARGO_MANIFEST_PATH` / `CurrentDir` for native filesystem reads while
/// still recording machine-independent virtual digests in [`Reads`] (rule 25).
#[derive(Clone, Copy, Default, Debug)]
pub struct NativePaths<'a> {
    pub manifest_dir: Option<&'a str>,
    pub workspace_root: Option<&'a str>,
}

/// Result of receiving one length-prefixed frame from a [`NativeBinTransport`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RecvFrame {
    Whole { len: usize },
    Closed,
    Deadline,
}

/// Low-level transport error from a [`NativeBinTransport`].
#[derive(Debug)]
pub enum TransportError {
    Channel(ChannelError),
    Run(RunError),
    Budget(ProcLimit, OwnString<SharedAlloc<'static>>),
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TransportError::Channel(err) => write!(f, "{err}"),
            TransportError::Run(err) => write!(f, "{err}"),
            TransportError::Budget(_, why) => f.write_str(why.as_str()),
        }
    }
}

/// Bidirectional framed IPC transport seam for [`NativeBinServer`].
pub trait NativeBinTransport {
    fn now(&self) -> Duration;

    fn send_frame(&mut self, framed: &[u8], deadline: Deadline) -> Result<(), TransportError>;

    fn recv_frame(
        &mut self,
        into: &mut OwnVec<u8, SharedAlloc<'static>>,
        max_frame: u32,
        deadline: Deadline,
    ) -> Result<RecvFrame, TransportError>;

    fn shutdown_and_reap(self);
}

/// Production transport backed by a confined [`bstd::procs::Served`] child process.
pub struct ServedTransport {
    served: Served,
    budget: Budget,
}

impl ServedTransport {
    pub fn new(served: Served, budget: Budget) -> Self {
        Self { served, budget }
    }

    fn check_budget(&self) -> Result<(), TransportError> {
        match check_served_budget(self.served.id(), self.budget) {
            Ok(_) => Ok(()),
            Err(Fault::Stopped(Stop::Budget(Over::Cpu { used, most }))) => {
                let mut msg = OwnString::new_in(SharedAlloc::global());
                let _ = fmt::Write::write_fmt(
                    &mut msg,
                    format_args!("native proc-macro exceeded CPU budget ({used:?} > {most:?})"),
                );
                Err(TransportError::Budget(ProcLimit::Fuel, msg))
            }
            Err(Fault::Stopped(Stop::Budget(Over::Memory { used, most }))) => {
                let mut msg = OwnString::new_in(SharedAlloc::global());
                let _ = fmt::Write::write_fmt(
                    &mut msg,
                    format_args!("native proc-macro exceeded memory budget ({used} > {most} bytes)"),
                );
                Err(TransportError::Budget(ProcLimit::Memory, msg))
            }
            Err(other) => {
                let mut msg = OwnString::new_in(SharedAlloc::global());
                let _ = fmt::Write::write_fmt(
                    &mut msg,
                    format_args!("native proc-macro budget check failed: {other:?}"),
                );
                Err(TransportError::Budget(ProcLimit::Fuel, msg))
            }
        }
    }
}

impl NativeBinTransport for ServedTransport {
    fn now(&self) -> Duration {
        self.served.now()
    }

    fn send_frame(&mut self, framed: &[u8], deadline: Deadline) -> Result<(), TransportError> {
        self.check_budget()?;
        self.served.send(framed, deadline).map_err(TransportError::Run)
    }

    fn recv_frame(
        &mut self,
        into: &mut OwnVec<u8, SharedAlloc<'static>>,
        max_frame: u32,
        deadline: Deadline,
    ) -> Result<RecvFrame, TransportError> {
        #[cfg(target_os = "macos")]
        {
            const METER_SLICE: Duration = Duration::from_millis(100);
            loop {
                self.check_budget()?;
                let now = self.served.now();
                if deadline.passed(now) {
                    return Ok(RecvFrame::Deadline);
                }
                let slice_end = Deadline::after(now, METER_SLICE).min(deadline);
                match self.served.recv_message(into, max_frame, slice_end) {
                    Ok(ChanMessage::Whole { len }) => {
                        self.check_budget()?;
                        return Ok(RecvFrame::Whole { len });
                    }
                    Ok(ChanMessage::Closed) => return Ok(RecvFrame::Closed),
                    Ok(ChanMessage::Deadline) => {
                        if deadline.passed(self.served.now()) {
                            return Ok(RecvFrame::Deadline);
                        }
                    }
                    Err(err) => return Err(TransportError::Run(err)),
                }
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            self.check_budget()?;
            match self.served.recv_message(into, max_frame, deadline) {
                Ok(ChanMessage::Whole { len }) => Ok(RecvFrame::Whole { len }),
                Ok(ChanMessage::Closed) => Ok(RecvFrame::Closed),
                Ok(ChanMessage::Deadline) => Ok(RecvFrame::Deadline),
                Err(err) => Err(TransportError::Run(err)),
            }
        }
    }

    fn shutdown_and_reap(self) {
        let _ = self.served.kill();
    }
}

/// In-process transport backed by a raw [`Channel`] and caller-provided [`Clock`] (used for
/// loopback socketpair tests with zero subprocesses).
pub struct ChannelTransport<C: Clock> {
    channel: Channel,
    clock: C,
}

impl<C: Clock> ChannelTransport<C> {
    pub fn new(channel: Channel, clock: C) -> Self {
        Self { channel, clock }
    }
}

impl<C: Clock> NativeBinTransport for ChannelTransport<C> {
    fn now(&self) -> Duration {
        self.clock.now()
    }

    fn send_frame(&mut self, framed: &[u8], deadline: Deadline) -> Result<(), TransportError> {
        self.channel
            .send(framed, deadline, &self.clock)
            .map_err(TransportError::Channel)
    }

    fn recv_frame(
        &mut self,
        into: &mut OwnVec<u8, SharedAlloc<'static>>,
        max_frame: u32,
        deadline: Deadline,
    ) -> Result<RecvFrame, TransportError> {
        match self.channel.recv_message(into, max_frame, deadline, &self.clock) {
            Ok(ChanMessage::Whole { len }) => Ok(RecvFrame::Whole { len }),
            Ok(ChanMessage::Closed) => Ok(RecvFrame::Closed),
            Ok(ChanMessage::Deadline) => Ok(RecvFrame::Deadline),
            Err(err) => Err(TransportError::Channel(err)),
        }
    }

    fn shutdown_and_reap(self) {
        drop(self.channel);
    }
}

fn fmt_err(args: fmt::Arguments<'_>) -> blaze_own::AString<'static> {
    let mut out = AString::new_in(Alloc::global());
    let _ = fmt::Write::write_fmt(&mut out, args);
    out
}

fn err_text(args: fmt::Arguments<'_>) -> impl Into<crate::host::CallError> {
    CallError::Failed(fmt_err(args).as_str().into())
}

fn proto_to_call_error(err: ProtoError) -> CallError {
    match err {
        ProtoError::FrameTooLarge { len, max } => CallError::Limit {
            limit: ProcLimit::Output,
            why: fmt_err(format_args!(
                "native proc-macro frame of {len} bytes exceeded limit of {max} bytes"
            ))
            .as_str()
            .into(),
        },
        other => CallError::Failed(
            fmt_err(format_args!("native proc-macro protocol error: {other}"))
                .as_str()
                .into(),
        ),
    }
}

fn server_err_to_call_error(err: ServerError) -> CallError {
    match err {
        ServerError::Limit(limit) => CallError::Limit {
            limit,
            why: fmt_err(format_args!("{err}")).as_str().into(),
        },
        ServerError::Position { .. }
        | ServerError::FileUnknown { .. }
        | ServerError::FileOfSite
        | ServerError::FileOfMadeToken => {
            CallError::Refused(fmt_err(format_args!("{err}")).as_str().into())
        }
        other => CallError::Failed(
            fmt_err(format_args!("its request was not the bridge's: {other}"))
                .as_str()
                .into(),
        ),
    }
}

fn strip_path_prefix<'a>(path: &'a str, prefix: &str) -> Option<&'a str> {
    let trimmed = prefix.trim_end_matches('/');
    if path == trimmed {
        return Some("");
    }
    let rest = path.strip_prefix(trimmed)?;
    rest.strip_prefix('/')
}

fn remap_tracked_path<'a>(
    path: &str,
    package: &Package,
    paths: NativePaths<'_>,
    alloc: Alloc<'a>,
) -> AString<'a> {
    let mut out = AString::new_in(alloc);
    if let Some(mdir) = paths.manifest_dir
        && let Some(rel) = strip_path_prefix(path, mdir)
    {
        let cwd = package.cwd();
        out.push_str(&cwd);
        if !rel.is_empty() {
            out.push('/');
            out.push_str(rel);
        }
        return out;
    }
    if let Some(wroot) = paths.workspace_root
        && let Some(rel) = strip_path_prefix(path, wroot)
    {
        out.push_str(package.files.seen().mount());
        if !rel.is_empty() {
            out.push('/');
            out.push_str(rel);
        }
        return out;
    }
    if !path.starts_with('/') {
        let cwd = package.cwd();
        out.push_str(&cwd);
        let rel = path.strip_prefix("./").unwrap_or(path);
        if !rel.is_empty() && rel != "." {
            out.push('/');
            out.push_str(rel);
        }
        return out;
    }
    out.push_str(path);
    out
}

/// Returns `true` if any produced token or diagnostic text leaks one of the host checkout paths
/// in `paths` (rule 25: machine paths must never enter cached macro outputs).
fn contains_host_path(produced: &Produced<'_>, paths: NativePaths<'_>) -> bool {
    let needles = [paths.manifest_dir, paths.workspace_root];
    for host_dir in needles.into_iter().flatten() {
        let trimmed = host_dir.trim_end_matches('/');
        if trimmed.len() <= 1 {
            continue;
        }
        let needle_bytes = trimmed.as_bytes();
        if produced
            .text
            .as_slice()
            .windows(needle_bytes.len())
            .any(|w| w == needle_bytes)
        {
            return true;
        }
        for idx in 0..produced.tokens.tokens().len() {
            if produced.tokens.text_at(idx).contains(trimmed) {
                return true;
            }
        }
    }
    false
}

/// Host driver for a single persistent confined Tier-2 native proc-macro worker over a
/// [`NativeBinTransport`].
pub struct NativeBinServer<T: NativeBinTransport> {
    transport: T,
    inbox: OwnVec<u8, SharedAlloc<'static>>,
    outbox: OwnVec<u8, SharedAlloc<'static>>,
    handshook: bool,
    config: NativeBinConfig,
}

impl<T: NativeBinTransport> NativeBinServer<T> {
    pub fn new(transport: T, config: NativeBinConfig) -> Self {
        let shared = SharedAlloc::global();
        Self {
            transport,
            inbox: OwnVec::new_in(shared),
            outbox: OwnVec::new_in(shared),
            handshook: false,
            config,
        }
    }

    /// Consumes the server and returns its underlying transport (for testing).
    pub fn into_transport(self) -> T {
        self.transport
    }

    /// Releases oversized `inbox`/`outbox` buffers before returning this worker to an idle pool so
    /// a single large expansion never pins multi-megabyte allocations on idle pooled workers.
    pub fn trim_idle_buffers(&mut self) {
        let shared = SharedAlloc::global();
        self.inbox.clear();
        if self.inbox.capacity() > IDLE_BUFFER_CAP_LIMIT {
            self.inbox = OwnVec::new_in(shared);
        }
        self.outbox.clear();
        if self.outbox.capacity() > IDLE_BUFFER_CAP_LIMIT {
            self.outbox = OwnVec::new_in(shared);
        }
    }

    #[cfg(test)]
    fn buffer_capacities(&self) -> (usize, usize) {
        (self.inbox.capacity(), self.outbox.capacity())
    }

    /// Sends a best-effort `Shutdown` frame and synchronously kills & reaps the worker.
    pub fn shutdown(mut self) {
        self.outbox.clear();
        if encode_host_frame(&HostMsg::Shutdown, &mut self.outbox, self.config.max_frame_bytes).is_ok() {
            let deadline = Deadline::after(self.transport.now(), Duration::from_millis(25));
            let _ = self.transport.send_frame(self.outbox.as_slice(), deadline);
        }
        self.transport.shutdown_and_reap();
    }

    fn send_msg(&mut self, msg: &HostMsg<'_>, deadline: Deadline) -> Result<(), CallError> {
        self.outbox.clear();
        encode_host_frame(msg, &mut self.outbox, self.config.max_frame_bytes)
            .map_err(proto_to_call_error)?;
        self.transport
            .send_frame(self.outbox.as_slice(), deadline)
            .map_err(|err| match err {
                TransportError::Budget(limit, why) => CallError::Limit { limit, why: why.as_str().into() },
                other => err_text(format_args!(
                    "failed sending frame to native proc-macro worker: {other}"
                ))
                .into(),
            })
    }

    fn recv_next_body<'a>(
        &mut self,
        deadline: Deadline,
        body_out: &mut OwnVec<u8, Alloc<'a>>,
    ) -> Result<(), CallError> {
        let frame_len = match self
            .transport
            .recv_frame(&mut self.inbox, self.config.max_frame_bytes, deadline)
        {
            Ok(RecvFrame::Whole { len }) => len,
            Ok(RecvFrame::Closed) => {
                return Err(err_text(format_args!(
                    "native proc-macro worker closed channel before replying"
                ))
                .into());
            }
            Ok(RecvFrame::Deadline) => {
                return Err(CallError::Limit {
                    limit: ProcLimit::Fuel,
                    why: fmt_err(format_args!("native proc-macro worker timed out"))
                        .as_str()
                        .into(),
                });
            }
            Err(TransportError::Budget(limit, why)) => {
                return Err(CallError::Limit { limit, why: why.as_str().into() });
            }
            Err(TransportError::Channel(ChannelError::TooLong { len, most })) => {
                return Err(CallError::Limit {
                    limit: ProcLimit::Output,
                    why: fmt_err(format_args!(
                        "native proc-macro frame of {len} bytes exceeded limit of {most} bytes"
                    ))
                    .as_str()
                    .into(),
                });
            }
            Err(err) => {
                return Err(err_text(format_args!("native proc-macro channel error: {err}")).into());
            }
        };
        let Some(body) = self.inbox.get(PREFIX..frame_len) else {
            return Err(err_text(format_args!(
                "native proc-macro worker returned malformed frame length {frame_len}"
            ))
            .into());
        };
        body_out.clear();
        body_out.try_reserve(body.len()).map_err(|_| {
            err_text(format_args!("out of memory buffering native proc-macro frame")).into()
        })?;
        body_out.extend_from_slice(body);
        self.inbox.drain(..frame_len);
        Ok(())
    }

    /// Performs the versioned `Hello` / `HelloAck` handshake with the worker.
    pub fn handshake(&mut self, alloc: Alloc<'_>) -> Result<(), CallError> {
        if self.handshook {
            return Ok(());
        }
        let deadline = Deadline::after(self.transport.now(), self.config.handshake_timeout);
        self.send_msg(&HostMsg::Hello { magic: MAGIC, version: PROTO_VERSION }, deadline)?;
        let mut body = OwnVec::new_in(alloc);
        self.recv_next_body(deadline, &mut body)?;
        let msg = decode_child_body(body.as_slice()).map_err(proto_to_call_error)?;
        match msg {
            ChildMsg::HelloAck { magic, version } => {
                validate_hello(magic, version).map_err(proto_to_call_error)?;
                self.handshook = true;
                Ok(())
            }
            other => Err(err_text(format_args!(
                "expected HelloAck from native proc-macro worker, got {other:?}"
            ))
            .into()),
        }
    }

    /// Runs one proc-macro expansion against this worker, driving [`Server`] and [`Sandbox`]
    /// identically to `host.rs` and returning [`NativeBinExpand`] with full `TokenOrigin` hygiene,
    /// `ProcDiagnostic`, complete granted-tree filesystem read tracking, and host-path leak checks.
    #[expect(clippy::too_many_arguments, reason = "explicit host inputs, sandbox, limits, paths, granted roots, and allocators (rule 75)")]
    pub fn expand<'a>(
        &mut self,
        export: &Exported,
        call: &ProcCall<'_>,
        edition: Edition,
        mut sandbox: Sandbox,
        limits: Limits,
        paths: NativePaths<'_>,
        granted_rel_roots: &[&Path],
        scratch: Alloc<'static>,
        alloc: Alloc<'a>,
    ) -> Result<NativeBinExpand<'a>, CallError> {
        if !self.handshook {
            self.handshake(alloc)?;
        }
        let mut run_input = Default::default();
        let mut server = Server::new(edition, limits.server, call, scratch, &mut run_input)
            .map_err(server_err_to_call_error)?;
        let deadline = Deadline::after(self.transport.now(), self.config.call_timeout);

        // Snapshot all granted read-only roots BEFORE running the worker (bounded by entry count,
        // per-file bytes, and total bytes). After the worker returns, we snapshot again and only
        // cache when BOTH snapshots succeed and are identical — preventing a mid-expansion file
        // edit from binding post-edit digests to pre-edit generated tokens (rule 21).
        let pre_snapshot = if granted_rel_roots.is_empty() {
            Some(OwnVec::new_in(SharedAlloc::global()))
        } else {
            sandbox.snapshot_granted_roots(
                granted_rel_roots,
                self.config.snapshot_max_entries,
                self.config.snapshot_max_file_bytes,
                self.config.snapshot_max_total_bytes,
            )
        };

        self.send_msg(
            &HostMsg::Expand { index: export.index, kind: export.kind, input: &run_input },
            deadline,
        )?;

        let mut body = OwnVec::new_in(alloc);
        let mut reply = Default::default();
        let mut dispatches: u64 = 0;

        let outcome = loop {
            self.recv_next_body(deadline, &mut body)?;
            let msg = decode_child_body(body.as_slice()).map_err(proto_to_call_error)?;
            match msg {
                ChildMsg::Dispatch { bytes } => {
                    dispatches = dispatches.saturating_add(1);
                    if dispatches > self.config.max_dispatches {
                        return Err(CallError::Limit {
                            limit: ProcLimit::Fuel,
                            why: fmt_err(format_args!(
                                "native proc-macro exceeded {} bridge dispatch limit",
                                self.config.max_dispatches
                            ))
                            .as_str()
                            .into(),
                        });
                    }
                    server
                        .dispatch(bytes, &mut reply)
                        .map_err(server_err_to_call_error)?;
                    let sent = self.send_msg(&HostMsg::Reply { bytes: &reply }, deadline);
                    reply.clear();
                    sent?;
                }
                ChildMsg::EnvVar { name } => {
                    let recorded = sandbox.var(name).map_err(CallError::EnvUnknown)?;
                    // Never leak machine checkout paths into the worker environment (rule 25):
                    // the worker's OS cwd IS `manifest_dir`, so `"."` and `"./Cargo.toml"` resolve
                    // every relative file read inside the sandbox identically across machines.
                    let answer: Option<&str> = match (name, paths.manifest_dir) {
                        ("CARGO_MANIFEST_DIR", Some(_)) => Some("."),
                        ("CARGO_MANIFEST_PATH", Some(_)) => Some("./Cargo.toml"),
                        _ => recorded.as_deref(),
                    };
                    self.send_msg(&HostMsg::EnvVal { value: answer }, deadline)?;
                }
                ChildMsg::CurrentDir => {
                    let virtual_cwd;
                    let dir = match paths.manifest_dir {
                        Some(_) => ".",
                        None => {
                            virtual_cwd = sandbox.package().cwd();
                            virtual_cwd.as_str()
                        }
                    };
                    self.send_msg(&HostMsg::CurrentDirVal { dir }, deadline)?;
                }
                ChildMsg::Output { bytes } => {
                    if bytes.len() > limits.output {
                        return Err(CallError::Limit {
                            limit: ProcLimit::Output,
                            why: fmt_err(format_args!(
                                "native proc-macro output of {} bytes exceeded limit of {} bytes",
                                bytes.len(),
                                limits.output
                            ))
                            .as_str()
                            .into(),
                        });
                    }
                    let outcome = server.outcome(bytes).map_err(|err| {
                        err_text(format_args!("its output is not the bridge's: {err}")).into()
                    })?;
                    break outcome;
                }
                ChildMsg::Panic { message } => {
                    break Outcome::Panicked(message.into());
                }
                ChildMsg::HelloAck { .. } => {
                    return Err(err_text(format_args!(
                        "unexpected HelloAck from native proc-macro worker during expansion"
                    ))
                    .into());
                }
            }
        };

        for tracked in core::mem::take(&mut server.tracked) {
            match tracked {
                Tracked::Env(name) => {
                    sandbox.var(&name).map_err(CallError::EnvUnknown)?;
                }
                Tracked::Path(path) => {
                    let remapped = remap_tracked_path(&path, sandbox.package(), paths, alloc);
                    if sandbox.track_path(remapped.as_str()).is_err()
                        && let Some((limit, why)) = sandbox.refused.take()
                    {
                        return Err(CallError::Limit { limit, why });
                    }
                }
            }
        }

        // Re-snapshot every granted read-only root AFTER the worker finishes and require an exact
        // match with `pre_snapshot` before attaching the reads and marking the expansion cacheable
        // (rule 21: any mid-expansion edit, add, delete, or symlink retarget forces `cacheable = false`).
        let cacheable = if let Some(pre) = pre_snapshot {
            let post_snapshot = if granted_rel_roots.is_empty() {
                Some(OwnVec::new_in(SharedAlloc::global()))
            } else {
                sandbox.snapshot_granted_roots(
                    granted_rel_roots,
                    self.config.snapshot_max_entries,
                    self.config.snapshot_max_file_bytes,
                    self.config.snapshot_max_total_bytes,
                )
            };
            if post_snapshot.as_ref() == Some(&pre) {
                sandbox.record_reads(pre);
                true
            } else {
                false
            }
        } else {
            false
        };

        let mut reads = Reads::new_in(alloc);
        for read in &sandbox.reads {
            let answer = read.digest.map_or(ReadAnswer::Absent, ReadAnswer::Digest);
            reads
                .push(ReadKind::ProcMacro, &read.subject, answer)
                .map_err(|err| err_text(format_args!("its reads do not fit: {err:?}")).into())?;
        }
        if let (true, Ok(file)) = (server.asked_file, call.file) {
            let answer = ReadAnswer::Digest(crate::sandbox::call_file_digest(file));
            reads
                .push(ReadKind::ProcMacro, crate::sandbox::CALL_FILE, answer)
                .map_err(|err| err_text(format_args!("its reads do not fit: {err:?}")).into())?;
        }

        let (handle, panicked) = match &outcome {
            Outcome::Tokens(handle) => (*handle, None),
            Outcome::Panicked(message) => (None, Some(message.as_str())),
        };
        let produced = server.produce(handle, panicked, reads, alloc).map_err(|err| match err {
            ProduceError::Server(ServerError::Limit(limit)) => CallError::Limit {
                limit,
                why: fmt_err(format_args!("{err:?}")).as_str().into(),
            },
            other => err_text(format_args!("its output does not make tokens: {other:?}")).into(),
        })?;
        if contains_host_path(&produced, paths) {
            return Err(CallError::Refused(
                fmt_err(format_args!(
                    "native proc-macro output leaked host checkout path (rule 25)"
                ))
                .as_str()
                .into(),
            ));
        }
        Ok(NativeBinExpand { produced, cacheable })
    }
}

/// Planted bug switches for deterministic simulation tests of [`PoolCore`] (rule 43).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PoolPlant {
    None,
    /// Control 1: ignores `max_idle_per_key` and `max_total_idle` bounds.
    DropCapEviction,
    /// Control 2: returns unhealthy/errored workers back into the idle pool.
    KeepErroredWorker,
}

/// Sans-IO coordinator for bounded idle worker pooling across keys.
pub struct PoolCore<K, W> {
    max_idle_per_key: usize,
    max_total_idle: usize,
    idle: OwnVec<(K, W), SharedAlloc<'static>>,
    plant: PoolPlant,
}

impl<K: PartialEq, W> PoolCore<K, W> {
    pub fn new(max_idle_per_key: usize, max_total_idle: usize, plant: PoolPlant) -> Self {
        Self {
            max_idle_per_key: max_idle_per_key.max(1),
            max_total_idle: max_total_idle.max(1),
            idle: OwnVec::new_in(SharedAlloc::global()),
            plant,
        }
    }

    pub fn idle_len(&self) -> usize {
        self.idle.len()
    }

    pub fn idle_for_key(&self, key: &K) -> usize {
        self.idle.iter().filter(|(k, _)| k == key).count()
    }

    /// Acquires the most recently used idle worker matching `key`, if one is warm.
    pub fn acquire(&mut self, key: &K) -> Option<W> {
        let pos = self.idle.iter().rposition(|(k, _)| k == key)?;
        let (_, worker) = self.idle.remove_shift(pos);
        Some(worker)
    }

    /// Releases `worker` back to the pool. Returns `Some(evicted)` when either `worker` itself is
    /// unhealthy or pool caps require evicting an older idle worker — the caller MUST synchronously
    /// shut down and reap any returned worker outside the pool lock.
    pub fn release(&mut self, key: K, worker: W, healthy: bool) -> Option<W> {
        if !healthy && self.plant != PoolPlant::KeepErroredWorker {
            return Some(worker);
        }
        if self.plant != PoolPlant::DropCapEviction {
            if self.idle_for_key(&key) >= self.max_idle_per_key
                && let Some(pos) = self.idle.iter().position(|(k, _)| k == &key)
            {
                let (_, evicted) = self.idle.remove_shift(pos);
                self.idle.push((key, worker));
                return Some(evicted);
            }
            if self.idle.len() >= self.max_total_idle && !self.idle.is_empty() {
                let (_, evicted) = self.idle.remove_shift(0);
                self.idle.push((key, worker));
                return Some(evicted);
            }
        }
        self.idle.push((key, worker));
        None
    }

    /// Drains all idle workers for shutdown and synchronous reaping.
    pub fn drain_all(&mut self) -> OwnVec<W, SharedAlloc<'static>> {
        let mut out = OwnVec::with_capacity_in(self.idle.len(), SharedAlloc::global());
        for (_, worker) in self.idle.drain(..) {
            out.push(worker);
        }
        out
    }
}

/// Deterministic SHA-256 digest of a canonically sorted, deduplicated set of read-only confinement
/// roots.
pub fn confined_roots_digest(sorted_unique_roots: &[&str]) -> [u8; 32] {
    let mut hasher = blaze_sha256::Sha256::new();
    hasher.update(b"blaze-native-bin-roots-v1\0");
    for root in sorted_unique_roots {
        hasher.update(root.as_bytes());
        hasher.update(b"\0");
    }
    hasher.finish()
}

#[derive(PartialEq, Eq, Debug)]
struct WorkerKey {
    bin_path: OwnString<SharedAlloc<'static>>,
    bin_sha256: OwnString<SharedAlloc<'static>>,
    cwd: OwnString<SharedAlloc<'static>>,
    roots_digest: [u8; 32],
}

impl WorkerKey {
    fn new(bin_path: &str, bin_sha256: &str, cwd: &str, roots_digest: [u8; 32]) -> Self {
        let shared = SharedAlloc::global();
        Self {
            bin_path: OwnString::from_str_in(bin_path, shared),
            bin_sha256: OwnString::from_str_in(bin_sha256, shared),
            cwd: OwnString::from_str_in(cwd, shared),
            roots_digest,
        }
    }
}

/// Shared pool of warm confined Tier-2 `native.bin` proc-macro workers keyed by
/// `(bin_path, bin_sha256, confined_roots_digest)`.
///
/// Spawning and RPC expansion run outside the pool mutex so concurrent expansions never serialize
/// on each other. Every evicted, errored, or dropped worker is synchronously killed and reaped so
/// zero zombie processes leak.
pub struct NativeBinPool {
    config: NativeBinConfig,
    world: PublishOnce<Io>,
    core: Mutex<PoolCore<WorkerKey, NativeBinServer<ServedTransport>>>,
}

impl NativeBinPool {
    pub fn new(config: NativeBinConfig) -> Self {
        Self {
            config,
            world: PublishOnce::new(),
            core: Mutex::new(PoolCore::new(
                config.max_idle_per_key,
                config.max_total_idle,
                PoolPlant::None,
            )),
        }
    }

    pub fn standard() -> Self {
        Self::new(NativeBinConfig::STANDARD)
    }

    fn world(&self) -> Result<&Io, CallError> {
        if let Some(io) = self.world.get() {
            return Ok(io);
        }
        let (io, _) = crate::record::native_world(Path::new("/")).map_err(|err| {
            err_text(format_args!(
                "failed to open native process world for Tier-2 proc macro: {err:?}"
            ))
            .into()
        })?;
        drop(self.world.set(io));
        self.world.get().ok_or_else(|| {
            err_text(format_args!("native process world cell empty after publish")).into()
        })
    }

    fn spawn_worker(
        &self,
        bin_path: &str,
        sorted_roots: &[&str],
        cwd: &str,
        alloc: Alloc<'_>,
    ) -> Result<NativeBinServer<ServedTransport>, CallError> {
        let io = self.world()?;
        let host = io.need_host().map_err(|err| {
            err_text(format_args!(
                "native host capability unavailable for Tier-2 proc macro: {err}"
            ))
            .into()
        })?;
        let spawner = host.spawner().map_err(|err| {
            err_text(format_args!(
                "process spawner unavailable for Tier-2 proc macro: {err}"
            ))
            .into()
        })?;
        // Use the worker's full process lifetime budget on `Cx` because `bstd::procs::Served`
        // caps every subsequent channel op by the `Cx` deadline; individual handshake and expand
        // RPCs enforce their own tighter per-call deadlines.
        let cx = io.cx(self.config.worker_budget.cpu, TASK_FRAME).map_err(|err| {
            err_text(format_args!(
                "failed to open task context for Tier-2 proc macro: {err}"
            ))
            .into()
        })?;
        let spec = RunSpec {
            program: bin_path,
            arg0: None,
            args: &[],
            env: &[],
            dir: cwd,
            streams: Streams {
                stdin: Input::Null,
                stdout: Output::Null,
                stderr: Output::Null,
            },
            own_group: false,
            // Pooled workers are shared across threads; `ParentDeath::Channel` exits cleanly when
            // `CHILD_FD` closes (parent exit or drop) without `PR_SET_PDEATHSIG` killing a worker
            // when a rayon thread exits.
            parent_death: ParentDeath::Channel,
        };
        let started = start_served_read_only(
            &spawner,
            &cx,
            &spec,
            sorted_roots,
            self.config.worker_budget,
            alloc,
        )
        .map_err(|fault| {
            err_text(format_args!(
                "failed to start confined Tier-2 proc-macro worker `{bin_path}`: {fault:?}"
            ))
            .into()
        })?;
        let mut server = NativeBinServer::new(
            ServedTransport::new(started.child, self.config.worker_budget),
            self.config,
        );
        if let Err(err) = server.handshake(alloc) {
            server.shutdown();
            return Err(err);
        }
        Ok(server)
    }

    /// Expands `export` using a warm confined worker matching `(bin_path, bin_sha256, cwd, read_roots)`,
    /// spawning one lazily on cache miss and evicting/reaping it cleanly on error.
    #[expect(clippy::too_many_arguments, reason = "explicit binary, roots, call, sandbox, limits, paths, granted roots, and allocators")]
    pub fn expand<'a>(
        &self,
        bin_path: &Path,
        bin_sha256: &str,
        read_roots: &[&str],
        granted_rel_roots: &[&Path],
        export: &Exported,
        call: &ProcCall<'_>,
        edition: Edition,
        sandbox: Sandbox,
        limits: Limits,
        paths: NativePaths<'_>,
        scratch: Alloc<'static>,
        alloc: Alloc<'a>,
    ) -> Result<NativeBinExpand<'a>, CallError> {
        let Some(bin_str) = bin_path.to_str() else {
            return Err(err_text(format_args!(
                "native proc-macro binary path is not valid UTF-8"
            ))
            .into());
        };
        let mut sorted_roots: AVec<'_, &str> = AVec::new_in(alloc);
        for &root in read_roots {
            if !root.is_empty() {
                sorted_roots.push(root);
            }
        }
        sorted_roots.sort_unstable();
        sorted_roots.dedup();
        let cwd = paths
            .manifest_dir
            .or(paths.workspace_root)
            .or_else(|| sorted_roots.first().copied())
            .unwrap_or("/");
        let roots_digest = confined_roots_digest(sorted_roots.as_slice());
        let key = WorkerKey::new(bin_str, bin_sha256, cwd, roots_digest);

        let maybe_worker = crate::host::lock(&self.core).acquire(&key);
        let mut worker = match maybe_worker {
            Some(w) => w,
            None => self.spawn_worker(bin_str, sorted_roots.as_slice(), cwd, alloc)?,
        };

        let result = worker.expand(
            export,
            call,
            edition,
            sandbox,
            limits,
            paths,
            granted_rel_roots,
            scratch,
            alloc,
        );
        match result {
            Ok(produced) => {
                worker.trim_idle_buffers();
                let evicted = crate::host::lock(&self.core).release(key, worker, true);
                if let Some(old) = evicted {
                    old.shutdown();
                }
                Ok(produced)
            }
            Err(err) => {
                worker.shutdown();
                Err(err)
            }
        }
    }
}

impl Drop for NativeBinPool {
    fn drop(&mut self) {
        let workers = crate::host::lock(&self.core).drain_all();
        for worker in workers {
            worker.shutdown();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read as _, Write as _};
    use std::os::fd::OwnedFd;
    use std::sync::Arc;
    use std::thread;

    use blaze_core::linked::channel;
    use blaze_expand::{Edition, ProcCall, ProcDiagnostic, ProcKind, ReadAnswer, ReadKind, lex};

    use super::*;
    use crate::{
        native_bin_proto::{
            FRAME_PREFIX_LEN, decode_host_body, encode_child_frame, split_frame,
        },
        package_env::PackageEnv,
        protocol::Method,
        sandbox::Files,
        wire::{Reader as WireReader, Writer as WireWriter},
    };

    struct TestClock(std::time::Instant);

    impl TestClock {
        fn new() -> Self {
            Self(std::time::Instant::now())
        }
    }

    impl Clock for TestClock {
        fn now(&self) -> Duration {
            self.0.elapsed()
        }
    }

    fn read_host_frame(fd: &mut std::fs::File, buf: &mut Vec<u8>) -> Option<Vec<u8>> {
        let mut chunk = [0u8; 4096];
        loop {
            if let Ok(Some((body, total))) = split_frame(buf, MAX_FRAME_BYTES) {
                let out = body.to_vec();
                buf.drain(..total);
                return Some(out);
            }
            match fd.read(&mut chunk) {
                Ok(0) => return None,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return None,
            }
        }
    }

    fn try_write_child_msg(fd: &mut std::fs::File, msg: &ChildMsg<'_>) -> bool {
        let heap = blaze_own::testing::heap();
        let mut out = OwnVec::new_in(heap.alloc());
        encode_child_frame(msg, &mut out, MAX_FRAME_BYTES).expect("encode child");
        fd.write_all(out.as_slice()).is_ok()
    }

    fn write_child_msg(fd: &mut std::fs::File, msg: &ChildMsg<'_>) {
        assert!(try_write_child_msg(fd, msg), "write child");
    }

    fn make_test_package(env_vars: &[(&str, &str)], files: &[(&str, &'static [u8])]) -> Arc<Package> {
        let env = PackageEnv::set_only(
            env_vars
                .iter()
                .map(|&(k, v)| (k.to_owned(), v.to_owned()))
                .collect(),
        )
        .expect("PackageEnv");
        let listed: Vec<(String, crate::sandbox::ListedBytes)> = files
            .iter()
            .map(|&(p, b)| (p.to_owned(), crate::sandbox::ListedBytes::Static(b)))
            .collect();
        Arc::new(Package::new(Files::Listed(Arc::from(listed)), env).expect("Package"))
    }

    fn run_loopback_peer(
        peer_fn: impl FnOnce(std::fs::File) + Send + 'static,
        config: NativeBinConfig,
    ) -> (NativeBinServer<ChannelTransport<TestClock>>, thread::JoinHandle<()>) {
        let (parent, child_end) = channel::pair().expect("socketpair");
        let child_fd: OwnedFd = child_end.into_fd();
        let child_file = std::fs::File::from(child_fd);
        let handle = thread::spawn(move || peer_fn(child_file));
        let server = NativeBinServer::new(ChannelTransport::new(parent, TestClock::new()), config);
        (server, handle)
    }

    #[test]
    fn loopback_full_expansion_env_cwd_tracking_and_panic_reuse() {
        let heap = blaze_own::testing::heap();
        let alloc = heap.alloc();

        let (mut server, handle) = run_loopback_peer(
            |mut fd| {
                let mut buf = Vec::new();
                // 1. Handshake
                let body = read_host_frame(&mut fd, &mut buf).expect("hello");
                assert_eq!(
                    decode_host_body(&body),
                    Ok(HostMsg::Hello { magic: MAGIC, version: PROTO_VERSION })
                );
                write_child_msg(&mut fd, &ChildMsg::HelloAck { magic: MAGIC, version: PROTO_VERSION });

                // 2. First Expand: reads env, cwd, tracks env & path, calls TsFromStr, returns Output
                let body = read_host_frame(&mut fd, &mut buf).expect("expand 1");
                let HostMsg::Expand { index, kind, input } = decode_host_body(&body).expect("decode expand 1") else {
                    panic!("expected Expand");
                };
                assert_eq!(index, 3);
                assert_eq!(kind, ProcKind::FnLike);
                assert!(input.len() >= 16);

                // Query CARGO_MANIFEST_DIR -> receives relative "." (zero host path leak!)
                write_child_msg(&mut fd, &ChildMsg::EnvVar { name: "CARGO_MANIFEST_DIR" });
                let b = read_host_frame(&mut fd, &mut buf).expect("env val");
                assert_eq!(
                    decode_host_body(&b),
                    Ok(HostMsg::EnvVal { value: Some(".") })
                );

                // Query CARGO_MANIFEST_PATH -> receives relative "./Cargo.toml" (zero host path leak!)
                write_child_msg(&mut fd, &ChildMsg::EnvVar { name: "CARGO_MANIFEST_PATH" });
                let b = read_host_frame(&mut fd, &mut buf).expect("env manifest path");
                assert_eq!(
                    decode_host_body(&b),
                    Ok(HostMsg::EnvVal { value: Some("./Cargo.toml") })
                );

                // Query SQLX_OFFLINE -> receives Some("true")
                write_child_msg(&mut fd, &ChildMsg::EnvVar { name: "SQLX_OFFLINE" });
                let b = read_host_frame(&mut fd, &mut buf).expect("env val 2");
                assert_eq!(decode_host_body(&b), Ok(HostMsg::EnvVal { value: Some("true") }));

                // Query CurrentDir -> receives relative "." (zero host path leak!)
                write_child_msg(&mut fd, &ChildMsg::CurrentDir);
                let b = read_host_frame(&mut fd, &mut buf).expect("cwd val");
                assert_eq!(
                    decode_host_body(&b),
                    Ok(HostMsg::CurrentDirVal { dir: "." })
                );

                // Bridge RPC: TrackPath using relative "./.sqlx/q.json" -> remaps to virtual `/package/.sqlx/q.json`
                let mut track_req = vec![Method::TrackPath as u8];
                WireWriter { out: &mut track_req }.str("./.sqlx/q.json");
                write_child_msg(&mut fd, &ChildMsg::Dispatch { bytes: &track_req });
                let _ = read_host_frame(&mut fd, &mut buf).expect("track_path reply");

                // Bridge RPC: TsFromStr("const ANSWER: u32 = 42;")
                let mut ts_req = vec![Method::TsFromStr as u8];
                WireWriter { out: &mut ts_req }.str("const ANSWER: u32 = 42;");
                write_child_msg(&mut fd, &ChildMsg::Dispatch { bytes: &ts_req });
                let rep = read_host_frame(&mut fd, &mut buf).expect("ts_from_str reply");
                let HostMsg::Reply { bytes: rep_bytes } = decode_host_body(&rep).expect("reply") else {
                    panic!("expected Reply");
                };
                let mut wr = WireReader::new(rep_bytes);
                assert_eq!(wr.u8().expect("ok"), 0);
                assert!(wr.some().expect("some"));
                let out_handle = wr.handle().expect("handle");

                // Encode Outcome::Tokens(Some(out_handle)): Ok(0), Some(0), handle
                let mut out_bytes = Vec::new();
                let mut ww = WireWriter { out: &mut out_bytes };
                ww.ok(true);
                ww.some(true);
                ww.handle(out_handle);
                write_child_msg(&mut fd, &ChildMsg::Output { bytes: &out_bytes });

                // 3. Second Expand on SAME persistent worker: returns Panic, keeping worker alive!
                let body = read_host_frame(&mut fd, &mut buf).expect("expand 2");
                assert!(matches!(decode_host_body(&body), Ok(HostMsg::Expand { index: 3, .. })));
                write_child_msg(&mut fd, &ChildMsg::Panic { message: "boom inside sqlx" });

                // 4. Third Expand on SAME persistent worker after panic: succeeds cleanly!
                let body = read_host_frame(&mut fd, &mut buf).expect("expand 3");
                assert!(matches!(decode_host_body(&body), Ok(HostMsg::Expand { index: 3, .. })));
                let mut empty_out = Vec::new();
                let mut ww = WireWriter { out: &mut empty_out };
                ww.ok(true);
                ww.some(false);
                write_child_msg(&mut fd, &ChildMsg::Output { bytes: &empty_out });

                // 5. Shutdown
                let body = read_host_frame(&mut fd, &mut buf).expect("shutdown");
                assert_eq!(decode_host_body(&body), Ok(HostMsg::Shutdown));
            },
            NativeBinConfig::STANDARD,
        );

        let pkg = make_test_package(
            &[("SQLX_OFFLINE", "true")],
            &[(".sqlx/q.json", b"{\"query\":\"SELECT 1\"}")],
        );
        let export = Exported {
            index: 3,
            kind: ProcKind::FnLike,
            name: "query".to_owned(),
            helpers: Vec::new(),
        };
        let input = lex("SELECT 1", Edition::E2021, alloc).expect("lex");
        let call = ProcCall { kind: ProcKind::FnLike, input: &input, attr_args: None, file: Ok("src/lib.rs") };
        let paths = NativePaths { manifest_dir: Some("/real/host/pkg"), workspace_root: None };

        // Expansion 1: verifies tokens + tracked reads (with virtual machine-independent paths!)
        let root_slice = [Path::new("")];
        let exp1 = server
            .expand(&export, &call, Edition::E2021, Sandbox::new(Arc::clone(&pkg)), Limits::STANDARD, paths, &root_slice, heap.alloc(), alloc)
            .expect("expand 1");
        assert!(exp1.cacheable);
        let prod1 = exp1.produced;
        assert!(prod1.diagnostics.is_empty());
        assert_eq!(prod1.tokens.tokens().len(), 7); // const ANSWER : u32 = 42 ;
        let mut saw_env_sqlx = false;
        let mut saw_env_manifest = false;
        let mut saw_file_q = false;
        for entry in prod1.reads.list() {
            let subject = prod1.reads.subject(entry);
            assert_eq!(entry.kind, ReadKind::ProcMacro);
            assert!(!subject.contains("/real/host"), "host path must never leak into Reads: {subject}");
            if subject == "env:SQLX_OFFLINE" {
                saw_env_sqlx = true;
            } else if subject == "env:CARGO_MANIFEST_DIR" {
                saw_env_manifest = true;
            } else if subject == "file:.sqlx/q.json" {
                assert!(matches!(entry.answer, ReadAnswer::Digest(_)));
                saw_file_q = true;
            }
        }
        assert!(saw_env_sqlx && saw_env_manifest && saw_file_q, "expected env + tracked file reads");

        // Expansion 2: macro panic becomes ProcDiagnostic, worker stays alive!
        let prod2 = server
            .expand(&export, &call, Edition::E2021, Sandbox::new(Arc::clone(&pkg)), Limits::STANDARD, paths, &root_slice, heap.alloc(), alloc)
            .expect("expand 2 panic")
            .produced;
        assert_eq!(prod2.diagnostics.len(), 1);
        assert!(matches!(prod2.diagnostics[0], ProcDiagnostic::Panicked { .. }));
        assert!(
            core::str::from_utf8(prod2.text.as_slice())
                .expect("utf8 diag text")
                .contains("boom inside sqlx")
        );

        // Expansion 3: worker reused cleanly after panic!
        let prod3 = server
            .expand(&export, &call, Edition::E2021, Sandbox::new(pkg), Limits::STANDARD, paths, &root_slice, heap.alloc(), alloc)
            .expect("expand 3 after panic")
            .produced;
        assert!(prod3.diagnostics.is_empty());
        assert!(prod3.tokens.tokens().is_empty());

        server.shutdown();
        handle.join().expect("peer thread");
    }

    #[test]
    fn loopback_rejects_bad_handshake_eof_unknown_env_and_dispatch_cap() {
        let heap = blaze_own::testing::heap();
        let alloc = heap.alloc();
        let pkg = make_test_package(&[], &[]);
        let export = Exported { index: 0, kind: ProcKind::FnLike, name: "m".to_owned(), helpers: Vec::new() };
        let input = lex("", Edition::E2021, alloc).expect("lex");
        let call = ProcCall { kind: ProcKind::FnLike, input: &input, attr_args: None, file: Ok("src/lib.rs") };

        // 1. Bad handshake version
        let (mut srv, h) = run_loopback_peer(
            |mut fd| {
                let mut buf = Vec::new();
                let _ = read_host_frame(&mut fd, &mut buf);
                write_child_msg(&mut fd, &ChildMsg::HelloAck { magic: MAGIC, version: 999 });
            },
            NativeBinConfig::STANDARD,
        );
        let err = srv.handshake(alloc).expect_err("version mismatch");
        assert!(matches!(err, CallError::Failed(ref msg) if msg.contains("version mismatch")));
        srv.shutdown();
        h.join().expect("join 1");

        // 2. Child exits / EOF mid-expansion
        let (mut srv, h) = run_loopback_peer(
            |mut fd| {
                let mut buf = Vec::new();
                let _ = read_host_frame(&mut fd, &mut buf);
                write_child_msg(&mut fd, &ChildMsg::HelloAck { magic: MAGIC, version: PROTO_VERSION });
                let _ = read_host_frame(&mut fd, &mut buf);
                drop(fd);
            },
            NativeBinConfig::STANDARD,
        );
        let err = match srv.expand(
            &export,
            &call,
            Edition::E2021,
            Sandbox::new(Arc::clone(&pkg)),
            Limits::STANDARD,
            NativePaths::default(),
            &[],
            heap.alloc(),
            alloc,
        ) {
            Ok(_) => panic!("eof must fail"),
            Err(e) => e,
        };
        assert!(matches!(err, CallError::Failed(ref msg) if msg.contains("closed channel")));
        srv.shutdown();
        h.join().expect("join 2");

        // 3. Unknown env var refused with CallError::EnvUnknown
        let unknown_env = PackageEnv::new(
            Vec::new(),
            vec!["UNKNOWN_SECRET_VAR".to_owned()],
            Vec::new(),
        )
        .expect("env");
        let unknown_pkg = Arc::new(Package::new(Files::Listed(Arc::from([])), unknown_env).expect("pkg"));
        let (mut srv, h) = run_loopback_peer(
            |mut fd| {
                let mut buf = Vec::new();
                let _ = read_host_frame(&mut fd, &mut buf);
                write_child_msg(&mut fd, &ChildMsg::HelloAck { magic: MAGIC, version: PROTO_VERSION });
                let _ = read_host_frame(&mut fd, &mut buf);
                write_child_msg(&mut fd, &ChildMsg::EnvVar { name: "UNKNOWN_SECRET_VAR" });
                let _ = read_host_frame(&mut fd, &mut buf);
            },
            NativeBinConfig::STANDARD,
        );
        let err = match srv.expand(
            &export,
            &call,
            Edition::E2021,
            Sandbox::new(unknown_pkg),
            Limits::STANDARD,
            NativePaths::default(),
            &[],
            heap.alloc(),
            alloc,
        ) {
            Ok(_) => panic!("unknown env must refuse"),
            Err(e) => e,
        };
        assert!(matches!(err, CallError::EnvUnknown(ref e) if e.name == "UNKNOWN_SECRET_VAR"));
        srv.shutdown();
        h.join().expect("join 3");

        // 4. Bounded dispatch cap stops runaway child
        let mut tight = NativeBinConfig::STANDARD;
        tight.max_dispatches = 2;
        let (mut srv, h) = run_loopback_peer(
            |mut fd| {
                let mut buf = Vec::new();
                let _ = read_host_frame(&mut fd, &mut buf);
                write_child_msg(&mut fd, &ChildMsg::HelloAck { magic: MAGIC, version: PROTO_VERSION });
                let _ = read_host_frame(&mut fd, &mut buf);
                for _ in 0..4 {
                    let mut req = vec![Method::TsFromStr as u8];
                    WireWriter { out: &mut req }.str("");
                    if !try_write_child_msg(&mut fd, &ChildMsg::Dispatch { bytes: &req }) {
                        break;
                    }
                    if read_host_frame(&mut fd, &mut buf).is_none() {
                        break;
                    }
                }
            },
            tight,
        );
        let err = match srv.expand(
            &export,
            &call,
            Edition::E2021,
            Sandbox::new(Arc::clone(&pkg)),
            Limits::STANDARD,
            NativePaths::default(),
            &[],
            heap.alloc(),
            alloc,
        ) {
            Ok(_) => panic!("dispatch cap must stop"),
            Err(e) => e,
        };
        assert!(matches!(err, CallError::Limit { limit: ProcLimit::Fuel, .. }));
        srv.shutdown();
        h.join().expect("join 4");

        // 5. Oversized frame header triggers Limit::Output immediately
        let mut small_frame = NativeBinConfig::STANDARD;
        small_frame.max_frame_bytes = 64;
        let (mut srv, h) = run_loopback_peer(
            |mut fd| {
                let mut buf = Vec::new();
                let _ = read_host_frame(&mut fd, &mut buf);
                let _ = fd.write_all(&1024u32.to_be_bytes());
            },
            small_frame,
        );
        let err = srv.handshake(alloc).expect_err("oversized frame must stop");
        assert!(matches!(err, CallError::Limit { limit: ProcLimit::Output, .. }));
        assert_eq!(FRAME_PREFIX_LEN, PREFIX);
        srv.shutdown();
        h.join().expect("join 5");
    }

    /// Deterministic Simulation Test (AGENTS.md rule 43) for `PoolCore` with an oracle and two
    /// planted-bug controls proving:
    /// - idle counts never exceed `max_idle_per_key` or `max_total_idle`,
    /// - unhealthy/errored workers are NEVER retained in the pool or re-acquired,
    /// - every spawned worker is accounted for and zero workers leak on `drain_all()`.
    fn run_pool_dst(seed: u64, plant: PoolPlant) -> Result<(), String> {
        #[derive(Debug)]
        struct FakeWorker {
            id: u64,
            poisoned: bool,
        }

        let max_per_key = 2usize;
        let max_total = 4usize;
        let mut pool: PoolCore<u8, FakeWorker> = PoolCore::new(max_per_key, max_total, plant);
        let mut rng = seed ^ 0x9e37_79b9_7f4a_7c15;
        let mut next = || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };

        let mut next_id = 1u64;
        let mut spawned = 0usize;
        let mut reaped = 0usize;
        let mut checked_out: Vec<(u8, FakeWorker)> = Vec::new();

        for step in 0..400usize {
            let op = next() % 10;
            if op < 5 || checked_out.is_empty() {
                // Acquire or spawn for a key in 0..4
                let key = (next() % 4) as u8;
                let worker = match pool.acquire(&key) {
                    Some(w) => {
                        if w.poisoned {
                            return Err(format!("step {step}: acquired poisoned worker {}", w.id));
                        }
                        w
                    }
                    None => {
                        let id = next_id;
                        next_id += 1;
                        spawned += 1;
                        FakeWorker { id, poisoned: false }
                    }
                };
                checked_out.push((key, worker));
            } else {
                // Release a random checked-out worker, healthy 70% of the time
                let idx = (next() as usize) % checked_out.len();
                let (key, mut worker) = checked_out.swap_remove(idx);
                let healthy = (next() % 10) < 7;
                if !healthy {
                    worker.poisoned = true;
                }
                if let Some(evicted) = pool.release(key, worker, healthy) {
                    drop(evicted);
                    reaped += 1;
                }
            }

            // Oracle invariants after every step
            if pool.idle_len() > max_total {
                return Err(format!(
                    "step {step}: idle_len {} exceeded max_total {max_total}",
                    pool.idle_len()
                ));
            }
            for k in 0..4u8 {
                if pool.idle_for_key(&k) > max_per_key {
                    return Err(format!(
                        "step {step}: idle_for_key({k}) {} exceeded max_per_key {max_per_key}",
                        pool.idle_for_key(&k)
                    ));
                }
            }
            if spawned != reaped + checked_out.len() + pool.idle_len() {
                return Err(format!(
                    "step {step}: worker accounting mismatch: spawned={spawned}, reaped={reaped}, out={}, idle={}",
                    checked_out.len(),
                    pool.idle_len()
                ));
            }
        }

        // Shutdown: release remaining checked-out workers as unhealthy and drain all idle workers.
        for (key, mut w) in checked_out.drain(..) {
            w.poisoned = true;
            if let Some(evicted) = pool.release(key, w, false) {
                drop(evicted);
                reaped += 1;
            }
        }
        let drained = pool.drain_all();
        reaped += drained.len();
        drop(drained);
        if pool.idle_len() != 0 {
            return Err("pool not empty after drain_all".to_owned());
        }
        if reaped != spawned {
            return Err(format!("leaked workers: spawned={spawned}, reaped={reaped}"));
        }
        Ok(())
    }

    #[test]
    fn pool_core_dst_passes_clean_and_catches_both_planted_bugs() {
        for seed in 1..=32u64 {
            run_pool_dst(seed, PoolPlant::None).unwrap_or_else(|e| panic!("seed {seed} failed: {e}"));
        }
        // Control 1: DropCapEviction must fail the cap oracle.
        let cap_caught = (1..=16u64).any(|seed| run_pool_dst(seed, PoolPlant::DropCapEviction).is_err());
        assert!(cap_caught, "DST oracle must catch PoolPlant::DropCapEviction");
        // Control 2: KeepErroredWorker must fail the poisoned-worker oracle.
        let poison_caught = (1..=16u64).any(|seed| run_pool_dst(seed, PoolPlant::KeepErroredWorker).is_err());
        assert!(poison_caught, "DST oracle must catch PoolPlant::KeepErroredWorker");
    }

    #[test]
    fn loopback_tracks_untracked_fs_reads_and_invalidates_on_edit_add_delete() {
        let heap = blaze_own::testing::heap();
        let alloc = heap.alloc();
        let dir = std::env::temp_dir().join(format!("blaze-native-bin-reads-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".sqlx")).expect("mkdir");
        std::fs::write(dir.join("Cargo.toml"), b"[package]\nname=\"demo\"\n").expect("cargo toml");
        std::fs::write(dir.join(".sqlx/q1.json"), b"{\"sql\":\"SELECT 1\"}").expect("q1");

        let pkg = Arc::new(
            Package::new(Files::Folder(dir.clone()), PackageEnv::set_only(Vec::new()).expect("env"))
                .expect("Package"),
        );
        let export = Exported { index: 0, kind: ProcKind::FnLike, name: "q".to_owned(), helpers: Vec::new() };
        let input = lex("", Edition::E2021, alloc).expect("lex");
        let call = ProcCall { kind: ProcKind::FnLike, input: &input, attr_args: None, file: Ok("src/lib.rs") };

        // Peer produces output WITHOUT ever calling `TrackPath` (simulating direct `std::fs::read`).
        let (mut srv, h) = run_loopback_peer(
            |mut fd| {
                let mut buf = Vec::new();
                let _ = read_host_frame(&mut fd, &mut buf);
                write_child_msg(&mut fd, &ChildMsg::HelloAck { magic: MAGIC, version: PROTO_VERSION });
                let _ = read_host_frame(&mut fd, &mut buf);
                let mut out_bytes = Vec::new();
                let mut ww = WireWriter { out: &mut out_bytes };
                ww.ok(true);
                ww.some(false);
                write_child_msg(&mut fd, &ChildMsg::Output { bytes: &out_bytes });
                let _ = read_host_frame(&mut fd, &mut buf);
            },
            NativeBinConfig::STANDARD,
        );
        let dir_str = dir.to_str().expect("utf8");
        let paths = NativePaths { manifest_dir: Some(dir_str), workspace_root: None };
        let exp = srv
            .expand(
                &export,
                &call,
                Edition::E2021,
                Sandbox::new(Arc::clone(&pkg)),
                Limits::STANDARD,
                paths,
                &[Path::new("")],
                heap.alloc(),
                alloc,
            )
            .expect("expand");
        assert!(exp.cacheable);
        srv.shutdown();
        h.join().expect("join");

        let all_reads_hold = |pkg: &Package, reads: &Reads<'_>| -> bool {
            reads.list().iter().all(|entry| {
                let subj = reads.subject(entry);
                let Ok(now) = crate::hook::read_now(pkg, subj) else { return false };
                now == entry.answer
            })
        };

        // 1. Unchanged directory -> every recorded read holds (cache HIT!).
        assert!(all_reads_hold(&pkg, &exp.produced.reads), "unchanged tree must validate");

        // 2. Modifying an UNTRACKED file inside the granted root invalidates cache!
        std::fs::write(dir.join(".sqlx/q1.json"), b"{\"sql\":\"SELECT 2\"}").expect("edit q1");
        assert!(!all_reads_hold(&pkg, &exp.produced.reads), "editing untracked file must invalidate reads");
        std::fs::write(dir.join(".sqlx/q1.json"), b"{\"sql\":\"SELECT 1\"}").expect("restore q1");
        assert!(all_reads_hold(&pkg, &exp.produced.reads), "restored file must validate again");

        // 3. Adding a new file inside the granted root invalidates cache via dir listing!
        std::fs::write(dir.join(".sqlx/q2.json"), b"{}").expect("add q2");
        assert!(!all_reads_hold(&pkg, &exp.produced.reads), "adding file must invalidate reads");
        std::fs::remove_file(dir.join(".sqlx/q2.json")).expect("rm q2");
        assert!(all_reads_hold(&pkg, &exp.produced.reads), "removing added file must validate again");

        // 4. Deleting an existing file invalidates cache!
        std::fs::remove_file(dir.join(".sqlx/q1.json")).expect("rm q1");
        assert!(!all_reads_hold(&pkg, &exp.produced.reads), "deleting file must invalidate reads");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn loopback_refuses_host_checkout_path_leak_in_tokens() {
        let heap = blaze_own::testing::heap();
        let alloc = heap.alloc();
        let pkg = make_test_package(&[], &[]);
        let export = Exported { index: 0, kind: ProcKind::FnLike, name: "leak".to_owned(), helpers: Vec::new() };
        let input = lex("", Edition::E2021, alloc).expect("lex");
        let call = ProcCall { kind: ProcKind::FnLike, input: &input, attr_args: None, file: Ok("src/lib.rs") };

        let (mut srv, h) = run_loopback_peer(
            |mut fd| {
                let mut buf = Vec::new();
                let _ = read_host_frame(&mut fd, &mut buf);
                write_child_msg(&mut fd, &ChildMsg::HelloAck { magic: MAGIC, version: PROTO_VERSION });
                let _ = read_host_frame(&mut fd, &mut buf);

                // Emit a string literal containing the host checkout path.
                let mut ts_req = vec![Method::TsFromStr as u8];
                WireWriter { out: &mut ts_req }.str("const P: &str = \"/home/ix/secret-checkout/pkg/data\";");
                write_child_msg(&mut fd, &ChildMsg::Dispatch { bytes: &ts_req });
                let rep = read_host_frame(&mut fd, &mut buf).expect("reply");
                let HostMsg::Reply { bytes: rep_bytes } = decode_host_body(&rep).expect("reply") else {
                    panic!("expected Reply");
                };
                let mut wr = WireReader::new(rep_bytes);
                assert_eq!(wr.u8().expect("ok"), 0);
                assert!(wr.some().expect("some"));
                let out_handle = wr.handle().expect("handle");

                let mut out_bytes = Vec::new();
                let mut ww = WireWriter { out: &mut out_bytes };
                ww.ok(true);
                ww.some(true);
                ww.handle(out_handle);
                write_child_msg(&mut fd, &ChildMsg::Output { bytes: &out_bytes });
                let _ = read_host_frame(&mut fd, &mut buf);
            },
            NativeBinConfig::STANDARD,
        );
        let paths = NativePaths {
            manifest_dir: Some("/home/ix/secret-checkout/pkg"),
            workspace_root: Some("/home/ix/secret-checkout"),
        };
        let err = match srv.expand(
            &export,
            &call,
            Edition::E2021,
            Sandbox::new(pkg),
            Limits::STANDARD,
            paths,
            &[],
            heap.alloc(),
            alloc,
        ) {
            Ok(_) => panic!("host path leak in tokens must be refused"),
            Err(e) => e,
        };
        assert!(matches!(err, CallError::Refused(ref msg) if msg.contains("leaked host checkout path")));
        srv.shutdown();
        h.join().expect("join");
    }

    #[test]
    fn loopback_marks_uncacheable_when_file_mutates_mid_expansion() {
        let heap = blaze_own::testing::heap();
        let alloc = heap.alloc();
        let dir = std::env::temp_dir().join(format!("blaze-native-bin-mid-edit-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".sqlx")).expect("mkdir");
        std::fs::write(dir.join(".sqlx/q1.json"), b"{\"sql\":\"SELECT 1\"}").expect("q1");

        let pkg = Arc::new(
            Package::new(Files::Folder(dir.clone()), PackageEnv::set_only(Vec::new()).expect("env"))
                .expect("Package"),
        );
        let export = Exported { index: 0, kind: ProcKind::FnLike, name: "q".to_owned(), helpers: Vec::new() };
        let input = lex("", Edition::E2021, alloc).expect("lex");
        let call = ProcCall { kind: ProcKind::FnLike, input: &input, attr_args: None, file: Ok("src/lib.rs") };

        let target_file = dir.join(".sqlx/q1.json");
        let (mut srv, h) = run_loopback_peer(
            move |mut fd| {
                let mut buf = Vec::new();
                let _ = read_host_frame(&mut fd, &mut buf);
                write_child_msg(&mut fd, &ChildMsg::HelloAck { magic: MAGIC, version: PROTO_VERSION });
                let _ = read_host_frame(&mut fd, &mut buf);
                // Simulate concurrent external edit between pre-snapshot and post-snapshot!
                std::fs::write(&target_file, b"{\"sql\":\"SELECT 999\"}").expect("mid-edit");
                let mut out_bytes = Vec::new();
                let mut ww = WireWriter { out: &mut out_bytes };
                ww.ok(true);
                ww.some(false);
                write_child_msg(&mut fd, &ChildMsg::Output { bytes: &out_bytes });
                let _ = read_host_frame(&mut fd, &mut buf);
            },
            NativeBinConfig::STANDARD,
        );
        let dir_str = dir.to_str().expect("utf8");
        let paths = NativePaths { manifest_dir: Some(dir_str), workspace_root: None };
        let exp = srv
            .expand(
                &export,
                &call,
                Edition::E2021,
                Sandbox::new(Arc::clone(&pkg)),
                Limits::STANDARD,
                paths,
                &[Path::new("")],
                heap.alloc(),
                alloc,
            )
            .expect("expand succeeds even when uncacheable");
        assert!(!exp.cacheable, "mid-expansion file mutation MUST set cacheable=false");
        srv.shutdown();
        h.join().expect("join");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn loopback_tracks_symlink_link_target_retarget_with_identical_len_and_bytes() {
        use std::os::unix::fs::symlink;
        let heap = blaze_own::testing::heap();
        let alloc = heap.alloc();
        let dir = std::env::temp_dir().join(format!("blaze-native-bin-symlink-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        // Two target files with EQUAL path lengths and IDENTICAL contents.
        std::fs::write(dir.join("a.sql"), b"SELECT 42").expect("a.sql");
        std::fs::write(dir.join("b.sql"), b"SELECT 42").expect("b.sql");
        symlink("a.sql", dir.join("link.sql")).expect("symlink a");

        let pkg = Arc::new(
            Package::new(Files::Folder(dir.clone()), PackageEnv::set_only(Vec::new()).expect("env"))
                .expect("Package"),
        );
        let export = Exported { index: 0, kind: ProcKind::FnLike, name: "q".to_owned(), helpers: Vec::new() };
        let input = lex("", Edition::E2021, alloc).expect("lex");
        let call = ProcCall { kind: ProcKind::FnLike, input: &input, attr_args: None, file: Ok("src/lib.rs") };

        let (mut srv, h) = run_loopback_peer(
            |mut fd| {
                let mut buf = Vec::new();
                let _ = read_host_frame(&mut fd, &mut buf);
                write_child_msg(&mut fd, &ChildMsg::HelloAck { magic: MAGIC, version: PROTO_VERSION });
                let _ = read_host_frame(&mut fd, &mut buf);
                let mut out_bytes = Vec::new();
                let mut ww = WireWriter { out: &mut out_bytes };
                ww.ok(true);
                ww.some(false);
                write_child_msg(&mut fd, &ChildMsg::Output { bytes: &out_bytes });
                let _ = read_host_frame(&mut fd, &mut buf);
            },
            NativeBinConfig::STANDARD,
        );
        let dir_str = dir.to_str().expect("utf8");
        let exp = srv
            .expand(
                &export,
                &call,
                Edition::E2021,
                Sandbox::new(Arc::clone(&pkg)),
                Limits::STANDARD,
                NativePaths { manifest_dir: Some(dir_str), workspace_root: None },
                &[Path::new("")],
                heap.alloc(),
                alloc,
            )
            .expect("expand");
        assert!(exp.cacheable);
        srv.shutdown();
        h.join().expect("join");

        let all_reads_hold = |pkg: &Package, reads: &Reads<'_>| -> bool {
            reads.list().iter().all(|entry| {
                let subj = reads.subject(entry);
                let Ok(now) = crate::hook::read_now(pkg, subj) else { return false };
                now == entry.answer
            })
        };
        assert!(
            exp.produced.reads.list().iter().any(|e| exp.produced.reads.subject(e) == "link:link.sql"),
            "granted-tree snapshot must record `link:link.sql` digest"
        );
        assert!(all_reads_hold(&pkg, &exp.produced.reads), "initial symlink must hold");

        // Retarget symlink to `b.sql` (same target length AND same bytes!) — `link:link.sql` MUST invalidate!
        std::fs::remove_file(dir.join("link.sql")).expect("rm link");
        symlink("b.sql", dir.join("link.sql")).expect("symlink b");
        assert!(
            !all_reads_hold(&pkg, &exp.produced.reads),
            "retargeting symlink to equal-length path with identical content must invalidate reads"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn loopback_marks_uncacheable_when_snapshot_byte_cap_exceeded() {
        let heap = blaze_own::testing::heap();
        let alloc = heap.alloc();
        let dir = std::env::temp_dir().join(format!("blaze-native-bin-byte-cap-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        // Write a 4 KiB file and set snapshot_max_file_bytes = 1024 so streaming short-circuits.
        std::fs::write(dir.join("big.bin"), vec![0xABu8; 4096]).expect("big.bin");

        let pkg = Arc::new(
            Package::new(Files::Folder(dir.clone()), PackageEnv::set_only(Vec::new()).expect("env"))
                .expect("Package"),
        );
        let export = Exported { index: 0, kind: ProcKind::FnLike, name: "q".to_owned(), helpers: Vec::new() };
        let input = lex("", Edition::E2021, alloc).expect("lex");
        let call = ProcCall { kind: ProcKind::FnLike, input: &input, attr_args: None, file: Ok("src/lib.rs") };

        let cfg = NativeBinConfig {
            snapshot_max_file_bytes: 1024,
            snapshot_max_total_bytes: 2048,
            ..NativeBinConfig::STANDARD
        };
        let (mut srv, h) = run_loopback_peer(
            |mut fd| {
                let mut buf = Vec::new();
                let _ = read_host_frame(&mut fd, &mut buf);
                write_child_msg(&mut fd, &ChildMsg::HelloAck { magic: MAGIC, version: PROTO_VERSION });
                let _ = read_host_frame(&mut fd, &mut buf);
                let mut out_bytes = Vec::new();
                let mut ww = WireWriter { out: &mut out_bytes };
                ww.ok(true);
                ww.some(false);
                write_child_msg(&mut fd, &ChildMsg::Output { bytes: &out_bytes });
                let _ = read_host_frame(&mut fd, &mut buf);
            },
            cfg,
        );
        let dir_str = dir.to_str().expect("utf8");
        let exp = srv
            .expand(
                &export,
                &call,
                Edition::E2021,
                Sandbox::new(Arc::clone(&pkg)),
                Limits::STANDARD,
                NativePaths { manifest_dir: Some(dir_str), workspace_root: None },
                &[Path::new("")],
                heap.alloc(),
                alloc,
            )
            .expect("expand succeeds");
        assert!(!exp.cacheable, "exceeding snapshot byte cap MUST set cacheable=false");
        srv.shutdown();
        h.join().expect("join");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn loopback_workspace_member_tracks_sibling_relative_read() {
        let heap = blaze_own::testing::heap();
        let alloc = heap.alloc();
        let ws = std::env::temp_dir().join(format!("blaze-native-bin-ws-member-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&ws);
        std::fs::create_dir_all(ws.join("crates/app/src")).expect("mkdir app");
        std::fs::create_dir_all(ws.join("crates/shared")).expect("mkdir shared");
        std::fs::write(ws.join("crates/app/Cargo.toml"), b"[package]\nname=\"app\"\n").expect("app toml");
        std::fs::write(ws.join("crates/shared/schema.sql"), b"CREATE TABLE t (id INT);").expect("schema");

        let root = crate::script_sandbox::ScriptRoot::member("crates/app").expect("member root");
        let pkg = Arc::new(
            Package::new(
                Files::Member { workspace: ws.clone(), root },
                PackageEnv::set_only(Vec::new()).expect("env"),
            )
            .expect("Package"),
        );
        let export = Exported { index: 0, kind: ProcKind::FnLike, name: "q".to_owned(), helpers: Vec::new() };
        let input = lex("", Edition::E2021, alloc).expect("lex");
        let call = ProcCall { kind: ProcKind::FnLike, input: &input, attr_args: None, file: Ok("crates/app/src/lib.rs") };

        let (mut srv, h) = run_loopback_peer(
            |mut fd| {
                let mut buf = Vec::new();
                let _ = read_host_frame(&mut fd, &mut buf);
                write_child_msg(&mut fd, &ChildMsg::HelloAck { magic: MAGIC, version: PROTO_VERSION });
                let _ = read_host_frame(&mut fd, &mut buf);
                // Query CARGO_MANIFEST_DIR -> must receive "." (zero host paths)
                write_child_msg(&mut fd, &ChildMsg::EnvVar { name: "CARGO_MANIFEST_DIR" });
                let rep = read_host_frame(&mut fd, &mut buf).expect("env rep");
                assert_eq!(decode_host_body(&rep).expect("env"), HostMsg::EnvVal { value: Some(".") });
                let mut out_bytes = Vec::new();
                let mut ww = WireWriter { out: &mut out_bytes };
                ww.ok(true);
                ww.some(false);
                write_child_msg(&mut fd, &ChildMsg::Output { bytes: &out_bytes });
                let _ = read_host_frame(&mut fd, &mut buf);
            },
            NativeBinConfig::STANDARD,
        );
        let app_dir = ws.join("crates/app");
        let exp = srv
            .expand(
                &export,
                &call,
                Edition::E2021,
                Sandbox::new(Arc::clone(&pkg)),
                Limits::STANDARD,
                NativePaths {
                    manifest_dir: Some(app_dir.to_str().expect("utf8")),
                    workspace_root: Some(ws.to_str().expect("utf8")),
                },
                &[Path::new("")],
                heap.alloc(),
                alloc,
            )
            .expect("expand");
        assert!(exp.cacheable);
        srv.shutdown();
        h.join().expect("join");

        let all_reads_hold = |pkg: &Package, reads: &Reads<'_>| -> bool {
            reads.list().iter().all(|entry| {
                let subj = reads.subject(entry);
                let Ok(now) = crate::hook::read_now(pkg, subj) else { return false };
                now == entry.answer
            })
        };
        assert!(all_reads_hold(&pkg, &exp.produced.reads), "initial workspace member must hold");
        // Mutating sibling `../shared/schema.sql` MUST invalidate cached reads!
        std::fs::write(ws.join("crates/shared/schema.sql"), b"CREATE TABLE t (id BIGINT);").expect("edit schema");
        assert!(
            !all_reads_hold(&pkg, &exp.produced.reads),
            "editing workspace sibling `../shared/schema.sql` must invalidate cached reads"
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn trim_idle_buffers_drops_oversized_capacity() {
        let heap = blaze_own::testing::heap();
        let alloc = heap.alloc();
        let big_env = "x".repeat(IDLE_BUFFER_CAP_LIMIT * 2);
        let pkg = make_test_package(&[("BIG_ENV", big_env.as_str())], &[]);
        let export = Exported { index: 0, kind: ProcKind::FnLike, name: "big".to_owned(), helpers: Vec::new() };
        let input = lex("", Edition::E2021, alloc).expect("lex");
        let call = ProcCall { kind: ProcKind::FnLike, input: &input, attr_args: None, file: Ok("src/lib.rs") };

        let (mut srv, h) = run_loopback_peer(
            |mut fd| {
                let mut buf = Vec::new();
                let _ = read_host_frame(&mut fd, &mut buf);
                write_child_msg(&mut fd, &ChildMsg::HelloAck { magic: MAGIC, version: PROTO_VERSION });
                let _ = read_host_frame(&mut fd, &mut buf);
                // Request BIG_ENV so host `outbox` grows past IDLE_BUFFER_CAP_LIMIT.
                write_child_msg(&mut fd, &ChildMsg::EnvVar { name: "BIG_ENV" });
                let _ = read_host_frame(&mut fd, &mut buf);
                // Return a large panic message (> IDLE_BUFFER_CAP_LIMIT) so `inbox` also grows past cap.
                let big_msg = "p".repeat(IDLE_BUFFER_CAP_LIMIT * 2);
                write_child_msg(&mut fd, &ChildMsg::Panic { message: &big_msg });
                let _ = read_host_frame(&mut fd, &mut buf);
            },
            NativeBinConfig::STANDARD,
        );
        let exp = srv
            .expand(
                &export,
                &call,
                Edition::E2021,
                Sandbox::new(pkg),
                Limits::STANDARD,
                NativePaths::default(),
                &[],
                heap.alloc(),
                alloc,
            )
            .expect("expand");
        assert!(!exp.produced.diagnostics.is_empty(), "expected panic diagnostic");
        let (in_before, out_before) = srv.buffer_capacities();
        assert!(in_before > IDLE_BUFFER_CAP_LIMIT, "inbox should have grown past limit: {in_before}");
        assert!(out_before > IDLE_BUFFER_CAP_LIMIT, "outbox should have grown past limit: {out_before}");

        srv.trim_idle_buffers();
        let (in_after, out_after) = srv.buffer_capacities();
        assert!(in_after <= IDLE_BUFFER_CAP_LIMIT, "inbox capacity must shrink: {in_after}");
        assert!(out_after <= IDLE_BUFFER_CAP_LIMIT, "outbox capacity must shrink: {out_after}");

        srv.shutdown();
        h.join().expect("join");
    }
}
