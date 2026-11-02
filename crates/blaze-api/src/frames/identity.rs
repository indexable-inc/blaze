//! Every request and response of the API held to [`cbor::reference`], the codec before the
//! one in use: the same bytes from each way an envelope is written, the same value read back
//! by both, the same answer from both for the bytes changed, and nothing of the API's own
//! types put in order on the way out.

use std::fmt::Debug;

use blaze_ids::{ContentId, Fingerprint, rng::SplitMix64};
use blaze_protocol::cbor::{self, reference};
use blaze_query::{Changed, Edge, Input, QueryKey, QueryKind, Why};
use blaze_vfs::{Change, ChangeKind, Mode, Op, View};
use serde::{Serialize, de::DeserializeOwned};

use super::{decode, encode, encode_into};
use crate::{
    ApiError, Applicability, Artifact, ArtifactKind, Basis, BuildScriptRan, Cached, Cargo,
    CrateType, Diagnostic, Documented, DocumentedCrate, Edit, Edited, EditedItem, Envelope, FactRead, FetchVerb, How, Level,
    LinkHow, LinkKind, Linked, LiveFrame, Memory, Mismatch, NodeKind, Ranked, Reach, ReachedItem, Request, Response, ResponseRef, Span,
    ServedCrate, ServedPart, Status, Table, TagMemory, Target, TargetKind, Targets, TestOutcome,
    TestResult, Traced, Ub,
    UnitOf, UnitProfile, UnitStarted, UnitTarget, Verb, ViewRef, Worked,
};

const REQUESTS: usize = 30;
const RESPONSES: usize = 35;

const CHARS: [char; 9] = ['a', 'Z', '0', ' ', '-', '/', '\u{e9}', '\u{6c34}', '\u{1f525}'];

/// Values of the API from a seed, the same in every run. `plain` leaves out what holds a map
/// of another crate's type whose fields are not declared in their keys' order (`Op`,
/// `Change`, `Edge`, `Why`), which the writer puts in order.
struct Gen {
    rng: SplitMix64,
    plain: bool,
}

impl Gen {
    fn next(&mut self) -> u64 {
        self.rng.next_u64()
    }

    fn below(&mut self, n: u64) -> u64 {
        self.rng.below(n)
    }

    fn yes(&mut self) -> bool {
        self.next() & 1 == 1
    }

    /// A number of each width a head has.
    fn wide(&mut self) -> u64 {
        match self.below(5) {
            0 => self.below(24),
            1 => self.below(1 << 8),
            2 => self.below(1 << 16),
            3 => self.below(1 << 32),
            _ => self.next(),
        }
    }

    fn u32(&mut self) -> u32 {
        self.wide() as u32
    }

    /// Mostly short, sometimes past the lengths a head of one and of two bytes says.
    fn text(&mut self) -> String {
        let len = match self.below(20) {
            0 => 24 + self.below(8),
            1 => 256 + self.below(4),
            n => n % 6,
        };
        (0..len).map(|_| CHARS[self.below(CHARS.len() as u64) as usize]).collect()  // alloc: test and fuzz reference only
    }

    fn bytes(&mut self) -> Vec<u8> {
        let len = self.count(1);
        (0..len).map(|_| self.next() as u8).collect()  // alloc: test and fuzz reference only
    }

    /// How many items a list has: at the top sometimes past what a head of one byte says.
    fn count(&mut self, depth: u32) -> usize {
        match (depth, self.below(12)) {
            (0, 0) => 24 + self.below(4) as usize,
            (_, n) => (n % 3) as usize,
        }
    }

    fn many<T>(&mut self, depth: u32, mut one: impl FnMut(&mut Gen) -> T) -> Vec<T> {
        let len = self.count(depth);
        (0..len).map(|_| one(self)).collect()  // alloc: test and fuzz reference only
    }

    fn maybe<T>(&mut self, one: impl FnOnce(&mut Gen) -> T) -> Option<T> {
        match self.yes() {
            true => Some(one(self)),
            false => None,
        }
    }

    fn texts(&mut self, depth: u32) -> Vec<String> {
        self.many(depth, Gen::text)
    }

    fn outcome(&mut self) -> TestOutcome {
        match self.below(4) {
            0 => TestOutcome::Passed,
            1 => TestOutcome::Failed,
            2 => TestOutcome::Ignored,
            _ => TestOutcome::Unreached,
        }
    }

