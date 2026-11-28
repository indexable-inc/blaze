//! Explicit typestate sans-IO machine for fetching, verifying, and storing a single pinned crate
//! archive (`docs/v2/plan/design/sans-io-machines.md`, D172).
//!
//! Trust-step order:
//! 1. [`FetchUnitReq::StoreLookup`] — if the store already holds `key`, finish immediately with
//!    [`FetchUnitOutcome::Cached`] (zero local or network IO).
//! 2. [`FetchUnitReq::LocalArchive`] — untrusted local cache; bytes are checked against both
//!    `max_bytes` and `expected_sha256` before being accepted. A miss, IO error, oversize payload,
//!    or SHA-256 mismatch falls through to HTTPS (the next source in order, never a retry).
//! 3. [`FetchUnitReq::HttpsGet`] — tried ONCE (D172: never retried in the same run). Payload length
//!    is checked against `max_bytes` and its SHA-256 against `expected_sha256` BEFORE emitting
//!    [`FetchUnitReq::StorePut`]; any corruption fails immediately with
//!    [`FetchUnitError::ChecksumMismatch`].
//! 4. [`FetchUnitReq::StorePut`] — stores the verified archive under `key` and completes with
//!    [`FetchUnitOutcome::Fetched`].

#![blaze::sans_io]
#![forbid(unsafe_code)]

use core::fmt;

use blaze_machine::{
    EdgeKind, GraphEdge, HandleOp, Machine, MachineGraph, Request, StateId, StateNode, Turn,
};
use blaze_own::planted::{NoPlant, Planted, Plants};
use blaze_own::{FixedString, Full};

/// Planted-bug switches for [`FetchUnitMachine`] controls (rules 43 & 53).
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum FetchMachinePlant {
    #[default]
    None,
    /// Planted bug: skip SHA-256 verification on the HTTPS payload before emitting `StorePut`.
    SkipSha256Check,
}

impl Plants for FetchMachinePlant {}

/// Typed store failure reported by `StoreLookup` or `StorePut`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FetchStoreError {
    Io,
    DiskFull,
    Corrupt,
}

impl fmt::Display for FetchStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FetchStoreError::Io => f.write_str("store IO error"),
            FetchStoreError::DiskFull => f.write_str("store disk full"),
            FetchStoreError::Corrupt => f.write_str("store record corrupt"),
        }
    }
}

/// Typed HTTPS transport failure reported by `HttpsGet` (D172: terminal for this run, never
/// retried).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HttpsError {
    Connect,
    Tls,
    Reset,
    Timeout,
    HttpStatus(u16),
}

impl fmt::Display for HttpsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HttpsError::Connect => f.write_str("HTTPS connection failed"),
            HttpsError::Tls => f.write_str("HTTPS TLS handshake failed"),
            HttpsError::Reset => f.write_str("HTTPS connection reset"),
            HttpsError::Timeout => f.write_str("HTTPS request timed out"),
            HttpsError::HttpStatus(code) => write!(f, "HTTPS status {code}"),
        }
    }
}

/// Typed `#![no_std]`, zero-heap requests emitted by [`FetchUnitMachine`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FetchUnitReq {
    /// Checks whether the local content-addressed store already holds `key`.
    StoreLookup { key: [u8; 32] },
    /// Looks up an untrusted local `.crate` archive by crate name and version.
    LocalArchive { name: FixedString<64>, version: FixedString<32> },
    /// Fetches the `.crate` archive once over HTTPS with an explicit byte cap and deadline (D172).
    HttpsGet { url_hash: u64, max_bytes: u64, timeout_ns: u64 },
    /// Stores a verified archive of `bytes_len` bytes under `key`.
    StorePut { key: [u8; 32], bytes_len: u64 },
}

/// Answer to [`FetchUnitReq::StoreLookup`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StoreLookupAnswer {
    Hit,
    Miss,
    Err(FetchStoreError),
}

