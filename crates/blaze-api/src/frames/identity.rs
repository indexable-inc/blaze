//! Every request and response of the API held to [`cbor::reference`], the codec before the
//! one in use: the same bytes from each way an envelope is written, the same value read back
//! by both, the same answer from both for the bytes changed, and nothing of the API's own
//! types put in order on the way out.

use std::{any::TypeId, collections::HashSet, fmt::Debug};

use blaze_ids::{ContentId, Fingerprint, rng::SplitMix64};
use blaze_protocol::cbor::{self, reference};
use blaze_query::{Changed, Edge, Input, QueryKey, QueryKind, Why};
use blaze_vfs::{Change, ChangeKind, Mode, Op, View};
use serde::{Serialize, de::DeserializeOwned};

use super::{decode, encode, encode_into};
use crate::{
    ApiError, Applicability, Artifact, ArtifactKind, Basis, Behind, BuildScriptRan, Cached, Cargo,
    CrateType, Diagnostic, Documented, DocumentedCrate, Edit, Edited, EditedItem, Envelope,
    FactRead, FetchVerb, Goal, How, Level, LinkHow, LinkKind, Linked, LiveFrame, Memory, Mismatch,
    NodeKind, NotKept, OpenFailure, RanWhy, Ranked, Reach, ReachedItem, Request, Response, ResponseRef, ServedCrate,
    ServedPart, Span, Status, Table, TagMemory, Target, TargetKind, Targets, TestOutcome,
    TestResult, Tiers, Traced, Ub, Unbuilt, Unchecked, UnitOf, UnitProfile, UnitStarted,
    UnitTarget, Verb, ViewRef, Worked,
};

const CHARS: [char; 9] = ['a', 'Z', '0', ' ', '-', '/', '\u{e9}', '\u{6c34}', '\u{1f525}'];

