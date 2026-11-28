//! Explicit typestate sans-IO machine for authenticated, content-addressed remote cache pull and
//! push (`docs/v2/plan/design/sans-io-machines.md`).
//!
//! Pull sequence:
//! `HeadCheck` -> (`Absent` => `Turn::Done(Miss)`) | (`Found` => `DownloadBundle` ->
//! `VerifyContentHash` -> `CommitLocal` -> `Turn::Done(Pulled)`).
//!
//! Push sequence:
//! `HeadCheck` -> (`Found` => `Turn::Done(AlreadyPresent)`) | (`Absent` => `UploadBundle` ->
//! `Turn::Done(Pushed)`).
//!
//! Invariants:
//! - Never commits unverified bytes to local storage: downloaded payload length, namespace hash,
//!   and BLAKE3 content hash are verified BEFORE emitting [`RemoteCacheReq::CommitLocal`]
//!   ([`RemoteCacheError::HashMismatch`]), outside the explicit planted-bug control
//!   [`RemoteMachinePlant::AcceptUnverifiedBundle`].
//! - Never leaks private namespace keys across namespaces: [`LOCAL_NAMESPACE_HASH`] (all-zero) is
//!   always refused before any remote request, `record_namespace_hash` must equal `namespace_hash`,
//!   and every emitted request and downloaded bundle carries an explicit `namespace_hash: [u8; 32]`.

#![blaze::sans_io]
#![forbid(unsafe_code)]

use core::fmt;

use blaze_machine::{
    EdgeKind, GraphEdge, HandleOp, Machine, MachineGraph, RecordableRequest, Request, StateId,
    StateNode, Turn,
};
use blaze_own::FixedVec;
use blaze_own::planted::{NoPlant, Planted, Plants};

/// Sentinel namespace hash for local-only private records that must never leave the machine.
pub const LOCAL_NAMESPACE_HASH: [u8; 32] = [0u8; 32];

/// Maximum inline payload captured by [`blaze_machine::RecordHandler`] for zero-IO
/// [`blaze_machine::Script`] replay without global heap allocation.
pub const RECORDED_PAYLOAD_MAX: usize = 128;

/// Planted-bug switches for [`RemoteCacheMachine`] controls (rules 43 & 53).
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum RemoteMachinePlant {
    #[default]
    None,
    /// Planted bug: accept downloaded bundle bytes without verifying their BLAKE3 content hash
    /// before emitting [`RemoteCacheReq::CommitLocal`].
    AcceptUnverifiedBundle,
    /// Planted bug: skip namespace isolation checks on entry and on downloaded bundle answers.
    SkipNamespaceCheck,
}

impl Plants for RemoteMachinePlant {}

/// Typed remote transport failure (single-try; never silently retried, rule 3).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RemoteTransportError {
    Unauthorized,
    Connect,
    Tls,
    Timeout,
    HttpStatus(u16),
}

impl fmt::Display for RemoteTransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RemoteTransportError::Unauthorized => f.write_str("remote cache authentication failed"),
            RemoteTransportError::Connect => f.write_str("remote cache connection failed"),
            RemoteTransportError::Tls => f.write_str("remote cache TLS handshake failed"),
            RemoteTransportError::Timeout => f.write_str("remote cache request timed out"),
            RemoteTransportError::HttpStatus(code) => write!(f, "remote cache HTTP status {code}"),
        }
    }
}

/// Typed local store failure returned by [`RemoteCacheReq::CommitLocal`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LocalStoreError {
    Io,
    DiskFull,
    Corrupt,
}

impl fmt::Display for LocalStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LocalStoreError::Io => f.write_str("local store IO error"),
            LocalStoreError::DiskFull => f.write_str("local store disk full"),
            LocalStoreError::Corrupt => f.write_str("local store record corrupt"),
        }
    }
}

/// Typed `#![no_std]`, zero-heap requests emitted by [`RemoteCacheMachine`].
///
/// Every variant binds `namespace_hash: [u8; 32]` explicitly so no private namespace key can be
/// routed to or committed under another namespace.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RemoteCacheReq {
    /// Checks whether `key` exists in `namespace_hash` on the authenticated remote bucket.
    HeadCheck {
        namespace_hash: [u8; 32],
        key: [u8; 32],
        auth_token_hash: [u8; 32],
    },
    /// Downloads bundle bytes for `key` in `namespace_hash` under explicit byte & timeout caps.
    DownloadBundle {
        namespace_hash: [u8; 32],
        key: [u8; 32],
        auth_token_hash: [u8; 32],
        max_bytes: u64,
        timeout_ns: u64,
    },
    /// Commits a hash-verified, namespace-verified bundle into local storage.
    CommitLocal {
        namespace_hash: [u8; 32],
        key: [u8; 32],
        content_hash: [u8; 32],
        bytes_len: u64,
    },
    /// Uploads a content-addressed bundle to `namespace_hash` after `HeadCheck` returned absent.
    UploadBundle {
        namespace_hash: [u8; 32],
        key: [u8; 32],
        auth_token_hash: [u8; 32],
        content_hash: [u8; 32],
        bytes_len: u64,
        timeout_ns: u64,
    },
}