/// Answer to [`FetchUnitReq::LocalArchive`], borrowing driver-owned bytes for one step.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LocalArchiveAnswer<'s> {
    Found(&'s [u8]),
    NotFound,
    IoError,
}

/// Answer to [`FetchUnitReq::HttpsGet`], borrowing driver-owned bytes for one step.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HttpsGetAnswer<'s> {
    Ok(&'s [u8]),
    Err(HttpsError),
}

/// Typed answer family paired with [`FetchUnitReq`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FetchUnitAnswer<'s> {
    StoreLookup(StoreLookupAnswer),
    LocalArchive(LocalArchiveAnswer<'s>),
    HttpsGet(HttpsGetAnswer<'s>),
    StorePut(Result<(), FetchStoreError>),
}

impl Request for FetchUnitReq {
    type Answer<'s> = FetchUnitAnswer<'s>;
}

/// Wrapper allowing `&'static [u8]` answers in [`blaze_machine::Script`] tests to convert cleanly
/// into `FetchUnitAnswer<'s>` for any driver borrow lifetime `'s`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ScriptFetchAnswer(pub FetchUnitAnswer<'static>);

impl<'s> From<ScriptFetchAnswer> for FetchUnitAnswer<'s> {
    #[inline]
    fn from(value: ScriptFetchAnswer) -> Self {
        value.0
    }
}

/// Terminal success outcome of [`FetchUnitMachine`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FetchUnitOutcome {
    /// Store already held `key`; neither local archive nor HTTPS was touched.
    Cached,
    /// Verified archive of `bytes_len` bytes was stored under `key`.
    Fetched { bytes_len: u64 },
}

/// Terminal failure outcome of [`FetchUnitMachine`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FetchUnitError {
    Store(FetchStoreError),
    Https(HttpsError),
    PayloadTooLarge { actual: u64, max: u64 },
    ChecksumMismatch { expected: [u8; 32], actual: [u8; 32] },
}

impl fmt::Display for FetchUnitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FetchUnitError::Store(e) => write!(f, "fetch store error: {e}"),
            FetchUnitError::Https(e) => write!(f, "fetch HTTPS error: {e}"),
            FetchUnitError::PayloadTooLarge { actual, max } => {
                write!(f, "archive length {actual} exceeds limit {max}")
            }
            FetchUnitError::ChecksumMismatch { .. } => {
                f.write_str("archive SHA-256 did not match lock checksum")
            }
        }
    }
}

/// Pinned crate specification given to [`FetchUnitMachine`].
#[blaze::args]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FetchUnitSpec {
    pub key: [u8; 32],
    pub name: FixedString<64>,
    pub version: FixedString<32>,
    pub url_hash: u64,
    pub expected_sha256: [u8; 32],
    pub max_bytes: u64,
    pub timeout_ns: u64,
}

/// Helper for building a bounded inline [`FixedString`] with zero heap allocation.
#[inline]
pub fn fixed_str<const N: usize>(text: &str) -> Result<FixedString<N>, Full<&str>> {
    let mut out = FixedString::new();
    out.try_push_str(text)?;
    Ok(out)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Init,
    CheckingStore,
    CheckingLocal,
    FetchingHttps,
    Storing { bytes_len: u64 },
}

/// Explicit typestate sans-IO machine for fetching, verifying, and storing one pinned crate
/// archive (`#![no_std]`, zero global heap, D172 single-try HTTPS).
#[derive(Clone, Copy, Debug)]
pub struct FetchUnitMachine<P: Planted<FetchMachinePlant> = NoPlant> {
    spec: FetchUnitSpec,
    state: State,
    plant: P,
}

impl FetchUnitMachine<NoPlant> {
    /// Creates a production [`FetchUnitMachine`] with zero planted bugs.
    #[inline]
    pub const fn new(spec: FetchUnitSpec) -> Self {
        Self { spec, state: State::Init, plant: NoPlant }
    }
}