    fn pairs(&mut self, depth: u32) -> Vec<(String, String)> {
        self.many(depth, |g| (g.text(), g.text()))
    }

    fn id(&mut self) -> ContentId {
        let mut bytes = [0; 32];
        for byte in &mut bytes {
            *byte = self.next() as u8;
        }
        ContentId(bytes)
    }

    fn fingerprint(&mut self) -> Fingerprint {
        let high = u128::from(self.next());
        Fingerprint((high << 64) | u128::from(self.next()))
    }

    fn key(&mut self) -> QueryKey {
        QueryKey { kind: QueryKind(self.wide() as u16), input: self.id() }
    }

    fn fact(&mut self) -> FactRead {
        FactRead { key: self.key(), what: self.text() }
    }

    fn op(&mut self) -> Op {
        match self.below(4) {
            0 => Op::Write {
                path: self.text(),
                bytes: self.bytes(),
                mode: match self.below(3) {
                    0 => Mode::Regular,
                    1 => Mode::Executable,
                    _ => Mode::Symlink,
                },
            },
            1 => {
                Op::Edit { path: self.text(), at: self.wide(), len: self.wide(), bytes: self.bytes() }
            }
            2 => Op::Delete { path: self.text() },
            _ => Op::Rename { from: self.text(), to: self.text() },
        }
    }

    fn ops(&mut self, depth: u32) -> Vec<Op> {
        match self.plain {
            true => Vec::new(),  // alloc: test and fuzz reference only
            false => self.many(depth, Gen::op),
        }
    }

    fn changes(&mut self, depth: u32) -> Vec<Change> {
        if self.plain {
            return Vec::new();  // alloc: test and fuzz reference only
        }
        self.many(depth, |g| Change {
            path: g.text(),
            kind: match g.below(3) {
                0 => ChangeKind::Added,
                1 => ChangeKind::Removed,
                _ => ChangeKind::Modified,
            },
        })
    }

    fn edges(&mut self, depth: u32) -> Vec<Edge> {
        if self.plain {
            return Vec::new();  // alloc: test and fuzz reference only
        }
        self.many(depth, |g| Edge { read: g.key(), saw: g.fingerprint() })
    }

    fn why(&mut self) -> Why {
        Why {
            query: self.key(),
            chain: self.many(1, |g| Changed {
                key: g.key(),
                was: g.fingerprint(),
                now: g.fingerprint(),
            }),
            input: self.maybe(|g| match g.below(3) {
                0 => Input::File(g.text()),
                1 => Input::Dir(g.text()),
                _ => Input::Config,
            }),
        }
    }

    fn view(&mut self, depth: u32) -> ViewRef {
        let kinds = if self.plain || depth > 2 { 2 } else { 3 };
        match self.below(kinds) {
            0 => ViewRef::Head(self.text()),
            1 => ViewRef::Root(View { root: self.id() }),
            _ => ViewRef::WhatIf { ops: self.ops(depth + 1), base: Box::new(self.view(depth + 1)) },  // alloc: test and fuzz reference only
        }
    }

    fn cargo(&mut self) -> Cargo {
        Cargo {
            cwd: self.text(),
            env: self.pairs(1),
            args: self.texts(1),
            root: self.text(),
            color: self.yes(),
            files: self.many(1, |g| (g.text(), g.bytes())),
            width: self.maybe(Gen::u32),
            command: self.text(),
            refused: self.maybe(Gen::u32),
            verbose: self.next() as u8,
        }
    }

    fn targets(&mut self) -> Targets {
        Targets {
            cargo: self.maybe(Gen::cargo),
            target: self.maybe(Gen::text),
            profile: self.text(),
            features: self.texts(1),
            packages: self.texts(1),
            all_features: self.yes(),
            no_default_features: self.yes(),
        }
    }