/// Answer to [`RemoteCacheReq::HeadCheck`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HeadCheckAnswer {
    Found { bytes_len: u64 },
    Absent,
    Err(RemoteTransportError),
}

/// Downloaded bundle payload: zero-copy driver slice or inline recorded buffer for `Script` replay.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum DownloadBytes<'s> {
    /// Zero-copy slice borrowed from the driver's scratch buffer (or a `'static` test fixture).
    Borrowed(&'s [u8]),
    /// Owned inline buffer captured by [`blaze_machine::RecordHandler`] for zero-IO replay.
    Inline(FixedVec<u8, RECORDED_PAYLOAD_MAX>),
}

impl DownloadBytes<'_> {
    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        match self {
            DownloadBytes::Borrowed(bytes) => bytes,
            DownloadBytes::Inline(buf) => buf.as_slice(),
        }
    }
}

/// Answer to [`RemoteCacheReq::DownloadBundle`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum DownloadBundleAnswer<'s> {
    Ok { namespace_hash: [u8; 32], bytes: DownloadBytes<'s> },
    Err(RemoteTransportError),
}

/// Typed answer family paired with [`RemoteCacheReq`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum RemoteCacheAnswer<'s> {
    HeadCheck(HeadCheckAnswer),
    DownloadBundle(DownloadBundleAnswer<'s>),
    CommitLocal(Result<(), LocalStoreError>),
    UploadBundle(Result<(), RemoteTransportError>),
}

impl Request for RemoteCacheReq {
    type Answer<'s> = RemoteCacheAnswer<'s>;
}

/// Owned recorded download answer used by [`blaze_machine::RecordHandler`] -> `Script` replay.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum RecordedDownloadAnswer {
    Ok { namespace_hash: [u8; 32], bytes: FixedVec<u8, RECORDED_PAYLOAD_MAX> },
    Err(RemoteTransportError),
}

/// Owned recorded answer captured by [`blaze_machine::RecordHandler`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum RecordedRemoteAnswer {
    HeadCheck(HeadCheckAnswer),
    DownloadBundle(RecordedDownloadAnswer),
    CommitLocal(Result<(), LocalStoreError>),
    UploadBundle(Result<(), RemoteTransportError>),
}

impl<'s> From<RecordedRemoteAnswer> for RemoteCacheAnswer<'s> {
    #[inline]
    fn from(value: RecordedRemoteAnswer) -> Self {
        match value {
            RecordedRemoteAnswer::HeadCheck(ans) => RemoteCacheAnswer::HeadCheck(ans),
            RecordedRemoteAnswer::DownloadBundle(RecordedDownloadAnswer::Ok {
                namespace_hash,
                bytes,
            }) => RemoteCacheAnswer::DownloadBundle(DownloadBundleAnswer::Ok {
                namespace_hash,
                bytes: DownloadBytes::Inline(bytes),
            }),
            RecordedRemoteAnswer::DownloadBundle(RecordedDownloadAnswer::Err(err)) => {
                RemoteCacheAnswer::DownloadBundle(DownloadBundleAnswer::Err(err))
            }
            RecordedRemoteAnswer::CommitLocal(res) => RemoteCacheAnswer::CommitLocal(res),
            RecordedRemoteAnswer::UploadBundle(res) => RemoteCacheAnswer::UploadBundle(res),
        }
    }
}

impl RecordableRequest for RemoteCacheReq {
    type Recorded = RecordedRemoteAnswer;

    fn record_answer(answer: &Self::Answer<'_>) -> Self::Recorded {
        match answer {
            RemoteCacheAnswer::HeadCheck(ans) => RecordedRemoteAnswer::HeadCheck(*ans),
            RemoteCacheAnswer::DownloadBundle(DownloadBundleAnswer::Ok {
                namespace_hash,
                bytes,
            }) => {
                let mut inline = FixedVec::new();
                for &b in bytes.as_slice().iter().take(RECORDED_PAYLOAD_MAX) {
                    if inline.push(b).is_err() {
                        break;
                    }
                }
                RecordedRemoteAnswer::DownloadBundle(RecordedDownloadAnswer::Ok {
                    namespace_hash: *namespace_hash,
                    bytes: inline,
                })
            }
            RemoteCacheAnswer::DownloadBundle(DownloadBundleAnswer::Err(err)) => {
                RecordedRemoteAnswer::DownloadBundle(RecordedDownloadAnswer::Err(*err))
            }
            RemoteCacheAnswer::CommitLocal(res) => RecordedRemoteAnswer::CommitLocal(*res),
            RemoteCacheAnswer::UploadBundle(res) => RecordedRemoteAnswer::UploadBundle(*res),
        }
    }
}

/// Wrapper allowing `&'static [u8]` answers in [`blaze_machine::Script`] tests to convert cleanly
/// into `RemoteCacheAnswer<'s>` for any driver borrow lifetime `'s`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ScriptRemoteAnswer(pub RemoteCacheAnswer<'static>);

impl<'s> From<ScriptRemoteAnswer> for RemoteCacheAnswer<'s> {
    #[inline]
    fn from(value: ScriptRemoteAnswer) -> Self {
        value.0
    }
}

/// Operation performed by [`RemoteCacheMachine`]: content-addressed pull or deduplicated push.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RemoteCacheOp {
    Pull { expected_content_hash: [u8; 32], max_bytes: u64 },
    Push { content_hash: [u8; 32], bytes_len: u64 },
}