impl<P: Planted<FetchMachinePlant>> FetchUnitMachine<P> {
    /// Creates a [`FetchUnitMachine`] carrying a planted-bug seam for controls.
    #[inline]
    pub const fn with_plant(spec: FetchUnitSpec, plant: P) -> Self {
        Self { spec, state: State::Init, plant }
    }
}

impl<P: Planted<FetchMachinePlant>> Machine<()> for FetchUnitMachine<P> {
    type Req = FetchUnitReq;
    type Out = FetchUnitOutcome;
    type Err = FetchUnitError;

    fn start(self, _cx: &mut ()) -> Turn<Self, Self::Req, Self::Out, Self::Err> {
        let Self { spec, state, plant } = self;
        match state {
            State::Init => Turn::Ask {
                request: FetchUnitReq::StoreLookup { key: spec.key },
                next: Self { spec, state: State::CheckingStore, plant },
            },
            State::CheckingStore
            | State::CheckingLocal
            | State::FetchingHttps
            | State::Storing { .. } => {
                unreachable!("FetchUnitMachine::start called on non-Init state")
            }
        }
    }

    fn resume(
        self,
        answer: FetchUnitAnswer<'_>,
        _cx: &mut (),
    ) -> Turn<Self, Self::Req, Self::Out, Self::Err> {
        let Self { spec, state, plant } = self;
        match (state, answer) {
            (State::CheckingStore, FetchUnitAnswer::StoreLookup(StoreLookupAnswer::Hit)) => {
                Turn::Done(FetchUnitOutcome::Cached)
            }
            (State::CheckingStore, FetchUnitAnswer::StoreLookup(StoreLookupAnswer::Miss)) => {
                Turn::Ask {
                    request: FetchUnitReq::LocalArchive {
                        name: spec.name,
                        version: spec.version,
                    },
                    next: Self { spec, state: State::CheckingLocal, plant },
                }
            }
            (State::CheckingStore, FetchUnitAnswer::StoreLookup(StoreLookupAnswer::Err(err)))
            | (State::Storing { .. }, FetchUnitAnswer::StorePut(Err(err))) => {
                Turn::Fail(FetchUnitError::Store(err))
            }
            (State::CheckingLocal, FetchUnitAnswer::LocalArchive(LocalArchiveAnswer::Found(bytes))) => {
                let bytes_len = bytes.len() as u64;
                // Local cache is untrusted: accept only if within `max_bytes` AND SHA-256 matches.
                if bytes_len <= spec.max_bytes
                    && blaze_sha256::digest(bytes) == spec.expected_sha256
                {
                    Turn::Ask {
                        request: FetchUnitReq::StorePut { key: spec.key, bytes_len },
                        next: Self { spec, state: State::Storing { bytes_len }, plant },
                    }
                } else {
                    Turn::Ask {
                        request: FetchUnitReq::HttpsGet {
                            url_hash: spec.url_hash,
                            max_bytes: spec.max_bytes,
                            timeout_ns: spec.timeout_ns,
                        },
                        next: Self { spec, state: State::FetchingHttps, plant },
                    }
                }
            }
            (
                State::CheckingLocal,
                FetchUnitAnswer::LocalArchive(
                    LocalArchiveAnswer::NotFound | LocalArchiveAnswer::IoError,
                ),
            ) => Turn::Ask {
                request: FetchUnitReq::HttpsGet {
                    url_hash: spec.url_hash,
                    max_bytes: spec.max_bytes,
                    timeout_ns: spec.timeout_ns,
                },
                next: Self { spec, state: State::FetchingHttps, plant },
            },
            (State::FetchingHttps, FetchUnitAnswer::HttpsGet(HttpsGetAnswer::Ok(bytes))) => {
                let bytes_len = bytes.len() as u64;
                if bytes_len > spec.max_bytes {
                    return Turn::Fail(FetchUnitError::PayloadTooLarge {
                        actual: bytes_len,
                        max: spec.max_bytes,
                    });
                }
                if !plant.has(FetchMachinePlant::SkipSha256Check) {
                    let actual = blaze_sha256::digest(bytes);
                    if actual != spec.expected_sha256 {
                        return Turn::Fail(FetchUnitError::ChecksumMismatch {
                            expected: spec.expected_sha256,
                            actual,
                        });
                    }
                }
                Turn::Ask {
                    request: FetchUnitReq::StorePut { key: spec.key, bytes_len },
                    next: Self { spec, state: State::Storing { bytes_len }, plant },
                }
            }
            // D172: HTTPS is tried once; any transport error fails the unit immediately.
            (State::FetchingHttps, FetchUnitAnswer::HttpsGet(HttpsGetAnswer::Err(err))) => {
                Turn::Fail(FetchUnitError::Https(err))
            }
            (State::Storing { bytes_len }, FetchUnitAnswer::StorePut(Ok(()))) => {
                Turn::Done(FetchUnitOutcome::Fetched { bytes_len })
            }
            _ => unreachable!("FetchUnitAnswer always matches active waiting state"),
        }
    }
}