    fn request(&mut self) -> Request {
        match self.below(REQUESTS as u64) {
            0 => Request::Commit { ops: self.ops(0), base: self.id(), head: self.text() },
            1 => Request::Check { view: self.view(0), targets: self.targets() },
            2 => Request::Build { view: self.view(0), targets: self.targets() },
            3 => Request::Test {
                ub: self.maybe(|g| Ub { seed: g.maybe(Gen::wide), schedules: g.maybe(Gen::u32) }),
                all: self.yes(),
                args: self.texts(0),
                view: self.view(0),
                filter: self.texts(0),
                targets: self.targets(),
            },
            4 => Request::Run {
                bin: self.maybe(Gen::text),
                args: self.texts(0),
                view: self.view(0),
                targets: self.targets(),
            },
            5 => Request::Diff { to: self.view(0), from: self.view(0) },
            6 => Request::Why { view: self.view(0), query: self.key() },
            7 => Request::Status,
            8 => Request::Cancel { id: self.wide() },
            9 => Request::Open { view: self.view(0) },
            10 => Request::Close { root: self.id() },
            11 => Request::Reach { to: self.view(0), from: self.view(0), targets: self.targets() },
            12 => Request::BestOf {
                base: self.view(0),
                test: self.yes(),
                filter: self.texts(0),
                targets: self.targets(),
                candidates: self.many(0, |g| g.ops(1)),
            },
            13 => Request::Verify { view: self.view(0), targets: self.targets() },
            14 => Request::Snapshot,
            15 => Request::CodegenInputs { view: self.view(0), targets: self.targets() },
            16 => Request::Trace { view: self.view(0), targets: self.targets() },
            17 => Request::Tables { view: self.view(0) },
            18 => Request::Edit { base: self.id(), head: self.text(), edits: self.many(0, Gen::edit) },
            19 => Request::WillTest {
                view: self.view(0),
                filter: self.texts(0),
                targets: self.targets(),
                wait_ms: self.wide(),
            },
            20 => Request::WillSave {
                ops: self.ops(0),
                head: self.text(),
                test: self.yes(),
                targets: self.targets(),
                wait_ms: self.wide(),
            },
            21 => Request::Memory,
            22 => Request::Pack { view: self.view(0), targets: self.targets(), program: self.text() },
            23 => Request::Doc { view: self.view(0), no_deps: self.yes(), private: self.yes(), targets: self.targets() },
            24 => Request::Explore { dev: self.yes() },
            25 => Request::Profile { seconds: self.u32() },
            26 => Request::Live,
            27 => Request::Sql { sql: self.text(), last_s: self.u32() },
            28 => Request::Hot {
                top: self.u32(),
                since: if self.yes() { crate::HotSince::LastBuild } else { crate::HotSince::Start },
            },
            _ => Request::Stop,
        }
    }

    fn target(&mut self) -> Target {
        Target { file: self.text(), item: self.text() }
    }

    fn edit(&mut self) -> Edit {
        match self.below(10) {
            0 => Edit::ReplaceBody { body: self.text(), item: self.target() },
            1 => Edit::ReplaceSignature { item: self.target(), signature: self.text() },
            2 => Edit::ReplaceItem { item: self.target(), text: self.text() },
            3 => Edit::AddField { field: self.text(), strukt: self.target() },
            4 => Edit::RemoveField { field: self.text(), strukt: self.target() },
            5 => Edit::AddItem { file: self.text(), into: self.maybe(Gen::text), text: self.text() },
            6 => Edit::RemoveItem { item: self.target() },
            7 => Edit::AddUse { file: self.text(), into: self.maybe(Gen::text), path: self.text() },
            8 => Edit::Rename { to: self.text(), item: self.target() },
            _ => Edit::Replace { to: self.text(), from: self.text() },
        }
    }

    fn edited(&mut self, depth: u32) -> Edited {
        const HOWS: [How; 8] = [
            How::Body,
            How::Signature,
            How::Fields,
            How::Whole,
            How::Added,
            How::Removed,
            How::Renamed,
            How::Scope,
        ];
        Edited {
            ops: self.ops(depth),
            items: self.many(depth, |g| EditedItem {
                how: HOWS[g.below(HOWS.len() as u64) as usize],
                key: g.maybe(Gen::id),
                file: g.text(),
                path: g.text(),
            }),
        }
    }