/// Specification given to [`RemoteCacheMachine`] (`#[blaze::args]`, rule 83).
#[blaze::args]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RemoteCacheSpec {
    /// Target remote namespace hash checked on every request.
    pub namespace_hash: [u8; 32],
    /// Namespace hash stamped on the record being pulled or pushed; must equal `namespace_hash`.
    pub record_namespace_hash: [u8; 32],
    /// Content-addressed key of the bundle.
    pub key: [u8; 32],
    /// Authenticated credential digest bound to every remote request.
    pub auth_token_hash: [u8; 32],
    /// Pull or push operation parameters.
    pub op: RemoteCacheOp,
    /// Per-request timeout in nanoseconds (rule 18 `bounded-progress`).
    pub timeout_ns: u64,
}

/// Terminal success outcome of [`RemoteCacheMachine`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RemoteCacheOutcome {
    /// Pull verified content hash & namespace and committed `bytes_len` bytes locally.
    Pulled { bytes_len: u64 },
    /// Pull `HeadCheck` returned `Absent`; nothing was downloaded or committed.
    Miss,
    /// Push `HeadCheck` returned `Found`; upload was deduplicated and skipped.
    AlreadyPresent,
    /// Push `HeadCheck` returned `Absent`; `bytes_len` bytes were uploaded once.
    Pushed { bytes_len: u64 },
}

/// Terminal failure outcome of [`RemoteCacheMachine`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RemoteCacheError {
    /// Local-only private namespace (`[0u8; 32]`) may never cross the remote cache boundary.
    LocalNamespaceForbidden,
    /// Cross-namespace key or payload leak detected and refused before commit/network IO.
    NamespaceMismatch { expected: [u8; 32], actual: [u8; 32] },
    /// Downloaded payload BLAKE3 digest did not match `expected_content_hash`.
    HashMismatch { expected: [u8; 32], actual: [u8; 32] },
    /// Remote payload exceeded the caller's explicit `max_bytes` cap.
    PayloadTooLarge { actual: u64, max: u64 },
    /// Remote HTTP/transport failure.
    Transport(RemoteTransportError),
    /// Local store commit failure.
    LocalStore(LocalStoreError),
}

impl fmt::Display for RemoteCacheError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RemoteCacheError::LocalNamespaceForbidden => {
                f.write_str("local-only private namespace is forbidden on remote cache")
            }
            RemoteCacheError::NamespaceMismatch { .. } => {
                f.write_str("remote cache namespace mismatch refused")
            }
            RemoteCacheError::HashMismatch { .. } => {
                f.write_str("remote cache bundle BLAKE3 hash mismatch")
            }
            RemoteCacheError::PayloadTooLarge { actual, max } => {
                write!(f, "remote cache bundle length {actual} exceeds limit {max}")
            }
            RemoteCacheError::Transport(err) => write!(f, "remote cache transport error: {err}"),
            RemoteCacheError::LocalStore(err) => write!(f, "remote cache local store error: {err}"),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Init,
    HeadChecking,
    Downloading,
    CommittingLocal { bytes_len: u64 },
    Uploading { bytes_len: u64 },
}

/// Explicit typestate sans-IO machine for authenticated, content-addressed remote cache pull and
/// push (`#![no_std]`-clean, zero global heap, single-try remote requests).
#[derive(Clone, Copy, Debug)]
pub struct RemoteCacheMachine<P: Planted<RemoteMachinePlant> = NoPlant> {
    spec: RemoteCacheSpec,
    state: State,
    plant: P,
}

impl RemoteCacheMachine<NoPlant> {
    /// Creates a production [`RemoteCacheMachine`] with zero planted bugs.
    #[inline]
    pub const fn new(spec: RemoteCacheSpec) -> Self {
        Self { spec, state: State::Init, plant: NoPlant }
    }
}

impl<P: Planted<RemoteMachinePlant>> RemoteCacheMachine<P> {
    /// Creates a [`RemoteCacheMachine`] carrying a planted-bug seam for DST controls.
    #[inline]
    pub const fn with_plant(spec: RemoteCacheSpec, plant: P) -> Self {
        Self { spec, state: State::Init, plant }
    }
}

impl<P: Planted<RemoteMachinePlant>> Machine<()> for RemoteCacheMachine<P> {
    type Req = RemoteCacheReq;
    type Out = RemoteCacheOutcome;
    type Err = RemoteCacheError;

    fn start(self, _cx: &mut ()) -> Turn<Self, Self::Req, Self::Out, Self::Err> {
        let Self { spec, state, plant } = self;
        match state {
            State::Init => {
                if !plant.has(RemoteMachinePlant::SkipNamespaceCheck) {
                    if spec.namespace_hash == LOCAL_NAMESPACE_HASH
                        || spec.record_namespace_hash == LOCAL_NAMESPACE_HASH
                    {
                        return Turn::Fail(RemoteCacheError::LocalNamespaceForbidden);
                    }
                    if spec.record_namespace_hash != spec.namespace_hash {
                        return Turn::Fail(RemoteCacheError::NamespaceMismatch {
                            expected: spec.namespace_hash,
                            actual: spec.record_namespace_hash,
                        });
                    }
                }
                Turn::Ask {
                    request: RemoteCacheReq::HeadCheck {
                        namespace_hash: spec.namespace_hash,
                        key: spec.key,
                        auth_token_hash: spec.auth_token_hash,
                    },
                    next: Self { spec, state: State::HeadChecking, plant },
                }
            }
            State::HeadChecking
            | State::Downloading
            | State::CommittingLocal { .. }
            | State::Uploading { .. } => {
                unreachable!("RemoteCacheMachine::start called on non-Init state")
            }
        }
    }

