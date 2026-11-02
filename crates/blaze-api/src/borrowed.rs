//! A response read where it lies in its frame ([`ResponseRef`]): text and byte strings borrowed
//! from the frame's bytes and lists decoded as they are iterated ([`List`]), so the responses a
//! build streams (diagnostics, units, fetches, build scripts, test results, a run's output, its
//! end) read with no allocation. The rest come once a request at most and are read owned, as
//! [`Response`]'s are.
//!
//! The same bytes read as a [`ResponseRef`] and as a [`Response`] give the same value
//! ([`ResponseRef::into_owned`]) or are both refused, under the same limits: the frame's length
//! ([`crate::LONGEST_REQUEST`], [`crate::frames::Frames`]'s) and the decoder's depth. Each
//! field is declared in its key's order, as [`Response`]'s are. CBOR only: JSON lends nothing.

use blaze_ids::ContentId;
use blaze_protocol::cbor::List;
use blaze_query::{QueryKey, Why};
use blaze_vfs::Change;
use serde::Deserialize;

use crate::{
    ApiError, Applicability, Artifact, Basis, BuildScriptRan, Cached, CrateType, Diagnostic,
    Documented, Edited, FactRead, FetchVerb, Hot, Level, LiveFrame, Memory, Mismatch, Ranked, Reach, Response, Span, Status,
    Table, TargetKind, TestOutcome, TestResult, Traced, UnitOf, UnitStarted, UnitTarget, Verb,
    Worked,
};