    fn span(&mut self) -> Span {
        Span {
            file: self.text(),
            line: self.u32(),
            label: self.maybe(Gen::text),
            column: self.u32(),
            primary: self.yes(),
            byte_end: self.u32(),
            end_line: self.u32(),
            byte_start: self.u32(),
            end_column: self.u32(),
            replacement: self.maybe(Gen::text),
            applicability: self.maybe(|g| {
                const SURE: [Applicability; 4] = [
                    Applicability::MachineApplicable,
                    Applicability::MaybeIncorrect,
                    Applicability::HasPlaceholders,
                    Applicability::Unspecified,
                ];
                SURE[g.below(SURE.len() as u64) as usize]
            }),
        }
    }

    fn unit_target(&mut self) -> UnitTarget {
        const KINDS: [TargetKind; 6] = [
            TargetKind::Lib,
            TargetKind::Bin,
            TargetKind::Test,
            TargetKind::Bench,
            TargetKind::Example,
            TargetKind::CustomBuild,
        ];
        const TYPES: [CrateType; 8] = [
            CrateType::Bin,
            CrateType::Lib,
            CrateType::Rlib,
            CrateType::Dylib,
            CrateType::Cdylib,
            CrateType::Staticlib,
            CrateType::ProcMacro,
            CrateType::Sdylib,
        ];
        UnitTarget {
            doc: self.yes(),
            kind: KINDS[self.below(KINDS.len() as u64) as usize],
            name: self.text(),
            test: self.yes(),
            doctest: self.yes(),
            edition: self.text(),
            src_path: self.text(),
            crate_types: self.many(1, |g| TYPES[g.below(TYPES.len() as u64) as usize]),
        }
    }

    fn unit_of(&mut self) -> UnitOf {
        UnitOf {
            id: self.u32(),
            test: self.yes(),
            target: self.unit_target(),
            package_id: self.text(),
            manifest_path: self.text(),
        }
    }

    fn diagnostic(&mut self, depth: u32) -> Diagnostic {
        Diagnostic {
            code: self.maybe(Gen::text),
            unit: self.maybe(Gen::unit_of),
            facts: self.many(1, Gen::fact),
            level: match self.below(5) {
                0 => Level::Error,
                1 => Level::Warning,
                2 => Level::Note,
                3 => Level::Help,
                _ => Level::FailureNote,
            },
            spans: self.many(1, Gen::span),
            message: self.text(),
            children: match depth < 2 {
                true => self.many(1, |g| g.diagnostic(depth + 1)),
                false => Vec::new(),  // alloc: test and fuzz reference only
            },
            rendered: self.text(),
            explanation: self.maybe(Gen::text),
        }
    }

    fn artifact(&mut self) -> Artifact {
        const KINDS: [ArtifactKind; 8] = [
            ArtifactKind::Bin,
            ArtifactKind::Lib,
            ArtifactKind::Test,
            ArtifactKind::Example,
            ArtifactKind::Bench,
            ArtifactKind::ProcMacro,
            ArtifactKind::CustomBuild,
            ArtifactKind::Doctest,
        ];
        Artifact {
            env: self.pairs(1),
            kind: KINDS[self.below(KINDS.len() as u64) as usize],
            path: self.text(),
            fresh: self.yes(),
            runner: self.texts(1),
            target: self.unit_target(),
            content: self.id(),
            harness: self.yes(),
            package: self.text(),
            profile: UnitProfile {
                test: self.yes(),
                debuginfo: self.u32(),
                opt_level: self.text(),
                overflow_checks: self.yes(),
                debug_assertions: self.yes(),
            },
            features: self.texts(1),
            filenames: self.texts(1),
            executable: self.maybe(Gen::text),
            manifest_path: self.text(),
        }
    }