/// Reflected state graph of [`FetchUnitMachine`], verified by [`blaze_machine::verify_graph`].
pub fn fetch_unit_graph() -> MachineGraph<'static> {
    const INIT: StateId = StateId(0);
    const CHECKING_STORE: StateId = StateId(1);
    const CHECKING_LOCAL: StateId = StateId(2);
    const FETCHING_HTTPS: StateId = StateId(3);
    const STORING: StateId = StateId(4);

    static STATES: [StateNode; 5] = [
        StateNode { id: INIT, name: "Init", loop_state: false, initial_handles: 0 },
        StateNode {
            id: CHECKING_STORE,
            name: "CheckingStore",
            loop_state: false,
            initial_handles: 0,
        },
        StateNode {
            id: CHECKING_LOCAL,
            name: "CheckingLocal",
            loop_state: false,
            initial_handles: 0,
        },
        StateNode {
            id: FETCHING_HTTPS,
            name: "FetchingHttps",
            loop_state: false,
            initial_handles: 0,
        },
        StateNode { id: STORING, name: "Storing", loop_state: false, initial_handles: 0 },
    ];

    static EDGES: [GraphEdge; 14] = [
        GraphEdge {
            from: INIT,
            kind: EdgeKind::Ask { to: CHECKING_STORE },
            handle_op: HandleOp::None,
            label: "lookup_store",
        },
        GraphEdge {
            from: CHECKING_STORE,
            kind: EdgeKind::Done,
            handle_op: HandleOp::None,
            label: "store_hit",
        },
        GraphEdge {
            from: CHECKING_STORE,
            kind: EdgeKind::Ask { to: CHECKING_LOCAL },
            handle_op: HandleOp::None,
            label: "store_miss_check_local",
        },
        GraphEdge {
            from: CHECKING_STORE,
            kind: EdgeKind::Fail,
            handle_op: HandleOp::None,
            label: "store_lookup_err",
        },
        GraphEdge {
            from: CHECKING_STORE,
            kind: EdgeKind::Abandon,
            handle_op: HandleOp::None,
            label: "store_lookup_cancel",
        },
        GraphEdge {
            from: CHECKING_LOCAL,
            kind: EdgeKind::Ask { to: STORING },
            handle_op: HandleOp::None,
            label: "local_verified_put",
        },
        GraphEdge {
            from: CHECKING_LOCAL,
            kind: EdgeKind::Ask { to: FETCHING_HTTPS },
            handle_op: HandleOp::None,
            label: "local_miss_or_corrupt_https",
        },
        GraphEdge {
            from: CHECKING_LOCAL,
            kind: EdgeKind::Abandon,
            handle_op: HandleOp::None,
            label: "local_cancel",
        },
        GraphEdge {
            from: FETCHING_HTTPS,
            kind: EdgeKind::Ask { to: STORING },
            handle_op: HandleOp::None,
            label: "https_verified_put",
        },
        GraphEdge {
            from: FETCHING_HTTPS,
            kind: EdgeKind::Fail,
            handle_op: HandleOp::None,
            label: "https_failed_or_corrupt",
        },
        GraphEdge {
            from: FETCHING_HTTPS,
            kind: EdgeKind::Abandon,
            handle_op: HandleOp::None,
            label: "https_cancel",
        },
        GraphEdge {
            from: STORING,
            kind: EdgeKind::Done,
            handle_op: HandleOp::None,
            label: "store_put_ok",
        },
        GraphEdge {
            from: STORING,
            kind: EdgeKind::Fail,
            handle_op: HandleOp::None,
            label: "store_put_err",
        },
        GraphEdge {
            from: STORING,
            kind: EdgeKind::Abandon,
            handle_op: HandleOp::None,
            label: "store_put_cancel",
        },
    ];

    MachineGraph {
        name: "FetchUnit",
        entry: INIT,
        grace_max_steps: 1,
        states: &STATES,
        edges: &EDGES,
    }
}