    fn resume(
        self,
        answer: RemoteCacheAnswer<'_>,
        _cx: &mut (),
    ) -> Turn<Self, Self::Req, Self::Out, Self::Err> {
        let Self { spec, state, plant } = self;
        match (state, answer) {
            (State::HeadChecking, RemoteCacheAnswer::HeadCheck(HeadCheckAnswer::Err(err))) => {
                Turn::Fail(RemoteCacheError::Transport(err))
            }
            (State::HeadChecking, RemoteCacheAnswer::HeadCheck(HeadCheckAnswer::Absent)) => {
                match spec.op {
                    RemoteCacheOp::Pull { .. } => Turn::Done(RemoteCacheOutcome::Miss),
                    RemoteCacheOp::Push { content_hash, bytes_len } => Turn::Ask {
                        request: RemoteCacheReq::UploadBundle {
                            namespace_hash: spec.namespace_hash,
                            key: spec.key,
                            auth_token_hash: spec.auth_token_hash,
                            content_hash,
                            bytes_len,
                            timeout_ns: spec.timeout_ns,
                        },
                        next: Self { spec, state: State::Uploading { bytes_len }, plant },
                    },
                }
            }
            (
                State::HeadChecking,
                RemoteCacheAnswer::HeadCheck(HeadCheckAnswer::Found { bytes_len }),
            ) => match spec.op {
                RemoteCacheOp::Pull { max_bytes, .. } => {
                    if bytes_len > max_bytes {
                        return Turn::Fail(RemoteCacheError::PayloadTooLarge {
                            actual: bytes_len,
                            max: max_bytes,
                        });
                    }
                    Turn::Ask {
                        request: RemoteCacheReq::DownloadBundle {
                            namespace_hash: spec.namespace_hash,
                            key: spec.key,
                            auth_token_hash: spec.auth_token_hash,
                            max_bytes,
                            timeout_ns: spec.timeout_ns,
                        },
                        next: Self { spec, state: State::Downloading, plant },
                    }
                }
                RemoteCacheOp::Push { .. } => Turn::Done(RemoteCacheOutcome::AlreadyPresent),
            },
            (State::Downloading, RemoteCacheAnswer::DownloadBundle(DownloadBundleAnswer::Err(err))) => {
                Turn::Fail(RemoteCacheError::Transport(err))
            }
            (
                State::Downloading,
                RemoteCacheAnswer::DownloadBundle(DownloadBundleAnswer::Ok {
                    namespace_hash,
                    bytes,
                }),
            ) => {
                let RemoteCacheOp::Pull { expected_content_hash, max_bytes } = spec.op else {
                    unreachable!("Downloading state is only entered for Pull");
                };
                if !plant.has(RemoteMachinePlant::SkipNamespaceCheck)
                    && namespace_hash != spec.namespace_hash
                {
                    return Turn::Fail(RemoteCacheError::NamespaceMismatch {
                        expected: spec.namespace_hash,
                        actual: namespace_hash,
                    });
                }
                let raw = bytes.as_slice();
                let bytes_len = raw.len() as u64;
                if bytes_len > max_bytes {
                    return Turn::Fail(RemoteCacheError::PayloadTooLarge {
                        actual: bytes_len,
                        max: max_bytes,
                    });
                }
                let actual = blaze_ids::b3::hash(raw);
                if !plant.has(RemoteMachinePlant::AcceptUnverifiedBundle)
                    && actual != expected_content_hash
                {
                    return Turn::Fail(RemoteCacheError::HashMismatch {
                        expected: expected_content_hash,
                        actual,
                    });
                }
                Turn::Ask {
                    request: RemoteCacheReq::CommitLocal {
                        namespace_hash: spec.namespace_hash,
                        key: spec.key,
                        content_hash: actual,
                        bytes_len,
                    },
                    next: Self { spec, state: State::CommittingLocal { bytes_len }, plant },
                }
            }
            (State::CommittingLocal { bytes_len }, RemoteCacheAnswer::CommitLocal(Ok(()))) => {
                Turn::Done(RemoteCacheOutcome::Pulled { bytes_len })
            }
            (State::CommittingLocal { .. }, RemoteCacheAnswer::CommitLocal(Err(err))) => {
                Turn::Fail(RemoteCacheError::LocalStore(err))
            }
            (State::Uploading { bytes_len }, RemoteCacheAnswer::UploadBundle(Ok(()))) => {
                Turn::Done(RemoteCacheOutcome::Pushed { bytes_len })
            }
            (State::Uploading { .. }, RemoteCacheAnswer::UploadBundle(Err(err))) => {
                Turn::Fail(RemoteCacheError::Transport(err))
            }
            _ => unreachable!("RemoteCacheAnswer always matches active waiting state"),
        }
    }
}