    /// A response; [`Response::Of`], the last, only at a `depth` under 2.
    fn response(&mut self, depth: u32) -> Response {
        let kinds = if depth > 1 { RESPONSES - 1 } else { RESPONSES };
        match self.below(kinds as u64) {
            0 => Response::Committed { root: self.id(), rebased: self.yes() },
            1 => Response::Diagnostic(self.diagnostic(0)),
            2 => Response::Built {
                members: self.u32(),
                profile: self.text(),
                artifacts: self.many(depth, Gen::artifact),
            },
            3 => Response::Unit(UnitStarted {
                name: self.text(),
                verb: match self.below(3) {
                    0 => Verb::Compiling,
                    1 => Verb::Checking,
                    _ => Verb::Fresh,
                },
                source: self.maybe(Gen::text),
                package: self.u32(),
                version: self.text(),
                package_id: self.text(),
            }),
            4 => Response::UnitEnded(self.unit_of()),
            5 => Response::Fetched {
                verb: match self.yes() {
                    true => FetchVerb::Updating,
                    false => FetchVerb::Downloaded,
                },
                what: self.text(),
            },
            6 => Response::BuildScript(BuildScriptRan {
                env: self.pairs(depth),
                cfgs: self.texts(depth),
                out_dir: self.text(),
                package_id: self.text(),
                linked_libs: self.texts(depth),
                linked_paths: self.texts(depth),
            }),
            7 => Response::Work(Worked {
                link: match self.yes() {
                    true => LinkKind::Patched,
                    false => LinkKind::Placed,
                },
                found: self.wide(),
                green: self.wide(),
                links: self.many(depth, |g| Linked {
                    how: match g.below(3) {
                        0 => LinkHow::First,
                        1 => LinkHow::Spliced,
                        _ => LinkHow::Full { why: g.texts(depth) },
                    },
                    path: g.text(),
                }),
                reran: self.wide(),
                served: self.many(depth, |g| ServedCrate {
                    why: g.maybe(Gen::text),
                    bodies: ServedPart::ALL[(g.next() % 3) as usize],
                    outcome: ServedPart::ALL[(g.next() % 3) as usize],
                    package: g.text(),
                }),
                compiled: self.wide(),
                steps_us: self.many(depth, |g| {
                    (NodeKind::ALL[(g.next() % NodeKind::ALL.len() as u64) as usize], g.wide())
                }),
                critical_path_us: self.wide(),
            }),
            8 => Response::TestResult(TestResult {
                name: self.text(),
                binary: self.text(),
                cached: self.maybe(|g| Cached {
                    basis: match g.yes() {
                        true => Basis::Executed,
                        false => Basis::Reached { why: g.text() },
                    },
                    millis: g.wide(),
                    outcome: g.outcome(),
                }),
                millis: self.wide(),
                output: self.bytes(),
                outcome: self.outcome(),
                outside: self.yes(),
                executable: self.u32(),
            }),
            9 => Response::Ran {
                code: self.maybe(|g| g.next() as i32),
                stderr: self.bytes(),
                stdout: self.bytes(),
            },
            10 => Response::Diff { changes: self.changes(depth) },
            11 if self.plain => Response::Done { ok: self.yes() },
            11 => Response::Why(self.why()),
            12 => Response::Status(Status {
                pid: self.u32(),
                views: self.u32(),
                history: self.maybe(Gen::text),
                requests: self.many(depth, Gen::wide),
                budget_bytes: self.wide(),
                memory_bytes: self.wide(),
                build: crate::Build {
                    id: self.id(),
                    exe: crate::Executable { path: self.text(), len: self.wide(), modified_ns: self.wide(), inode: self.wide() },
                },
            }),
            13 => Response::Done { ok: self.yes() },
            14 => Response::Error(ApiError { at: self.maybe(Gen::text), what: self.text() }),
            15 => Response::Opened { root: self.id() },
            16 => Response::Reached(Reach {
                items: self.many(depth, |g| ReachedItem {
                    path: g.text(),
                    facts: g.many(1, Gen::fact),
                }),
                tests: self.texts(depth),
                changes: self.changes(depth),
                compiled: self.texts(depth),
            }),
            17 => Response::Ranked(self.many(depth, |g| Ranked {
                ok: g.yes(),
                root: g.id(),
                errors: g.u32(),
                failed: g.u32(),
                passed: g.u32(),
                warnings: g.u32(),
                candidate: g.u32(),
            })),
            18 => Response::Verified {
                mismatches: self.many(depth, |g| Mismatch {
                    fact: g.maybe(Gen::fact),
                    query: g.key(),
                    scratch: g.fingerprint(),
                    incremental: g.fingerprint(),
                }),
            },
            19 => Response::Snapshotted { heads: self.many(depth, |g| (g.text(), g.id())) },
            20 => Response::CodegenInputs(self.many(depth, |g| (g.id(), g.id()))),
            21 => Response::Trace(self.many(depth, |g| Traced {
                key: g.key(),
                ran: g.yes(),
                edges: g.edges(1),
                fingerprint: g.fingerprint(),
            })),
            22 => Response::Tables(self.many(depth, |g| Table {
                name: g.text(),
                bytes: g.wide(),
                elements: g.wide(),
            })),
            23 => Response::Edited(self.edited(depth)),
            24 => Response::Announced { root: self.id() },
            25 => Response::Memory(Memory {
                tags: self.many(depth, |g| TagMemory {
                    tag: g.text(),
                    live: g.wide(),
                    rate: g.wide(),
                    budget: g.wide(),
                    allocated: g.wide(),
                    task_peak: g.wide(),
                    high_water: g.wide(),
                }),
                committed: self.wide(),
                committed_high_water: self.wide(),
            }),
            26 => Response::Packed { pack: self.bytes() },
            27 => Response::DocPage { page: self.bytes(), path: self.text() },
            28 => Response::Documented(Documented {
                dir: self.text(),
                pages: self.wide(),
                crates: self.many(depth, |g| DocumentedCrate { name: g.text(), asked: g.yes(), package: g.text() }),
                source_pages: self.wide(),
            }),
            29 => Response::Explored { link: self.text() },
            30 => Response::Profiled { trace: self.bytes() },
            31 => Response::Live(LiveFrame {
                lost: self.wide(),
                open: self.bytes(),
                at_ns: self.wide() as i64,
                spans: self.bytes(),
                skipped: self.wide(),
                counters: self.bytes(),
            }),
            32 => Response::Queried { ipc: self.bytes(), rows: self.wide() },
            33 => Response::Hot(crate::Hot {
                keys: self.many(depth, |g| crate::HotKey {
                    span: g.text(),
                    function: match g.below(3) {
                        0 => crate::HotFunction::None,
                        1 => crate::HotFunction::Named(g.text()),
                        _ => crate::HotFunction::Forgotten(g.u32()),
                    },
                    lower_ns: g.wide(),
                    upper_ns: g.wide(),
                }),
                torn: self.wide(),
                since: if self.yes() { crate::HotSince::LastBuild } else { crate::HotSince::Start },
                spans: self.wide(),
                waits: self.many(depth, |g| crate::HotWait {
                    kind: match g.below(6) {
                        0 => crate::HotWaitKind::Key,
                        1 => crate::HotWaitKind::Lock,
                        2 => crate::HotWaitKind::Channel,
                        3 => crate::HotWaitKind::Io,
                        4 => crate::HotWaitKind::Child,
                        _ => crate::HotWaitKind::Scope,
                    },
                    count: g.wide(),
                    max_ns: g.wide(),
                    p50_ns: g.wide(),
                    p90_ns: g.wide(),
                    p99_ns: g.wide(),
                }),
                error_ns: self.wide(),
                total_ns: self.wide(),
            }),
            _ => {
                Response::Of { response: Box::new(self.response(depth + 1)), candidate: self.u32() }  // alloc: test and fuzz reference only
            }
        }
    }