#[cfg(test)]
mod tests {
    use blaze_machine::{
        DriveError, DriveOpts, Script, ScriptError, drive, emit_lean, verify_graph,
    };
    use blaze_own::FixedString;
    use blaze_own::testing::heap;

    use super::{
        FetchMachinePlant, FetchUnitAnswer, FetchUnitError, FetchUnitMachine, FetchUnitOutcome,
        FetchUnitReq, FetchUnitSpec, HttpsError, HttpsGetAnswer, LocalArchiveAnswer,
        ScriptFetchAnswer, StoreLookupAnswer, fetch_unit_graph, fixed_str,
    };

    const VALID_ARCHIVE: &[u8] = b"pinned-crate-archive-payload-v1";
    const CORRUPT_LOCAL: &[u8] = b"tampered-local-cache-bytes";
    const CORRUPT_HTTPS: &[u8] = b"tampered-https-response-bytes";

    fn sample_spec() -> FetchUnitSpec {
        FetchUnitSpec {
            key: [0xAB; 32],
            name: fixed_str::<64>("serde").expect("fits"),
            version: fixed_str::<32>("1.0.229").expect("fits"),
            url_hash: 0x1234_5678_9ABC_DEF0,
            expected_sha256: blaze_sha256::digest(VALID_ARCHIVE),
            max_bytes: 1024,
            timeout_ns: 30_000_000_000,
        }
    }

    #[test]
    fn fetch_unit_cache_hit_skips_local_and_https() {
        let h = heap();
        let spec = sample_spec();

        let mut script: Script<'_, FetchUnitReq, ScriptFetchAnswer> = Script::new_in(h.alloc());
        script.expect_answer(
            FetchUnitReq::StoreLookup { key: spec.key },
            ScriptFetchAnswer(FetchUnitAnswer::StoreLookup(StoreLookupAnswer::Hit)),
        );

        let res = drive(&mut script, &mut (), FetchUnitMachine::new(spec), DriveOpts { max_steps: 8 });
        assert_eq!(res, Ok(FetchUnitOutcome::Cached));
        assert_eq!(script.finish(), Ok(()));
    }