/// [`Response`], read where it lies.
#[derive(Deserialize, Clone, Debug)]
pub enum ResponseRef<'a> {
    Committed { root: ContentId, rebased: bool },
    Diagnostic(#[serde(borrow)] DiagnosticRef<'a>),
    Built { members: u32, profile: String, artifacts: Vec<Artifact> },
    Unit(#[serde(borrow)] UnitStartedRef<'a>),
    UnitEnded(#[serde(borrow)] UnitOfRef<'a>),
    Fetched { verb: FetchVerb, what: &'a str },
    BuildScript(#[serde(borrow)] BuildScriptRanRef<'a>),
    Work(Worked),
    TestResult(#[serde(borrow)] TestResultRef<'a>),
    Ran {
        code: Option<i32>,
        #[serde(deserialize_with = "blaze_protocol::bytes::borrow")]
        stderr: &'a [u8],
        #[serde(deserialize_with = "blaze_protocol::bytes::borrow")]
        stdout: &'a [u8],
    },
    Diff { changes: Vec<Change> },
    Why(Why),
    Status(Status),
    Done { ok: bool },
    Error(#[serde(borrow)] ApiErrorRef<'a>),
    Opened { root: ContentId },
    Reached(Reach),
    Of {
        #[serde(borrow)]
        response: Box<ResponseRef<'a>>,
        candidate: u32,
    },
    Ranked(Vec<Ranked>),
    Verified { mismatches: Vec<Mismatch> },
    Snapshotted { heads: Vec<(String, ContentId)> },
    CodegenInputs(Vec<(ContentId, ContentId)>),
    Trace(Vec<Traced>),
    Tables(Vec<Table>),
    Edited(Edited),
    Announced { root: ContentId },
    Memory(Memory),
    Packed {
        #[serde(deserialize_with = "blaze_protocol::bytes::borrow")]
        pack: &'a [u8],
    },
    DocPage {
        #[serde(deserialize_with = "blaze_protocol::bytes::borrow")]
        page: &'a [u8],
        path: &'a str,
    },
    Documented(Documented),
    Explored { link: &'a str },
    Profiled {
        #[serde(deserialize_with = "blaze_protocol::bytes::borrow")]
        trace: &'a [u8],
    },
    Live(#[serde(borrow)] LiveFrameRef<'a>),
    Queried {
        #[serde(deserialize_with = "blaze_protocol::bytes::borrow")]
        ipc: &'a [u8],
        rows: u64,
    },
    Hot(Hot),
}

/// [`LiveFrame`], read where it lies: a frame every 250 ms decodes with no copy of its tables.
#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub struct LiveFrameRef<'a> {
    pub lost: u64,
    #[serde(deserialize_with = "blaze_protocol::bytes::borrow")]
    pub open: &'a [u8],
    pub at_ns: i64,
    #[serde(deserialize_with = "blaze_protocol::bytes::borrow")]
    pub spans: &'a [u8],
    pub skipped: u64,
    #[serde(deserialize_with = "blaze_protocol::bytes::borrow")]
    pub counters: &'a [u8],
}

impl LiveFrameRef<'_> {
    pub fn into_owned(self) -> LiveFrame {
        LiveFrame {
            lost: self.lost,
            open: bytes(self.open),
            at_ns: self.at_ns,
            spans: bytes(self.spans),
            skipped: self.skipped,
            counters: bytes(self.counters),
        }
    }
}

/// [`Diagnostic`], read where it lies.
#[derive(Deserialize, Clone, Copy, Debug)]
pub struct DiagnosticRef<'a> {
    #[serde(borrow)]
    pub code: Option<&'a str>,
    #[serde(borrow)]
    pub unit: Option<UnitOfRef<'a>>,
    #[serde(borrow)]
    pub facts: List<'a, FactReadRef<'a>>,
    pub level: Level,
    #[serde(borrow)]
    pub spans: List<'a, SpanRef<'a>>,
    pub message: &'a str,
    #[serde(borrow)]
    pub children: List<'a, DiagnosticRef<'a>>,
    pub rendered: &'a str,
    #[serde(borrow)]
    pub explanation: Option<&'a str>,
}

/// [`UnitOf`], read where it lies.
#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub struct UnitOfRef<'a> {
    pub id: u32,
    pub test: bool,
    #[serde(borrow)]
    pub target: UnitTargetRef<'a>,
    pub package_id: &'a str,
    pub manifest_path: &'a str,
}

/// [`UnitStarted`], read where it lies.
#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub struct UnitStartedRef<'a> {
    pub name: &'a str,
    pub verb: Verb,
    #[serde(borrow)]
    pub source: Option<&'a str>,
    pub package: u32,
    pub version: &'a str,
    pub package_id: &'a str,
}

/// [`UnitTarget`], read where it lies.
#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub struct UnitTargetRef<'a> {
    pub doc: bool,
    pub kind: TargetKind,
    pub name: &'a str,
    pub test: bool,
    pub doctest: bool,
    pub edition: &'a str,
    pub src_path: &'a str,
    #[serde(borrow)]
    pub crate_types: List<'a, CrateType>,
}

impl<'a> UnitTargetRef<'a> {
    /// [`UnitTarget::kinds`].
    pub fn kinds(self) -> impl Iterator<Item = &'static str> + 'a {
        let library = self.kind == TargetKind::Lib;
        let types = self.crate_types.iter().take(if library { usize::MAX } else { 0 });
        (!library).then_some(self.kind.name()).into_iter().chain(types.map(CrateType::name))
    }
}

/// [`Span`], read where it lies.
#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub struct SpanRef<'a> {
    pub file: &'a str,
    pub line: u32,
    #[serde(borrow)]
    pub label: Option<&'a str>,
    pub column: u32,
    pub primary: bool,
    pub byte_end: u32,
    pub end_line: u32,
    pub byte_start: u32,
    pub end_column: u32,
    #[serde(borrow)]
    pub replacement: Option<&'a str>,
    pub applicability: Option<Applicability>,
}

/// [`FactRead`], read where it lies.
#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub struct FactReadRef<'a> {
    pub key: QueryKey,
    pub what: &'a str,
}

/// [`BuildScriptRan`], read where it lies.
#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub struct BuildScriptRanRef<'a> {
    #[serde(borrow)]
    pub env: List<'a, (&'a str, &'a str)>,
    #[serde(borrow)]
    pub cfgs: List<'a, &'a str>,
    pub out_dir: &'a str,
    pub package_id: &'a str,
    #[serde(borrow)]
    pub linked_libs: List<'a, &'a str>,
    #[serde(borrow)]
    pub linked_paths: List<'a, &'a str>,
}