    /// A few bytes flipped, replaced, cut, put in or swapped.
    fn mutate(&mut self, bytes: &mut Vec<u8>) {
        for _ in 0..=self.below(3) {
            match self.below(5) {
                0 if !bytes.is_empty() => {
                    let at = self.below(bytes.len() as u64) as usize;
                    bytes[at] ^= 1 << self.below(8);
                }
                1 if !bytes.is_empty() => {
                    let at = self.below(bytes.len() as u64) as usize;
                    bytes[at] = self.next() as u8;
                }
                2 => bytes.truncate(self.below(bytes.len() as u64 + 1) as usize),
                3 => {
                    let at = self.below(bytes.len() as u64 + 1) as usize;
                    bytes.insert(at, self.next() as u8);
                }
                _ if bytes.len() >= 2 => {
                    let at = self.below(bytes.len() as u64 - 1) as usize;
                    bytes.swap(at, at + 1);
                }
                _ => bytes.push(0),
            }
        }
    }
}

/// Which of [`Gen::request`]'s a request is; a variant added is a variant to generate.
fn request_index(request: &Request) -> usize {
    match request {
        Request::Commit { .. } => 0,
        Request::Check { .. } => 1,
        Request::Build { .. } => 2,
        Request::Test { .. } => 3,
        Request::Run { .. } => 4,
        Request::Diff { .. } => 5,
        Request::Why { .. } => 6,
        Request::Status => 7,
        Request::Cancel { .. } => 8,
        Request::Open { .. } => 9,
        Request::Close { .. } => 10,
        Request::Reach { .. } => 11,
        Request::BestOf { .. } => 12,
        Request::Verify { .. } => 13,
        Request::Snapshot => 14,
        Request::CodegenInputs { .. } => 15,
        Request::Trace { .. } => 16,
        Request::Tables { .. } => 17,
        Request::Edit { .. } => 18,
        Request::WillTest { .. } => 19,
        Request::WillSave { .. } => 20,
        Request::Memory => 21,
        Request::Pack { .. } => 22,
        Request::Doc { .. } => 23,
        Request::Explore { .. } => 24,
        Request::Profile { .. } => 25,
        Request::Live => 26,
        Request::Sql { .. } => 27,
        Request::Hot { .. } => 28,
        Request::Stop => 29,
    }
}