    #[test]
    fn fetch_unit_corrupt_local_falls_through_to_verified_https_and_store_put() {
        let h = heap();
        let spec = sample_spec();

        let mut script: Script<'_, FetchUnitReq, ScriptFetchAnswer> = Script::new_in(h.alloc());
        script.expect_answer(
            FetchUnitReq::StoreLookup { key: spec.key },
            ScriptFetchAnswer(FetchUnitAnswer::StoreLookup(StoreLookupAnswer::Miss)),
        );
        // Untrusted local archive holds corrupt bytes -> must NOT be stored; fall through to HTTPS.
        script.expect_answer(
            FetchUnitReq::LocalArchive { name: spec.name, version: spec.version },
            ScriptFetchAnswer(FetchUnitAnswer::LocalArchive(LocalArchiveAnswer::Found(
                CORRUPT_LOCAL,
            ))),
        );
        script.expect_answer(
            FetchUnitReq::HttpsGet {
                url_hash: spec.url_hash,
                max_bytes: spec.max_bytes,
                timeout_ns: spec.timeout_ns,
            },
            ScriptFetchAnswer(FetchUnitAnswer::HttpsGet(HttpsGetAnswer::Ok(VALID_ARCHIVE))),
        );
        script.expect_answer(
            FetchUnitReq::StorePut { key: spec.key, bytes_len: VALID_ARCHIVE.len() as u64 },
            ScriptFetchAnswer(FetchUnitAnswer::StorePut(Ok(()))),
        );

        let res = drive(&mut script, &mut (), FetchUnitMachine::new(spec), DriveOpts { max_steps: 8 });
        assert_eq!(res, Ok(FetchUnitOutcome::Fetched { bytes_len: VALID_ARCHIVE.len() as u64 }));
        assert_eq!(script.finish(), Ok(()));
    }

    #[test]
    fn fetch_unit_corrupt_https_refused_before_store_put_and_planted_control_caught() {
        let h = heap();
        let spec = sample_spec();
        let corrupt_digest = blaze_sha256::digest(CORRUPT_HTTPS);

        // Normal machine: refuses corrupt HTTPS payload with ChecksumMismatch BEFORE StorePut.
        let mut script: Script<'_, FetchUnitReq, ScriptFetchAnswer> = Script::new_in(h.alloc());
        script.expect_answer(
            FetchUnitReq::StoreLookup { key: spec.key },
            ScriptFetchAnswer(FetchUnitAnswer::StoreLookup(StoreLookupAnswer::Miss)),
        );
        script.expect_answer(
            FetchUnitReq::LocalArchive { name: spec.name, version: spec.version },
            ScriptFetchAnswer(FetchUnitAnswer::LocalArchive(LocalArchiveAnswer::NotFound)),
        );
        script.expect_answer(
            FetchUnitReq::HttpsGet {
                url_hash: spec.url_hash,
                max_bytes: spec.max_bytes,
                timeout_ns: spec.timeout_ns,
            },
            ScriptFetchAnswer(FetchUnitAnswer::HttpsGet(HttpsGetAnswer::Ok(CORRUPT_HTTPS))),
        );

        let res = drive(
            &mut script,
            &mut (),
            FetchUnitMachine::with_plant(spec, FetchMachinePlant::None),
            DriveOpts { max_steps: 8 },
        );
        assert_eq!(
            res,
            Err(DriveError::Machine(FetchUnitError::ChecksumMismatch {
                expected: spec.expected_sha256,
                actual: corrupt_digest,
            }))
        );
        assert_eq!(script.finish(), Ok(()));

        // Planted-bug control: SkipSha256Check attempts StorePut on corrupt bytes, caught by Script!
        let mut planted_script: Script<'_, FetchUnitReq, ScriptFetchAnswer> =
            Script::new_in(h.alloc());
        planted_script.expect_answer(
            FetchUnitReq::StoreLookup { key: spec.key },
            ScriptFetchAnswer(FetchUnitAnswer::StoreLookup(StoreLookupAnswer::Miss)),
        );
        planted_script.expect_answer(
            FetchUnitReq::LocalArchive { name: spec.name, version: spec.version },
            ScriptFetchAnswer(FetchUnitAnswer::LocalArchive(LocalArchiveAnswer::NotFound)),
        );
        planted_script.expect_answer(
            FetchUnitReq::HttpsGet {
                url_hash: spec.url_hash,
                max_bytes: spec.max_bytes,
                timeout_ns: spec.timeout_ns,
            },
            ScriptFetchAnswer(FetchUnitAnswer::HttpsGet(HttpsGetAnswer::Ok(CORRUPT_HTTPS))),
        );

        let planted_res = drive(
            &mut planted_script,
            &mut (),
            FetchUnitMachine::with_plant(spec, FetchMachinePlant::SkipSha256Check),
            DriveOpts { max_steps: 8 },
        );
        assert_eq!(
            planted_res,
            Err(DriveError::Handler(ScriptError::UnexpectedRequest {
                step: 3,
                actual: FetchUnitReq::StorePut {
                    key: spec.key,
                    bytes_len: CORRUPT_HTTPS.len() as u64,
                },
            }))
        );
    }