/// Reflected state graph of [`RemoteCacheMachine`], verified by [`blaze_machine::verify_graph`].
pub fn remote_cache_graph() -> MachineGraph<'static> {
    const INIT: StateId = StateId(0);
    const HEAD_CHECKING: StateId = StateId(1);
    const DOWNLOADING: StateId = StateId(2);
    const COMMITTING_LOCAL: StateId = StateId(3);
    const UPLOADING: StateId = StateId(4);

    static STATES: [StateNode; 5] = [
        StateNode { id: INIT, name: "Init", loop_state: false, initial_handles: 0 },
        StateNode {
            id: HEAD_CHECKING,
            name: "HeadChecking",
            loop_state: false,
            initial_handles: 0,
        },
        StateNode { id: DOWNLOADING, name: "Downloading", loop_state: false, initial_handles: 0 },
        StateNode {
            id: COMMITTING_LOCAL,
            name: "CommittingLocal",
            loop_state: false,
            initial_handles: 0,
        },
        StateNode { id: UPLOADING, name: "Uploading", loop_state: false, initial_handles: 0 },
    ];

    static EDGES: [GraphEdge; 16] = [
        GraphEdge {
            from: INIT,
            kind: EdgeKind::Ask { to: HEAD_CHECKING },
            handle_op: HandleOp::None,
            label: "head_check",
        },
        GraphEdge {
            from: INIT,
            kind: EdgeKind::Fail,
            handle_op: HandleOp::None,
            label: "namespace_refused_before_head",
        },
        GraphEdge {
            from: HEAD_CHECKING,
            kind: EdgeKind::Done,
            handle_op: HandleOp::None,
            label: "pull_miss_or_push_dedup",
        },
        GraphEdge {
            from: HEAD_CHECKING,
            kind: EdgeKind::Ask { to: DOWNLOADING },
            handle_op: HandleOp::None,
            label: "pull_head_found_download",
        },
        GraphEdge {
            from: HEAD_CHECKING,
            kind: EdgeKind::Ask { to: UPLOADING },
            handle_op: HandleOp::None,
            label: "push_head_absent_upload",
        },
        GraphEdge {
            from: HEAD_CHECKING,
            kind: EdgeKind::Fail,
            handle_op: HandleOp::None,
            label: "head_err_or_too_large",
        },
        GraphEdge {
            from: HEAD_CHECKING,
            kind: EdgeKind::Abandon,
            handle_op: HandleOp::None,
            label: "head_cancel",
        },
        GraphEdge {
            from: DOWNLOADING,
            kind: EdgeKind::Ask { to: COMMITTING_LOCAL },
            handle_op: HandleOp::None,
            label: "download_verified_commit_local",
        },
        GraphEdge {
            from: DOWNLOADING,
            kind: EdgeKind::Fail,
            handle_op: HandleOp::None,
            label: "download_err_or_unverified",
        },
        GraphEdge {
            from: DOWNLOADING,
            kind: EdgeKind::Abandon,
            handle_op: HandleOp::None,
            label: "download_cancel",
        },
        GraphEdge {
            from: COMMITTING_LOCAL,
            kind: EdgeKind::Done,
            handle_op: HandleOp::None,
            label: "commit_local_ok",
        },
        GraphEdge {
            from: COMMITTING_LOCAL,
            kind: EdgeKind::Fail,
            handle_op: HandleOp::None,
            label: "commit_local_err",
        },
        GraphEdge {
            from: COMMITTING_LOCAL,
            kind: EdgeKind::Abandon,
            handle_op: HandleOp::None,
            label: "commit_local_cancel",
        },
        GraphEdge {
            from: UPLOADING,
            kind: EdgeKind::Done,
            handle_op: HandleOp::None,
            label: "upload_ok",
        },
        GraphEdge {
            from: UPLOADING,
            kind: EdgeKind::Fail,
            handle_op: HandleOp::None,
            label: "upload_err",
        },
        GraphEdge {
            from: UPLOADING,
            kind: EdgeKind::Abandon,
            handle_op: HandleOp::None,
            label: "upload_cancel",
        },
    ];

    MachineGraph {
        name: "RemoteCache",
        entry: INIT,
        grace_max_steps: 1,
        states: &STATES,
        edges: &EDGES,
    }
}

#[cfg(test)]
mod tests {
    use blaze_machine::{
        DriveError, DriveOpts, RecordHandler, Script, ScriptError, TranscriptEntry, drive,
        emit_lean, verify_graph,
    };
    use blaze_own::FixedString;

    use super::{
        DownloadBundleAnswer, DownloadBytes, HeadCheckAnswer, LOCAL_NAMESPACE_HASH,
        RemoteCacheAnswer, RemoteCacheError, RemoteCacheMachine, RemoteCacheOp, RemoteCacheOutcome,
        RemoteCacheReq, RemoteCacheSpec, RemoteMachinePlant, ScriptRemoteAnswer,
        remote_cache_graph,
    };

    const ORG_NS: [u8; 32] = [0x11; 32];
    const OTHER_NS: [u8; 32] = [0x22; 32];
    const KEY: [u8; 32] = [0xAA; 32];
    const AUTH_TOKEN_HASH: [u8; 32] = [0x55; 32];
    const VALID_BUNDLE: &[u8] = b"blaze-remote-verified-segment-bundle-v1";
    const CORRUPT_BUNDLE: &[u8] = b"tampered-unverified-segment-bytes";