/// One variant of an enum, each variant that can be made here as likely, made by its arm.
///
/// An arm is a variant's name and what follows it in a constructor (`{ fields }`, `(values)`
/// or nothing), then optionally `if` the condition under which it can be made. The list of arms
/// is all the bookkeeping there is: a variant's position and the count are the list's, so two
/// changes that each add an arm merge without renumbering, and a variant of the enum the list
/// leaves out does not compile ("patterns `&Request::New { .. }` not covered").
macro_rules! one_of {
    ($g:expr, $enum:path {
        $($variant:ident $({ $($fields:tt)* })? $(( $($values:tt)* ))? $(if $can:expr)?),+ $(,)?
    }) => {{
        type This = $enum;
        #[derive(Clone, Copy)]
        enum Kind {
            $($variant),+
        }
        // Every variant of `This` has an arm: the check that one added gets a generator.
        const _: fn(&This) = |it| match it {
            $(This::$variant { .. } => {})+
        };
        let kinds = [$((Kind::$variant, true $(&& $can)?)),+];
        match $g.pick(&kinds) {
            $(Kind::$variant => This::$variant $({ $($fields)* })? $(( $($values)* ))?,)+
        }
    }};
}

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

    /// One of the `kinds` marked `true`, each as likely: what [`one_of!`] draws with.
    fn pick<K: Copy>(&mut self, kinds: &[(K, bool)]) -> K {
        let can = kinds.iter().filter(|(_, can)| *can).count();
        let at = self.rng.index(can);
        let mut can = kinds.iter().filter(|(_, can)| *can).map(|&(kind, _)| kind);
        can.nth(at).expect("one_of! with no arm that can be made here")
    }

    /// One of a type's own list of its values (`Goal::ALL`), each as likely.
    fn one<T: Copy>(&mut self, all: &[T]) -> T {
        all[self.rng.index(all.len())]
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
        (0..len).map(|_| CHARS[self.below(CHARS.len() as u64) as usize]).collect() // alloc: test and fuzz reference only
    }

    fn bytes(&mut self) -> Vec<u8> {
        let len = self.count(1);
        (0..len).map(|_| self.next() as u8).collect() // alloc: test and fuzz reference only
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
        (0..len).map(|_| one(self)).collect() // alloc: test and fuzz reference only
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
        one_of!(self, TestOutcome { Passed, Failed, Ignored })
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
        one_of!(self, Op {
            Write {
                path: self.text(),
                bytes: self.bytes(),
                mode: one_of!(self, Mode { Regular, Executable, Symlink }),
            },
            Edit { path: self.text(), at: self.wide(), len: self.wide(), bytes: self.bytes() },
            Delete { path: self.text() },
            Rename { from: self.text(), to: self.text() },
        })
    }

    fn ops(&mut self, depth: u32) -> Vec<Op> {
        match self.plain {
            true => Vec::new(), // alloc: test and fuzz reference only
            false => self.many(depth, Gen::op),
        }
    }

    fn changes(&mut self, depth: u32) -> Vec<Change> {
        if self.plain {
            return Vec::new(); // alloc: test and fuzz reference only
        }
        self.many(depth, |g| Change {
            path: g.text(),
            kind: one_of!(g, ChangeKind { Added, Removed, Modified }),
        })
    }

    fn edges(&mut self, depth: u32) -> Vec<Edge> {
        if self.plain {
            return Vec::new(); // alloc: test and fuzz reference only
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
            input: self.maybe(|g| one_of!(g, Input {
                File(g.text()),
                Dir(g.text()),
                Config,
                FileAt { root: g.id(), path: g.text() },
                DirAt { root: g.id(), path: g.text() },
            })),
        }
    }

    fn view(&mut self, depth: u32) -> ViewRef {
        one_of!(self, ViewRef {
            Head(self.text()),
            Root(View { root: self.id() }),
            // Holds ops, which `plain` leaves out, and a view: only near the top, so it ends.
            WhatIf { ops: self.ops(depth + 1), base: Box::new(self.view(depth + 1)) } // alloc: test and fuzz reference only
                if !self.plain && depth <= 2,
        })
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
            prebuilt: self.yes(),
        }
    }

    fn targets(&mut self) -> Targets {
        Targets {
            cargo: self.maybe(Gen::cargo),
            target: self.maybe(Gen::text),
            profile: self.text(),
            goal: self.maybe(Gen::goal),
            features: self.texts(1),
            packages: self.texts(1),
            all_features: self.yes(),
            no_default_features: self.yes(),
        }
    }

    fn goal(&mut self) -> Goal {
        self.one(&Goal::ALL)
    }

    fn hot_since(&mut self) -> crate::HotSince {
        one_of!(self, crate::HotSince { LastBuild, Start })
    }

    fn request(&mut self) -> Request {
        one_of!(self, Request {
            Commit { ops: self.ops(0), base: self.id(), head: self.text() },
            Check { view: self.view(0), targets: self.targets() },
            Build { view: self.view(0), targets: self.targets() },
            Test {
                ub: self.maybe(|g| Ub { seed: g.maybe(Gen::wide), schedules: g.maybe(Gen::u32) }),
                all: self.yes(),
                args: self.texts(0),
                view: self.view(0),
                filter: self.texts(0),
                targets: self.targets(),
            },
            Run {
                bin: self.maybe(Gen::text),
                args: self.texts(0),
                view: self.view(0),
                targets: self.targets(),
            },
            Diff { to: self.view(0), from: self.view(0) },
            Why { view: self.view(0), query: self.key() },
            Status,
            Cancel { id: self.wide() },
            Open { view: self.view(0) },
            Close { root: self.id() },
            Reach { to: self.view(0), from: self.view(0), targets: self.targets() },
            BestOf {
                base: self.view(0),
                test: self.yes(),
                filter: self.texts(0),
                targets: self.targets(),
                candidates: self.many(0, |g| g.ops(1)),
            },
            Verify { view: self.view(0), targets: self.targets() },
            Snapshot,
            CodegenInputs { view: self.view(0), targets: self.targets() },
            Trace { view: self.view(0), targets: self.targets() },
            Tables { view: self.view(0) },
            Edit { base: self.id(), head: self.maybe(Gen::text), edits: self.many(0, Gen::edit) },
            WillTest {
                view: self.view(0),
                filter: self.texts(0),
                targets: self.targets(),
                wait_ms: self.wide(),
            },
            WillSave {
                ops: self.ops(0),
                head: self.text(),
                test: self.yes(),
                targets: self.targets(),
                wait_ms: self.wide(),
            },
            Memory,
            Doc {
                view: self.view(0),
                no_deps: self.yes(),
                private: self.yes(),
                targets: self.targets(),
            },
            Explore { dev: self.yes() },
            Profile { seconds: self.u32() },
            Live,
            Flush,
            Hot { top: self.u32(), since: self.hot_since() },
            Gc { dry_run: self.yes(), unkept: self.yes() },
            Rebuilt {
                all: self.yes(),
                cwd: self.maybe_text(),
                item: self.maybe_text(),
                build: if self.yes() { Some(self.wide()) } else { None },
                unexplained: self.yes(),
            },
            Launch { program: self.text(), pid: self.u32() },
            FetchUrl {
                url: self.maybe_text(),
                repo: self.maybe_text(),
                rev: self.maybe_text(),
                digest: self.text(),
                most_bytes: if self.yes() { Some(self.wide()) } else { None },
                out: self.maybe_text(),
            },
            ClientRows {
                build: self.wide(),
                events: self.bytes(),
                folded: self.bytes(),
                spans: self.bytes(),
                ended_ns: self.wide() as i64,
                started_ns: self.wide() as i64,
                pid: self.u32(),
                timed: (0..self.below(3))
                    .map(|_| crate::Timed {
                        name: self.text(),
                        start_ns: self.wide() as i64,
                        end_ns: self.wide() as i64,
                    })
                    .collect(),
            },
            Lock { view: self.view(0), targets: self.targets() },
            Stop,
            Store { words: self.texts(0) },
            Diagnostics {
                view: self.view(0),
                since: self.maybe_id(),
                client: self.text(),
                targets: self.targets(),
                wait_ms: self.wide(),
            },
            Prove {
                emit: self.yes(),
                lean: self.maybe_text(),
                view: self.view(0),
                subset: self.yes(),
                targets: self.targets(),
            },
            Query { view: self.view(0), rules: self.text() },
        })
    }

    fn target(&mut self) -> Target {
        Target { file: self.text(), item: self.text() }
    }

    fn edit(&mut self) -> Edit {
        one_of!(self, Edit {
            ReplaceBody { body: self.text(), item: self.target() },
            ReplaceSignature { item: self.target(), signature: self.text() },
            ReplaceItem { item: self.target(), text: self.text() },
            AddField { field: self.text(), strukt: self.target() },
            RemoveField { field: self.text(), strukt: self.target() },
            AddItem { file: self.text(), into: self.maybe(Gen::text), text: self.text() },
            RemoveItem { item: self.target() },
            AddUse { file: self.text(), into: self.maybe(Gen::text), path: self.text() },
            Rename { to: self.text(), item: self.target() },
            Rules { rules: self.text() },
            Replace { to: self.text(), from: self.text() },
        })
    }

    fn edited(&mut self, depth: u32) -> Edited {
        Edited {
            ops: self.ops(depth),
            items: self.many(depth, |g| EditedItem {
                how: one_of!(g, How { Body, Signature, Fields, Whole, Added, Removed, Renamed, Scope }),
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
            applicability: self.maybe(|g| g.one(&Applicability::ALL)),
        }
    }

    fn unit_target(&mut self) -> UnitTarget {
        UnitTarget {
            doc: self.yes(),
            kind: self.one(&TargetKind::ALL),
            name: self.text(),
            test: self.yes(),
            doctest: self.yes(),
            edition: self.text(),
            src_path: self.text(),
            crate_types: self.many(1, |g| g.one(&CrateType::ALL)),
        }
    }

    fn unit_of(&mut self) -> UnitOf {
        UnitOf {
            id: self.u32(),
            test: self.yes(),
            doctest: self.yes(),
            target: self.unit_target(),
            package_id: self.text(),
            manifest_path: self.text(),
            primary: self.yes(),
        }
    }

    fn diagnostic(&mut self, depth: u32) -> Diagnostic {
        Diagnostic {
            code: self.maybe(Gen::text),
            unit: self.maybe(Gen::unit_of),
            facts: self.many(1, Gen::fact),
            level: one_of!(self, Level { Error, Warning, Note, Help, FailureNote }),
            spans: self.many(1, Gen::span),
            message: self.text(),
            children: match depth < 2 {
                true => self.many(1, |g| g.diagnostic(depth + 1)),
                false => Vec::new(), // alloc: test and fuzz reference only
            },
            rendered: self.text(),
            explanation: self.maybe(Gen::text),
        }
    }

    fn found(&mut self) -> crate::Found {
        crate::Found {
            id: self.id(),
            code: self.maybe(Gen::text),
            help: self.texts(1),
            span: self.maybe(Gen::span),
            fixes: self.many(1, Gen::span),
            // A found is an error or a warning, never another level.
            level: if self.yes() { Level::Error } else { Level::Warning },
            message: self.text(),
            package: self.maybe(Gen::text),
            primary: self.yes(),
            rendered: self.text(),
        }
    }

    fn artifact(&mut self) -> Artifact {
        Artifact {
            env: self.pairs(1),
            kind: self.one(&ArtifactKind::ALL),
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

    fn response(&mut self, depth: u32) -> Response {
        one_of!(self, Response {
            Committed { root: self.id(), rebased: self.yes() },
            Diagnostic(self.diagnostic(0)),
            Built {
                members: self.u32(),
                profile: self.text(),
                artifacts: self.many(depth, Gen::artifact),
            },
            Unit(UnitStarted {
                name: self.text(),
                verb: one_of!(self, Verb { Compiling, Checking, Fresh }),
                source: self.maybe(Gen::text),
                package: self.u32(),
                version: self.text(),
                package_id: self.text(),
            }),
            UnitEnded(self.unit_of()),
            Fetched { verb: one_of!(self, FetchVerb { Updating, Downloaded }), what: self.text() },
            BuildScript(BuildScriptRan {
                env: self.pairs(depth),
                cfgs: self.texts(depth),
                out_dir: self.text(),
                package_id: self.text(),
                linked_libs: self.texts(depth),
                linked_paths: self.texts(depth),
            }),
            Work(Worked {
                why: self.maybe(|g| g.many(depth, |g| g.report(depth + 1))),
                link: one_of!(self, LinkKind { Patched, Placed }),
                found: self.wide(),
                green: self.wide(),
                links: self.many(depth, |g| Linked {
                    how: one_of!(g, LinkHow { First, Spliced, Unchanged, Full { why: g.texts(depth) } }),
                    path: g.text(),
                }),
                reran: self.wide(),
                misses: self.wide(),
                served: self.many(depth, |g| ServedCrate {
                    why: g.maybe(Gen::text),
                    bodies: g.one(&ServedPart::ALL),
                    outcome: g.one(&ServedPart::ALL),
                    package: g.text(),
                }),
                build_id: self.maybe(Gen::wide),
                compiled: self.wide(),
                steps_us: self.many(depth, |g| (g.one(&NodeKind::ALL), g.wide())),
                critical_path_us: self.wide(),
                placed: crate::PlacedBytes {
                    cloned: self.wide(),
                    written: self.wide(),
                    copied: self.wide(),
                },
                tiers: self.maybe(|g| Tiers {
                    goal: g.goal(),
                    optimized: g.wide(),
                    functions: g.wide(),
                }),
                storage: self.maybe(|g| crate::Storage {
                    store_bytes: g.wide(),
                    store_delta_bytes: g.wide() as i64,
                    held_bytes: g.wide(),
                    budget_bytes: g.wide(),
                    collectable_bytes: g.wide(),
                    volume_free_bytes: g.wide(),
                    over_budget: g.yes(),
                    warning: g.maybe_text(),
                }),
            }),
            TestResult(TestResult {
                ran: self.maybe(|g| RanWhy {
                    ask: g.maybe_text(),
                    now: g.maybe_text(),
                    why: g.text(),
                    said: g.text(),
                    before: g.maybe_text(),
                }),
                name: self.text(),
                binary: self.text(),
                cached: self.maybe(|g| Cached {
                    basis: one_of!(g, Basis { Executed, Reached { why: g.text() } }),
                    millis: g.wide(),
                    outcome: g.outcome(),
                }),
                millis: self.wide(),
                output: self.bytes(),
                outcome: self.outcome(),
                outside: self.yes(),
                executable: self.u32(),
                not_kept: self.maybe(|g| NotKept { no: g.text(), why: g.text() }),
            }),
            Ran {
                code: self.maybe(|g| g.next() as i32),
                stderr: self.bytes(),
                stdout: self.bytes(),
            },
            Diff { changes: self.changes(depth) },
            // Holds maps of another crate's types, which `plain` leaves out.
            Why(self.why()) if !self.plain,
            Status(Status {
                pid: self.u32(),
                views: self.u32(),
                history: self.maybe(Gen::text),
                requests: self.many(depth, Gen::wide),
                settings: self
                    .many(depth, |g| crate::Setting { name: g.text(), value: g.maybe(Gen::text) }),
                uptime_ms: self.wide(),
                placed_by: self.maybe_text(),
                budget_bytes: self.wide(),
                memory_bytes: self.wide(),
                builds: self.many(depth, |g| crate::Building {
                    id: g.u32(),
                    now: blaze_protocol::now::Now::default(),
                    what: g.text(),
                    phase: g.text(),
                    spans: g.texts(depth + 1),
                    for_ms: g.wide(),
                }),
                downloads: self.many(depth, |g| crate::Downloading {
                    url: g.text(),
                    done: g.wide(),
                    rate: g.maybe(Gen::wide),
                    what: g.text(),
                    total: g.maybe(Gen::wide),
                    idle_ms: g.wide(),
                    elapsed_ms: g.wide(),
                }),
                build: crate::Build {
                    id: self.id(),
                    exe: crate::Executable {
                        path: self.text(),
                        len: self.wide(),
                        modified_ns: self.wide(),
                        inode: self.wide(),
                    },
                },
                bugs: self.many(depth, |g| crate::Bug {
                    kind: g.text(),
                    what: g.text(),
                    at: g.text(),
                    count: g.wide(),
                    thread: g.maybe(Gen::text),
                }),
            }),
            Done { ok: self.yes() },
            Error(ApiError::from(self.report(depth))),
            Opened { root: self.id() },
            Reached(Reach {
                items: self
                    .many(depth, |g| ReachedItem { path: g.text(), facts: g.many(1, Gen::fact) }),
                tests: self.texts(depth),
                changes: self.changes(depth),
                compiled: self.texts(depth),
            }),
            Ranked(self.many(depth, |g| Ranked {
                ok: g.yes(),
                root: g.id(),
                errors: g.u32(),
                failed: g.u32(),
                passed: g.u32(),
                warnings: g.u32(),
                candidate: g.u32(),
            })),
            Verified {
                mismatches: self.many(depth, |g| Mismatch {
                    fact: g.maybe(Gen::fact),
                    query: g.key(),
                    scratch: g.fingerprint(),
                    incremental: g.fingerprint(),
                }),
            },
            Snapshotted { heads: self.many(depth, |g| (g.text(), g.id())) },
            CodegenInputs(self.many(depth, |g| (g.id(), g.id()))),
            Trace(self.many(depth, |g| Traced {
                key: g.key(),
                ran: g.yes(),
                edges: g.edges(1),
                fingerprint: g.fingerprint(),
            })),
            Tables(self.many(depth, |g| Table {
                name: g.text(),
                bytes: g.wide(),
                elements: g.wide(),
            })),
            Edited(self.edited(depth)),
            Announced { root: self.id() },
            Memory(Memory {
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
            DocPage { page: self.bytes(), path: self.text() },
            Documented(Documented {
                dir: self.text(),
                pages: self.wide(),
                crates: self.many(depth, |g| DocumentedCrate {
                    name: g.text(),
                    asked: g.yes(),
                    package: g.text(),
                }),
                source_pages: self.wide(),
            }),
            Explored { link: self.text() },
            Profiled { trace: self.bytes() },
            Live(LiveFrame {
                lost: self.wide(),
                open: self.bytes(),
                at_ns: self.wide() as i64,
                spans: self.bytes(),
                skipped: self.wide(),
                counters: self.bytes(),
            }),
            Flushed { seq: self.wide() },
            Hot(crate::Hot {
                keys: self.many(depth, |g| crate::HotKey {
                    span: g.text(),
                    function: one_of!(g, crate::HotFunction { None, Named(g.text()), Forgotten(g.u32()) }),
                    lower_ns: g.wide(),
                    upper_ns: g.wide(),
                }),
                torn: self.wide(),
                since: self.hot_since(),
                spans: self.wide(),
                waits: self.many(depth, |g| crate::HotWait {
                    kind: one_of!(g, crate::HotWaitKind { Key, Lock, Channel, Io, Child, Scope }),
                    count: g.wide(),
                    max_ns: g.wide(),
                    p50_ns: g.wide(),
                    p90_ns: g.wide(),
                    p99_ns: g.wide(),
                }),
                error_ns: self.wide(),
                total_ns: self.wide(),
            }),
            Collected { report: self.text() },
            UnitBegan(self.unit_of()),
            TestReady(self.artifact()),
            Rebuilt(self.rebuilt(depth)),
            Launched { live: self.yes(), env: self.many(depth, |g| (g.text(), g.text())) },
            Stopping { doing: self.text() },
            Unchecked(Unchecked {
                open: self
                    .many(depth, |g| OpenFailure { unit: g.text(), checked: g.texts(depth + 1) }),
                behind: self.many(depth, |g| Behind {
                    id: g.maybe(Gen::u32),
                    why: g.text(),
                    unit: g.text(),
                    units: g.texts(depth + 1),
                }),
                tests: self.many(depth, |g| Unbuilt {
                    unit: g.text(),
                    behind: g.text(),
                    why: g.text(),
                }),
                stopped: self.yes(),
            }),
            Stored { path: self.maybe_text(), blake3: self.text() },
            Report { unit: self.maybe(Gen::unit_of), report: self.report(depth) },
            Doing(crate::Doing {
                on: self.maybe_text(),
                now: blaze_protocol::now::Now::default(),
                busy: self.u32(),
                idle: self.u32(),
                units: self.many(depth, |g| crate::UnitDoing { id: g.u32(), doing: g.text() }),
                for_ms: self.wide(),
                running: self.maybe_text(),
            }),
            Diagnostics(crate::Seen {
                new: self.many(depth, Gen::found),
                base: one_of!(self, crate::Base { First, Held, Unknown }),
                root: self.id(),
                build: if self.yes() { Some(self.wide()) } else { None },
                ended: self.yes(),
                fixed: self.many(depth, Gen::found),
                token: self.maybe_id(),
                failed: self.maybe(|g| ApiError::from(g.report(depth))),
                unchanged: self.many(depth, Gen::found),
            }),
            Proven(crate::Proven {
                failed: self.maybe(|g| g.text()),
                subset: self.maybe(|g| crate::Subset {
                    generic: g.u32(),
                    refused: g.many(depth, |g| crate::SubsetRefusal { what: g.text(), count: g.u32() }),
                    outcomes: g.many(depth, |g| crate::SubsetFunction {
                        path: g.text(),
                        outcome: one_of!(g, crate::SubsetOutcome {
                            Translated,
                            Generic,
                            Refused { what: g.text() },
                        }),
                    }),
                    functions: g.u32(),
                    translated: g.u32(),
                }),
                records: self.many(depth, |g| one_of!(g, crate::ProofRecord {
                    Open {
                        why: g.text(),
                        function: g.text(),
                        statement: g.text(),
                        proof_file: g.text(),
                    },
                    Proved {
                        key: g.id(),
                        model: g.text(),
                        axioms: g.texts(depth),
                        theorem: g.text(),
                        function: g.text(),
                    },
                    Emitted {
                        path: g.text(),
                        module: g.text(),
                        function: g.text(),
                        statement: g.text(),
                    },
                    Refused { at: g.text(), what: g.text(), function: g.text() },
                    ModelBroken {
                        at: g.text(),
                        error: g.text(),
                        module: g.text(),
                        function: g.text(),
                    },
                })),
            }),
            TestsOf(crate::TestsOf {
                tests: self.u32(),
                binary: self.text(),
                nearest: self.many(depth, |g| crate::Near { name: g.text(), distance: g.u32() }),
                executable: self.u32(),
                filtered_out: self.u32(),
            }),
            Rows(crate::query::Rows {
                findings: self.many(depth, |g| crate::query::Finding {
                    at: g.maybe(Gen::cell),
                    message: g.text(),
                    relation: g.text(),
                    severity: one_of!(g, crate::query::Severity { Error, Warning }),
                }),
                relations: self.many(depth, |g| crate::query::Relation {
                    name: g.text(),
                    rows: g.many(depth + 1, |g| g.many(depth + 1, Gen::cell)),
                    columns: g.texts(depth + 1),
                }),
            }),
            // Holds a response: only near the top, so a value ends.
            Of { response: Box::new(self.response(depth + 1)), candidate: self.u32() } // alloc: test and fuzz reference only
                if depth < 2,
        })
    }

    /// A report: its causes only near the top.
    fn report(&mut self, depth: u32) -> crate::Report {
        crate::Report {
            at: self.maybe(Gen::report_text),
            log: self.maybe_text(),
            rule: self.maybe_text(),
            facts: self.many(depth + 1, |g| crate::Fact { key: g.text(), text: g.report_text() }),
            fixes: self.many(depth + 1, |g| crate::Fix {
                note: g.maybe(Gen::report_text),
                lines: g.texts(depth + 2),
            }),
            blocks: self.maybe(|g| crate::Blocks {
                all: g.texts(depth + 1),
                what: g.text(),
                count: g.wide(),
                notable: g.texts(depth + 1),
            }),
            causes: match depth {
                0 | 1 => self.many(depth + 1, |g| g.report(depth + 1)),
                _ => Vec::new(),
            },
            headline: self.report_text(),
            severity: one_of!(self, crate::Severity { Error, Warning, Note }),
        }
    }

    fn report_text(&mut self) -> crate::Text {
        crate::Text(self.many(1, |g| one_of!(g, crate::ReportSpan {
            Plain(g.text()),
            Code(g.text()),
            Path(g.text()),
            Package { name: g.text(), version: g.maybe(Gen::text) },
            Version(g.text()),
            Hash(g.text()),
            Var(g.text()),
            Count(g.wide()),
            Took(g.wide()),
            Command(g.text()),
            Strong(g.text()),
        })))
    }

    fn cell(&mut self) -> crate::query::Cell {
        one_of!(self, crate::query::Cell {
            Code {
                end: self.u32(),
                line: self.maybe(Gen::u32),
                path: self.text(),
                text: self.text(),
                start: self.u32(),
            },
            Expanded { call: self.text(), text: self.text() },
            Def { path: self.text() },
            Type { text: self.text() },
            Text(self.text()),
            Int(self.next() as i64),
        })
    }

    fn maybe_id(&mut self) -> Option<ContentId> {
        if self.yes() { Some(self.id()) } else { None }
    }

    fn maybe_text(&mut self) -> Option<String> {
        if self.yes() { Some(self.text()) } else { None }
    }

    fn rebuilt(&mut self, depth: u32) -> crate::Rebuilt {
        crate::Rebuilt {
            torn: self.maybe_text(),
            build: if self.yes() { Some(self.wide()) } else { None },
            chain: self.yes(),
            count: self.u32(),
            groups: self.many(depth, |g| crate::RebuildGroup {
                step: g.text(),
                cause: g.rebuild_cause(depth),
                count: g.u32(),
                items: g.many(depth, |g| g.text()),
            }),
            command: self.maybe_text(),
            dropped: if self.yes() { Some(self.wide()) } else { None },
            rebuilt: self.many(depth, |g| crate::Rebuild {
                ends: g.many(depth, |g| g.reason()),
                item: g.text(),
                step: g.text(),
                cause: g.rebuild_cause(depth),
            }),
            summary: self.many(depth, |g| g.report(depth + 1)),
            restoring: self.yes(),
            workspace: self.maybe_text(),
        }
    }

    fn reason(&mut self) -> crate::Reason {
        one_of!(self, crate::Reason {
            InputChanged { file: self.yes(), input: self.text(), trivia: self.yes() },
            FirstTime,
            Evicted {
                by: one_of!(self, crate::EvictedBy { Memory, Age, Cap }),
                when: self.wide(),
            },
            PreviousFailed { since: self.wide() },
            Forced { how: self.text() },
            KeyUnstable { part: self.text() },
            Unexplained { what: self.text() },
        })
    }

    fn rebuild_cause(&mut self, depth: u32) -> crate::RebuildCause {
        one_of!(self, crate::RebuildCause {
            First,
            Parts {
                parts: self.u32(),
                since: self.wide(),
                changed: self.many(depth, |g| crate::PartChanged {
                    now: g.maybe_id(),
                    was: g.maybe_id(),
                    kind: one_of!(g, crate::PartIs { Input, File, Result }),
                    part: g.text(),
                    names: g.maybe_text(),
                    trivia: g.yes(),
                }),
            },
            Lost { since: self.wide() },
            Beyond { what: self.text(), since: self.wide() },
            Unnamed { since: self.wide() },
            Read {
                now: self.maybe_id(),
                was: self.maybe_id(),
                read: self.text(),
                names: self.maybe_text(),
                reads: self.u32(),
                since: if self.yes() { Some(self.wide()) } else { None },
                changed: self.u32(),
            },
            Unrecorded,
            Collected,
            Unshaped,
            Twice { count: self.u32() },
            Always { why: self.text() },
            NotExplained { what: self.text() },
            Unfinished { since: self.wide() },
            Dropped { since: self.wide() },
            Failed { why: self.text() },
        })
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
    let mut kept = vec![7]; // alloc: test and fuzz reference only
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
    fn lent(
        read: Result<Envelope<ResponseRef<'_>>, cbor::Error>,
    ) -> Result<Envelope<Response>, cbor::Error> {
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

/// The names of an enum's variants in declaration order, read by reflection.
macro_rules! variant_names {
    ($enum:ty) => {{
        const COUNT: usize = TypeId::of::<$enum>().variants();
        const NAMES: [&str; COUNT] = {
            let mut names = [""; COUNT];
            let mut at = 0;
            while at < COUNT {
                names[at] = TypeId::of::<$enum>().variant(at).name();
                at += 1;
            }
            names
        };
        NAMES
    }};
}

/// The variant `value` is: the name its derived `Debug` writes first.
fn variant_name(value: &impl Debug) -> String {
    let written = format!("{value:?}"); // alloc: test and fuzz reference only
    let mut words = written.split(|c: char| !c.is_alphanumeric() && c != '_');
    words.next().expect("split yields at least one piece").to_owned()
}

/// Of `names`, those not in `seen`.
fn never<'a>(names: &[&'a str], seen: &HashSet<String>) -> Vec<&'a str> {
    names.iter().copied().filter(|name| !seen.contains(*name)).collect() // alloc: test and fuzz reference only
}

#[test]
fn every_request_and_response_is_the_bytes_the_old_codec_wrote() {
    let mut g = Gen { rng: SplitMix64::new(0x2545_f491_4f6c_dd1d), plain: false };
    let mut writer = cbor::Writer::new();
    let (mut requests, mut responses) = (HashSet::new(), HashSet::new()); // alloc: test and fuzz reference only
    for round in 0..3000 {
        g.plain = round % 2 == 0;
        let envelope = Envelope::new(g.wide(), g.request());
        requests.insert(variant_name(&envelope.body));
        let in_order = same(&mut writer, &envelope);
        assert!(
            in_order || !g.plain,
            "a map of the API's own types written out of order (declare fields by key, shorter \
             first, then bytewise): {envelope:?}"
        );
        let envelope = Envelope::new(g.wide(), g.response(0));
        responses.insert(variant_name(&envelope.body));
        same_lent(&cbor::to_vec(&envelope).unwrap());
        let in_order = same(&mut writer, &envelope);
        assert!(
            in_order || !g.plain,
            "a map of the API's own types written out of order (declare fields by key, shorter \
             first, then bytewise): {envelope:?}"
        );
    }
    // `one_of!` makes every variant have an arm; this holds that each arm's condition lets it
    // be drawn.
    let unseen = never(&variant_names!(Request), &requests);
    assert!(unseen.is_empty(), "requests never generated: {unseen:?}");
    let unseen = never(&variant_names!(Response), &responses);
    assert!(unseen.is_empty(), "responses never generated: {unseen:?}");
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