    #[test]
    fn fetch_unit_https_error_does_not_retry_and_limit_is_enforced() {
        let h = heap();
        let mut spec = sample_spec();

        // D172: single HTTPS error fails immediately without retrying.
        let mut err_script: Script<'_, FetchUnitReq, ScriptFetchAnswer> =
            Script::new_in(h.alloc());
        err_script.expect_answer(
            FetchUnitReq::StoreLookup { key: spec.key },
            ScriptFetchAnswer(FetchUnitAnswer::StoreLookup(StoreLookupAnswer::Miss)),
        );
        err_script.expect_answer(
            FetchUnitReq::LocalArchive { name: spec.name, version: spec.version },
            ScriptFetchAnswer(FetchUnitAnswer::LocalArchive(LocalArchiveAnswer::NotFound)),
        );
        err_script.expect_answer(
            FetchUnitReq::HttpsGet {
                url_hash: spec.url_hash,
                max_bytes: spec.max_bytes,
                timeout_ns: spec.timeout_ns,
            },
            ScriptFetchAnswer(FetchUnitAnswer::HttpsGet(HttpsGetAnswer::Err(HttpsError::Timeout))),
        );

        let res =
            drive(&mut err_script, &mut (), FetchUnitMachine::new(spec), DriveOpts { max_steps: 8 });
        assert_eq!(res, Err(DriveError::Machine(FetchUnitError::Https(HttpsError::Timeout))));
        assert_eq!(err_script.finish(), Ok(()));

        // Payload exceeding max_bytes is refused before hashing or StorePut.
        spec.max_bytes = 8;
        let mut big_script: Script<'_, FetchUnitReq, ScriptFetchAnswer> =
            Script::new_in(h.alloc());
        big_script.expect_answer(
            FetchUnitReq::StoreLookup { key: spec.key },
            ScriptFetchAnswer(FetchUnitAnswer::StoreLookup(StoreLookupAnswer::Miss)),
        );
        big_script.expect_answer(
            FetchUnitReq::LocalArchive { name: spec.name, version: spec.version },
            ScriptFetchAnswer(FetchUnitAnswer::LocalArchive(LocalArchiveAnswer::NotFound)),
        );
        big_script.expect_answer(
            FetchUnitReq::HttpsGet {
                url_hash: spec.url_hash,
                max_bytes: spec.max_bytes,
                timeout_ns: spec.timeout_ns,
            },
            ScriptFetchAnswer(FetchUnitAnswer::HttpsGet(HttpsGetAnswer::Ok(VALID_ARCHIVE))),
        );

        let big_res =
            drive(&mut big_script, &mut (), FetchUnitMachine::new(spec), DriveOpts { max_steps: 8 });
        assert_eq!(
            big_res,
            Err(DriveError::Machine(FetchUnitError::PayloadTooLarge {
                actual: VALID_ARCHIVE.len() as u64,
                max: 8,
            }))
        );
        assert_eq!(big_script.finish(), Ok(()));
    }

    #[test]
    fn fetch_unit_graph_verifies_and_emits_lean() {
        let graph = fetch_unit_graph();
        assert_eq!(verify_graph(&graph), Ok(()));

        let mut lean = FixedString::<4096>::new();
        assert_eq!(emit_lean(&graph, &mut lean), Ok(()));
        let text = lean.text().expect("Lean output fits in 4096 bytes");
        assert!(text.contains("namespace FetchUnit"), "missing namespace:\n{text}");
        assert!(text.contains("theorem verified_machine_no_leak"), "missing theorem:\n{text}");
        assert!(text.contains("end FetchUnit"), "missing end namespace:\n{text}");
    }
}