/// Which of [`Gen::response`]'s a response is; a variant added is a variant to generate.
fn response_index(response: &Response) -> usize {
    match response {
        Response::Committed { .. } => 0,
        Response::Diagnostic(_) => 1,
        Response::Built { .. } => 2,
        Response::Unit(_) => 3,
        Response::UnitEnded(_) => 4,
        Response::Fetched { .. } => 5,
        Response::BuildScript(_) => 6,
        Response::Work(_) => 7,
        Response::TestResult(_) => 8,
        Response::Ran { .. } => 9,
        Response::Diff { .. } => 10,
        Response::Why(_) => 11,
        Response::Status(_) => 12,
        Response::Done { .. } => 13,
        Response::Error(_) => 14,
        Response::Opened { .. } => 15,
        Response::Reached(_) => 16,
        Response::Ranked(_) => 17,
        Response::Verified { .. } => 18,
        Response::Snapshotted { .. } => 19,
        Response::CodegenInputs(_) => 20,
        Response::Trace(_) => 21,
        Response::Tables(_) => 22,
        Response::Edited(_) => 23,
        Response::Announced { .. } => 24,
        Response::Memory(_) => 25,
        Response::Packed { .. } => 26,
        Response::DocPage { .. } => 27,
        Response::Documented(_) => 28,
        Response::Explored { .. } => 29,
        Response::Profiled { .. } => 30,
        Response::Live(_) => 31,
        Response::Queried { .. } => 32,
        Response::Hot(_) => 33,
        Response::Of { .. } => 34,
    }
}

/// Holds `envelope` to the reference codec every way it is written and read; true when the
/// kept writer put no map of it in order.
fn same<T>(writer: &mut cbor::Writer, envelope: &Envelope<T>) -> bool
where
    T: Serialize + DeserializeOwned + PartialEq + Debug,
{
    let old = reference::to_vec(envelope).expect("the reference codec encodes every envelope");
    assert_eq!(cbor::to_vec(envelope).unwrap(), old, "{envelope:?}");
    let framed = encode(envelope).unwrap();
    assert_eq!(framed[..4], (old.len() as u32).to_be_bytes());
    assert_eq!(framed[4..], old[..], "{envelope:?}");
    let permuted = writer.permuted();
    let mut kept = vec![7];  // alloc: test and fuzz reference only
    encode_into(writer, &mut kept, envelope).unwrap();
    assert_eq!(kept[0], 7);
    assert_eq!(kept[5..], old[..], "{envelope:?}");
    let in_order = writer.permuted() == permuted;
    let back: Envelope<T> = cbor::from_slice(&old).unwrap();
    assert_eq!(&back, envelope);
    assert_eq!(reference::from_slice::<Envelope<T>>(&old).unwrap(), back);
    assert_eq!(&decode::<T>(&old).unwrap(), envelope);
    in_order
}

/// The same value, or the same error, from both codecs.
fn same_answer<T: DeserializeOwned + PartialEq + Debug>(bytes: &[u8]) {
    let new = cbor::from_slice::<Envelope<T>>(bytes);
    let old = reference::from_slice::<Envelope<T>>(bytes);
    assert_eq!(new, old, "{bytes:02x?}");
}