    fn pull_spec() -> RemoteCacheSpec {
        RemoteCacheSpec {
            namespace_hash: ORG_NS,
            record_namespace_hash: ORG_NS,
            key: KEY,
            auth_token_hash: AUTH_TOKEN_HASH,
            op: RemoteCacheOp::Pull {
                expected_content_hash: blaze_ids::b3::hash(VALID_BUNDLE),
                max_bytes: 1024,
            },
            timeout_ns: 15_000_000_000,
        }
    }

    fn push_spec() -> RemoteCacheSpec {
        RemoteCacheSpec {
            namespace_hash: ORG_NS,
            record_namespace_hash: ORG_NS,
            key: KEY,
            auth_token_hash: AUTH_TOKEN_HASH,
            op: RemoteCacheOp::Push {
                content_hash: blaze_ids::b3::hash(VALID_BUNDLE),
                bytes_len: VALID_BUNDLE.len() as u64,
            },
            timeout_ns: 15_000_000_000,
        }
    }

    #[test]
    fn remote_cache_pull_happy_path_and_record_handler_replay() {
        let alloc = blaze_own::Alloc::global();
        let spec = pull_spec();
        let expected_hash = blaze_ids::b3::hash(VALID_BUNDLE);

        let mut inner: Script<'_, RemoteCacheReq, ScriptRemoteAnswer> = Script::new_in(alloc);
        inner.expect_answer(
            RemoteCacheReq::HeadCheck {
                namespace_hash: ORG_NS,
                key: KEY,
                auth_token_hash: AUTH_TOKEN_HASH,
            },
            ScriptRemoteAnswer(RemoteCacheAnswer::HeadCheck(HeadCheckAnswer::Found {
                bytes_len: VALID_BUNDLE.len() as u64,
            })),
        );
        inner.expect_answer(
            RemoteCacheReq::DownloadBundle {
                namespace_hash: ORG_NS,
                key: KEY,
                auth_token_hash: AUTH_TOKEN_HASH,
                max_bytes: 1024,
                timeout_ns: spec.timeout_ns,
            },
            ScriptRemoteAnswer(RemoteCacheAnswer::DownloadBundle(DownloadBundleAnswer::Ok {
                namespace_hash: ORG_NS,
                bytes: DownloadBytes::Borrowed(VALID_BUNDLE),
            })),
        );
        inner.expect_answer(
            RemoteCacheReq::CommitLocal {
                namespace_hash: ORG_NS,
                key: KEY,
                content_hash: expected_hash,
                bytes_len: VALID_BUNDLE.len() as u64,
            },
            ScriptRemoteAnswer(RemoteCacheAnswer::CommitLocal(Ok(()))),
        );

        // Drive through `RecordHandler`, verify transcript, and replay via `into_script`.
        let mut recorder = RecordHandler::new_in(&mut inner, alloc);
        let live_res =
            drive(&mut recorder, &mut (), RemoteCacheMachine::new(spec), DriveOpts { max_steps: 8 });
        assert_eq!(live_res, Ok(RemoteCacheOutcome::Pulled { bytes_len: VALID_BUNDLE.len() as u64 }));
        assert_eq!(recorder.entries().len(), 3);
        assert!(matches!(
            recorder.entries()[0],
            TranscriptEntry::Answered { req: RemoteCacheReq::HeadCheck { .. }, .. }
        ));
        assert!(matches!(
            recorder.entries()[2],
            TranscriptEntry::Answered { req: RemoteCacheReq::CommitLocal { .. }, .. }
        ));

        let mut replay = recorder.into_script(alloc);
        assert_eq!(inner.finish(), Ok(()));

        let replay_res =
            drive(&mut replay, &mut (), RemoteCacheMachine::new(spec), DriveOpts { max_steps: 8 });
        assert_eq!(
            replay_res,
            Ok(RemoteCacheOutcome::Pulled { bytes_len: VALID_BUNDLE.len() as u64 })
        );
        assert_eq!(replay.finish(), Ok(()));
    }

    #[test]
    fn remote_cache_pull_miss_skips_download_and_commit() {
        let alloc = blaze_own::Alloc::global();
        let spec = pull_spec();

        let mut script: Script<'_, RemoteCacheReq, ScriptRemoteAnswer> = Script::new_in(alloc);
        script.expect_answer(
            RemoteCacheReq::HeadCheck {
                namespace_hash: ORG_NS,
                key: KEY,
                auth_token_hash: AUTH_TOKEN_HASH,
            },
            ScriptRemoteAnswer(RemoteCacheAnswer::HeadCheck(HeadCheckAnswer::Absent)),
        );

        let res =
            drive(&mut script, &mut (), RemoteCacheMachine::new(spec), DriveOpts { max_steps: 8 });
        assert_eq!(res, Ok(RemoteCacheOutcome::Miss));
        assert_eq!(script.finish(), Ok(()));
    }