/// [`TestResult`], read where it lies.
#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub struct TestResultRef<'a> {
    pub name: &'a str,
    pub binary: &'a str,
    #[serde(borrow)]
    pub cached: Option<CachedRef<'a>>,
    pub millis: u64,
    #[serde(deserialize_with = "blaze_protocol::bytes::borrow")]
    pub output: &'a [u8],
    pub outcome: TestOutcome,
    pub outside: bool,
    pub executable: u32,
}

/// [`Cached`], read where it lies.
#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub struct CachedRef<'a> {
    #[serde(borrow)]
    pub basis: BasisRef<'a>,
    pub millis: u64,
    pub outcome: TestOutcome,
}

/// [`Basis`], read where it lies.
#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum BasisRef<'a> {
    Executed,
    Reached { why: &'a str },
}

/// [`ApiError`], read where it lies.
#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub struct ApiErrorRef<'a> {
    #[serde(borrow)]
    pub at: Option<&'a str>,
    pub what: &'a str,
}

/// `text`, owned: every `into_owned` below allocates only through these four, for a response
/// kept past its frame (another request's, a result held to the end); the path that prints
/// reads in place.
fn owned(text: &str) -> String {
    text.to_owned() // alloc: a response kept past its frame.
}

fn maybe(text: Option<&str>) -> Option<String> {
    text.map(owned)
}

fn bytes(bytes: &[u8]) -> Vec<u8> {
    bytes.to_vec() // alloc: as `owned`'s.
}

fn all<'a, T: Deserialize<'a>, U>(list: List<'a, T>, each: impl FnMut(T) -> U) -> Vec<U> {
    list.iter().map(each).collect() // alloc: as `owned`'s.
}

impl ResponseRef<'_> {
    /// The [`Response`] the same bytes read as.
    pub fn into_owned(self) -> Response {
        match self {
            ResponseRef::Committed { root, rebased } => Response::Committed { root, rebased },
            ResponseRef::Diagnostic(it) => Response::Diagnostic(it.into_owned()),
            ResponseRef::Built { members, profile, artifacts } => Response::Built { members, profile, artifacts },
            ResponseRef::Unit(it) => Response::Unit(it.into_owned()),
            ResponseRef::UnitEnded(it) => Response::UnitEnded(it.into_owned()),
            ResponseRef::Fetched { verb, what } => Response::Fetched { verb, what: owned(what) },
            ResponseRef::BuildScript(it) => Response::BuildScript(it.into_owned()),
            ResponseRef::Work(it) => Response::Work(it),
            ResponseRef::TestResult(it) => Response::TestResult(it.into_owned()),
            ResponseRef::Ran { code, stderr, stdout } => {
                Response::Ran { code, stderr: bytes(stderr), stdout: bytes(stdout) }
            }
            ResponseRef::Diff { changes } => Response::Diff { changes },
            ResponseRef::Why(it) => Response::Why(it),
            ResponseRef::Status(it) => Response::Status(it),
            ResponseRef::Done { ok } => Response::Done { ok },
            ResponseRef::Error(it) => Response::Error(it.into_owned()),
            ResponseRef::Opened { root } => Response::Opened { root },
            ResponseRef::Reached(it) => Response::Reached(it),
            ResponseRef::Of { response, candidate } => {
                // alloc: as `owned`'s; a best-of's response is boxed owned too.
                Response::Of { response: Box::new((*response).into_owned()), candidate }
            }
            ResponseRef::Ranked(it) => Response::Ranked(it),
            ResponseRef::Verified { mismatches } => Response::Verified { mismatches },
            ResponseRef::Snapshotted { heads } => Response::Snapshotted { heads },
            ResponseRef::CodegenInputs(it) => Response::CodegenInputs(it),
            ResponseRef::Trace(it) => Response::Trace(it),
            ResponseRef::Tables(it) => Response::Tables(it),
            ResponseRef::Edited(it) => Response::Edited(it),
            ResponseRef::Announced { root } => Response::Announced { root },
            ResponseRef::Memory(it) => Response::Memory(it),
            ResponseRef::Packed { pack } => Response::Packed { pack: bytes(pack) },
            ResponseRef::DocPage { path, page } => Response::DocPage { path: owned(path), page: bytes(page) },
            ResponseRef::Documented(it) => Response::Documented(it),
            ResponseRef::Explored { link } => Response::Explored { link: owned(link) },
            ResponseRef::Profiled { trace } => Response::Profiled { trace: bytes(trace) },
            ResponseRef::Live(it) => Response::Live(it.into_owned()),
            ResponseRef::Queried { ipc, rows } => Response::Queried { ipc: bytes(ipc), rows },
            ResponseRef::Hot(it) => Response::Hot(it),
        }
    }
}