/// The response `bytes` encode read where it lies, by both codecs and by [`decode`], is the
/// one they read owned, or all of them refuse it.
fn same_lent(bytes: &[u8]) {
    fn lent(read: Result<Envelope<ResponseRef<'_>>, cbor::Error>) -> Result<Envelope<Response>, cbor::Error> {
        read.map(|it| Envelope { schema: it.schema, id: it.id, body: it.body.into_owned() })
    }
    let owned = cbor::from_slice::<Envelope<Response>>(bytes);
    let readings = [
        lent(cbor::from_slice::<Envelope<ResponseRef<'_>>>(bytes)),
        lent(reference::from_slice::<Envelope<ResponseRef<'_>>>(bytes)),
    ];
    for reading in readings {
        match (&owned, reading) {
            (Ok(owned), Ok(lent)) => assert_eq!(&lent, owned, "{bytes:02x?}"),
            (Err(_), Err(_)) => {}
            (owned, lent) => panic!("owned {owned:?}, in place {lent:?}: {bytes:02x?}"),
        }
    }
    let owned = decode::<Response>(bytes).map_err(|it| it.id);
    match (owned, decode::<ResponseRef<'_>>(bytes)) {
        (Ok(owned), Ok(lent)) => assert_eq!(lent.body.into_owned(), owned.body, "{bytes:02x?}"),
        (Err(owned), Err(lent)) => assert_eq!(lent.id, owned, "{bytes:02x?}"),
        (owned, lent) => panic!("owned {owned:?}, in place {lent:?}: {bytes:02x?}"),
    }
}

#[test]
fn every_request_and_response_is_the_bytes_the_old_codec_wrote() {
    let mut g = Gen { rng: SplitMix64::new(0x2545_f491_4f6c_dd1d), plain: false };
    let mut writer = cbor::Writer::new();
    let (mut requests, mut responses) = ([false; REQUESTS], [false; RESPONSES]);
    for round in 0..3000 {
        g.plain = round % 2 == 0;
        let envelope = Envelope::new(g.wide(), g.request());
        requests[request_index(&envelope.body)] = true;
        let in_order = same(&mut writer, &envelope);
        assert!(
            in_order || !g.plain,
            "a map of the API's own types written out of order (declare fields by key, shorter \
             first, then bytewise): {envelope:?}"
        );
        let envelope = Envelope::new(g.wide(), g.response(0));
        responses[response_index(&envelope.body)] = true;
        same_lent(&cbor::to_vec(&envelope).unwrap());
        let in_order = same(&mut writer, &envelope);
        assert!(
            in_order || !g.plain,
            "a map of the API's own types written out of order (declare fields by key, shorter \
             first, then bytewise): {envelope:?}"
        );
    }
    assert!(requests.iter().all(|&seen| seen), "requests generated: {requests:?}");
    assert!(responses.iter().all(|&seen| seen), "responses generated: {responses:?}");
}

#[test]
fn changed_bytes_read_as_the_old_codec_read_them() {
    let mut g = Gen { rng: SplitMix64::new(0x9e37_79b9_7f4a_7c15), plain: false };
    for _ in 0..1500 {
        let mut request = reference::to_vec(&Envelope::new(g.wide(), g.request())).unwrap();
        g.mutate(&mut request);
        same_answer::<Request>(&request);
        let mut response = reference::to_vec(&Envelope::new(g.wide(), g.response(0))).unwrap();
        g.mutate(&mut response);
        same_answer::<Response>(&response);
        same_lent(&response);
    }
}

/// The JSON rendering is the bytes serde_json writes, for every request and response: the
/// same escapes, numbers and order, and the same lines through `line_into`.
#[test]
fn json_is_the_bytes_serde_json_wrote() {
    let mut g = Gen { rng: SplitMix64::new(0x6a73_6f6e), plain: false };
    let mut out = Vec::new();
    for _ in 0..3000 {
        let request = Envelope::new(g.wide(), g.request());
        let expected = serde_json::to_string(&request).unwrap();
        assert_eq!(crate::json::request(&request), expected, "{request:?}");
        let response = Envelope::new(g.wide(), g.response(0));
        let expected = serde_json::to_string(&response).unwrap();
        assert_eq!(crate::json::response(&response), expected, "{response:?}");
        out.clear();
        crate::json::line_into(&mut out, &response);
        assert_eq!(out, expected.as_bytes(), "{response:?}");
    }
}