    #[test]
    fn remote_cache_pull_corrupt_payload_refused_before_commit_and_planted_control_caught() {
        let alloc = blaze_own::Alloc::global();
        let spec = pull_spec();
        let expected_hash = blaze_ids::b3::hash(VALID_BUNDLE);
        let corrupt_hash = blaze_ids::b3::hash(CORRUPT_BUNDLE);

        // Normal machine: refuses corrupt payload with `HashMismatch` BEFORE `CommitLocal`.
        let mut script: Script<'_, RemoteCacheReq, ScriptRemoteAnswer> = Script::new_in(alloc);
        script.expect_answer(
            RemoteCacheReq::HeadCheck {
                namespace_hash: ORG_NS,
                key: KEY,
                auth_token_hash: AUTH_TOKEN_HASH,
            },
            ScriptRemoteAnswer(RemoteCacheAnswer::HeadCheck(HeadCheckAnswer::Found {
                bytes_len: CORRUPT_BUNDLE.len() as u64,
            })),
        );
        script.expect_answer(
            RemoteCacheReq::DownloadBundle {
                namespace_hash: ORG_NS,
                key: KEY,
                auth_token_hash: AUTH_TOKEN_HASH,
                max_bytes: 1024,
                timeout_ns: spec.timeout_ns,
            },
            ScriptRemoteAnswer(RemoteCacheAnswer::DownloadBundle(DownloadBundleAnswer::Ok {
                namespace_hash: ORG_NS,
                bytes: DownloadBytes::Borrowed(CORRUPT_BUNDLE),
            })),
        );

        let res = drive(
            &mut script,
            &mut (),
            RemoteCacheMachine::with_plant(spec, RemoteMachinePlant::None),
            DriveOpts { max_steps: 8 },
        );
        assert_eq!(
            res,
            Err(DriveError::Machine(RemoteCacheError::HashMismatch {
                expected: expected_hash,
                actual: corrupt_hash,
            }))
        );
        assert_eq!(script.finish(), Ok(()));

        // Planted-bug control: `AcceptUnverifiedBundle` attempts `CommitLocal` on corrupt bytes,
        // caught immediately by `Script` as an unexpected step!
        let mut planted_script: Script<'_, RemoteCacheReq, ScriptRemoteAnswer> =
            Script::new_in(alloc);
        planted_script.expect_answer(
            RemoteCacheReq::HeadCheck {
                namespace_hash: ORG_NS,
                key: KEY,
                auth_token_hash: AUTH_TOKEN_HASH,
            },
            ScriptRemoteAnswer(RemoteCacheAnswer::HeadCheck(HeadCheckAnswer::Found {
                bytes_len: CORRUPT_BUNDLE.len() as u64,
            })),
        );
        planted_script.expect_answer(
            RemoteCacheReq::DownloadBundle {
                namespace_hash: ORG_NS,
                key: KEY,
                auth_token_hash: AUTH_TOKEN_HASH,
                max_bytes: 1024,
                timeout_ns: spec.timeout_ns,
            },
            ScriptRemoteAnswer(RemoteCacheAnswer::DownloadBundle(DownloadBundleAnswer::Ok {
                namespace_hash: ORG_NS,
                bytes: DownloadBytes::Borrowed(CORRUPT_BUNDLE),
            })),
        );

        let planted_res = drive(
            &mut planted_script,
            &mut (),
            RemoteCacheMachine::with_plant(spec, RemoteMachinePlant::AcceptUnverifiedBundle),
            DriveOpts { max_steps: 8 },
        );
        assert_eq!(
            planted_res,
            Err(DriveError::Handler(ScriptError::UnexpectedRequest {
                step: 2,
                actual: RemoteCacheReq::CommitLocal {
                    namespace_hash: ORG_NS,
                    key: KEY,
                    content_hash: corrupt_hash,
                    bytes_len: CORRUPT_BUNDLE.len() as u64,
                },
            }))
        );
    }