impl DiagnosticRef<'_> {
    pub fn into_owned(self) -> Diagnostic {
        Diagnostic {
            code: maybe(self.code),
            unit: self.unit.map(UnitOfRef::into_owned),
            facts: all(self.facts, FactReadRef::into_owned),
            level: self.level,
            spans: all(self.spans, SpanRef::into_owned),
            message: owned(self.message),
            children: all(self.children, DiagnosticRef::into_owned),
            rendered: owned(self.rendered),
            explanation: maybe(self.explanation),
        }
    }
}

impl UnitOfRef<'_> {
    pub fn into_owned(self) -> UnitOf {
        UnitOf {
            id: self.id,
            test: self.test,
            target: self.target.into_owned(),
            package_id: owned(self.package_id),
            manifest_path: owned(self.manifest_path),
        }
    }
}

impl UnitStartedRef<'_> {
    pub fn into_owned(self) -> UnitStarted {
        UnitStarted {
            name: owned(self.name),
            verb: self.verb,
            source: maybe(self.source),
            package: self.package,
            version: owned(self.version),
            package_id: owned(self.package_id),
        }
    }
}

impl UnitTargetRef<'_> {
    pub fn into_owned(self) -> UnitTarget {
        UnitTarget {
            doc: self.doc,
            kind: self.kind,
            name: owned(self.name),
            test: self.test,
            doctest: self.doctest,
            edition: owned(self.edition),
            src_path: owned(self.src_path),
            crate_types: all(self.crate_types, |it| it),
        }
    }
}

impl SpanRef<'_> {
    pub fn into_owned(self) -> Span {
        Span {
            file: owned(self.file),
            line: self.line,
            label: maybe(self.label),
            column: self.column,
            primary: self.primary,
            byte_end: self.byte_end,
            end_line: self.end_line,
            byte_start: self.byte_start,
            end_column: self.end_column,
            replacement: maybe(self.replacement),
            applicability: self.applicability,
        }
    }
}

impl FactReadRef<'_> {
    pub fn into_owned(self) -> FactRead {
        FactRead { key: self.key, what: owned(self.what) }
    }
}

impl BuildScriptRanRef<'_> {
    pub fn into_owned(self) -> BuildScriptRan {
        BuildScriptRan {
            env: all(self.env, |(name, value)| (owned(name), owned(value))),
            cfgs: all(self.cfgs, owned),
            out_dir: owned(self.out_dir),
            package_id: owned(self.package_id),
            linked_libs: all(self.linked_libs, owned),
            linked_paths: all(self.linked_paths, owned),
        }
    }
}

impl TestResultRef<'_> {
    pub fn into_owned(self) -> TestResult {
        TestResult {
            name: owned(self.name),
            binary: owned(self.binary),
            cached: self.cached.map(CachedRef::into_owned),
            millis: self.millis,
            output: bytes(self.output),
            outcome: self.outcome,
            outside: self.outside,
            executable: self.executable,
        }
    }
}

impl CachedRef<'_> {
    pub fn into_owned(self) -> Cached {
        let basis = match self.basis {
            BasisRef::Executed => Basis::Executed,
            BasisRef::Reached { why } => Basis::Reached { why: owned(why) },
        };
        Cached { basis, millis: self.millis, outcome: self.outcome }
    }
}

impl ApiErrorRef<'_> {
    pub fn into_owned(self) -> ApiError {
        ApiError { at: maybe(self.at), what: owned(self.what) }
    }
}

#[cfg(test)]
mod tests;