    #[test]
    fn remote_cache_namespace_isolation_refuses_cross_namespace_and_local_keys() {
        let alloc = blaze_own::Alloc::global();

        // 1. Local-only private namespace is refused before any network request.
        let mut local_spec = push_spec();
        local_spec.record_namespace_hash = LOCAL_NAMESPACE_HASH;
        let mut empty_script: Script<'_, RemoteCacheReq, ScriptRemoteAnswer> =
            Script::new_in(alloc);
        let local_res = drive(
            &mut empty_script,
            &mut (),
            RemoteCacheMachine::new(local_spec),
            DriveOpts { max_steps: 4 },
        );
        assert_eq!(local_res, Err(DriveError::Machine(RemoteCacheError::LocalNamespaceForbidden)));
        assert_eq!(empty_script.finish(), Ok(()));

        // 2. Cross-namespace push (`OTHER_NS` record into `ORG_NS` bucket) is refused before `HeadCheck`.
        let mut cross_spec = push_spec();
        cross_spec.record_namespace_hash = OTHER_NS;
        let mut cross_script: Script<'_, RemoteCacheReq, ScriptRemoteAnswer> =
            Script::new_in(alloc);
        let cross_res = drive(
            &mut cross_script,
            &mut (),
            RemoteCacheMachine::new(cross_spec),
            DriveOpts { max_steps: 4 },
        );
        assert_eq!(
            cross_res,
            Err(DriveError::Machine(RemoteCacheError::NamespaceMismatch {
                expected: ORG_NS,
                actual: OTHER_NS,
            }))
        );
        assert_eq!(cross_script.finish(), Ok(()));

        // 3. Downloaded bundle tagged with another namespace is refused before `CommitLocal`.
        let pull = pull_spec();
        let mut rx_script: Script<'_, RemoteCacheReq, ScriptRemoteAnswer> = Script::new_in(alloc);
        rx_script.expect_answer(
            RemoteCacheReq::HeadCheck {
                namespace_hash: ORG_NS,
                key: KEY,
                auth_token_hash: AUTH_TOKEN_HASH,
            },
            ScriptRemoteAnswer(RemoteCacheAnswer::HeadCheck(HeadCheckAnswer::Found {
                bytes_len: VALID_BUNDLE.len() as u64,
            })),
        );
        rx_script.expect_answer(
            RemoteCacheReq::DownloadBundle {
                namespace_hash: ORG_NS,
                key: KEY,
                auth_token_hash: AUTH_TOKEN_HASH,
                max_bytes: 1024,
                timeout_ns: pull.timeout_ns,
            },
            ScriptRemoteAnswer(RemoteCacheAnswer::DownloadBundle(DownloadBundleAnswer::Ok {
                namespace_hash: OTHER_NS,
                bytes: DownloadBytes::Borrowed(VALID_BUNDLE),
            })),
        );
        let rx_res =
            drive(&mut rx_script, &mut (), RemoteCacheMachine::new(pull), DriveOpts { max_steps: 8 });
        assert_eq!(
            rx_res,
            Err(DriveError::Machine(RemoteCacheError::NamespaceMismatch {
                expected: ORG_NS,
                actual: OTHER_NS,
            }))
        );
        assert_eq!(rx_script.finish(), Ok(()));
    }

    #[test]
    fn remote_cache_push_deduplicates_when_found_and_uploads_once_when_absent() {
        let alloc = blaze_own::Alloc::global();
        let spec = push_spec();
        let content_hash = blaze_ids::b3::hash(VALID_BUNDLE);

        // 1. `HeadCheck(Found)` -> skips `UploadBundle`, completes with `AlreadyPresent`.
        let mut dedup_script: Script<'_, RemoteCacheReq, ScriptRemoteAnswer> =
            Script::new_in(alloc);
        dedup_script.expect_answer(
            RemoteCacheReq::HeadCheck {
                namespace_hash: ORG_NS,
                key: KEY,
                auth_token_hash: AUTH_TOKEN_HASH,
            },
            ScriptRemoteAnswer(RemoteCacheAnswer::HeadCheck(HeadCheckAnswer::Found {
                bytes_len: VALID_BUNDLE.len() as u64,
            })),
        );
        let dedup_res = drive(
            &mut dedup_script,
            &mut (),
            RemoteCacheMachine::new(spec),
            DriveOpts { max_steps: 8 },
        );
        assert_eq!(dedup_res, Ok(RemoteCacheOutcome::AlreadyPresent));
        assert_eq!(dedup_script.finish(), Ok(()));

        // 2. `HeadCheck(Absent)` -> emits `UploadBundle` once, completes with `Pushed`.
        let mut upload_script: Script<'_, RemoteCacheReq, ScriptRemoteAnswer> =
            Script::new_in(alloc);
        upload_script.expect_answer(
            RemoteCacheReq::HeadCheck {
                namespace_hash: ORG_NS,
                key: KEY,
                auth_token_hash: AUTH_TOKEN_HASH,
            },
            ScriptRemoteAnswer(RemoteCacheAnswer::HeadCheck(HeadCheckAnswer::Absent)),
        );
        upload_script.expect_answer(
            RemoteCacheReq::UploadBundle {
                namespace_hash: ORG_NS,
                key: KEY,
                auth_token_hash: AUTH_TOKEN_HASH,
                content_hash,
                bytes_len: VALID_BUNDLE.len() as u64,
                timeout_ns: spec.timeout_ns,
            },
            ScriptRemoteAnswer(RemoteCacheAnswer::UploadBundle(Ok(()))),
        );
        let upload_res = drive(
            &mut upload_script,
            &mut (),
            RemoteCacheMachine::new(spec),
            DriveOpts { max_steps: 8 },
        );
        assert_eq!(
            upload_res,
            Ok(RemoteCacheOutcome::Pushed { bytes_len: VALID_BUNDLE.len() as u64 })
        );
        assert_eq!(upload_script.finish(), Ok(()));
    }

    #[test]
    fn remote_cache_graph_verifies_and_emits_lean() {
        let graph = remote_cache_graph();
        assert_eq!(verify_graph(&graph), Ok(()));

        let mut lean = FixedString::<4096>::new();
        assert_eq!(emit_lean(&graph, &mut lean), Ok(()));
        let text = lean.text().expect("Lean output fits in 4096 bytes");
        assert!(text.contains("namespace RemoteCache"), "missing namespace:\n{text}");
        assert!(text.contains("inductive State where"), "missing State:\n{text}");
        assert!(
            text.contains("theorem verified_machine_no_leak"),
            "missing verified_machine_no_leak:\n{text}"
        );
        assert!(text.contains("end RemoteCache"), "missing end namespace:\n{text}");
    }
}
