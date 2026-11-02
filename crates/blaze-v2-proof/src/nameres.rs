//! The oracle's cases for name resolution and the impl buckets (docs/v2/plan/design/frontend.md
//! items 1 to 3; docs/v2/plan/lanes/frontend/nameres-oracle.md). Each [`Case`] is one slot of
//! `harness/v2/fixture.py` aimed at them, and says what an edit of that slot, between any two
//! of its variants, may change and must rerun. [`check`] holds three traces to it: the build
//! before the edit, the incremental build after it and the build from scratch of the same
//! source.
//!
//! A case counts only keys that name no expansion round: the facts of
//! docs/v2/plan/lanes/items.md section 6.2 (`Resolves`, `Implements`, `Method`), `module_decls`
//! and `module_impls`. So it holds however many rounds a crate needs. The keys that do name one
//! (`scc_state`, `macro_step`, `fact_handle`, `ModuleScope::name`, ...) are held to the rules
//! that need no count: a result that reran read something that changed, a result kept read
//! nothing that did, none reran for nothing, none is stale against the build from scratch.
//!
//! Each [`Finding`] is on one [`Side`]: over (the build reran more than the facts say), under
//! (less: stale), or unmeasured (the traces hold no key of a kind the case counts).

use std::ops::RangeInclusive;

use blaze_ids::Fingerprint;
use blaze_query::{QueryKey, QueryKind};
use serde::Serialize;

use crate::{Stale, Trace, same_edges, under};

/// The kinds the cases read. Kind numbers are a schema: one number is one query for every
/// blaze, never given to another, so these are the defining crates' numbers.
pub mod kinds {
    use blaze_query::QueryKind;

    /// `blaze_items::kinds::METHOD`: what `x.m()` resolves to on a simplified self type.
    pub const METHOD: QueryKind = QueryKind(0x1003);
    /// `blaze_items::kinds::IMPLEMENTS`: the impls of a trait for a simplified self type.
    pub const IMPLEMENTS: QueryKind = QueryKind(0x1004);
    /// `blaze_items::kinds::RESOLVES`: what a name is in a scope, globs included.
    pub const RESOLVES: QueryKind = QueryKind(0x1005);

    // hir-def's resolver: the numbers of `hir_def::nameres::kinds`
    // (`crates/hir-def/src/nameres/facts/kinds.rs`) the cases name.
    /// `module_decls`: one module's round-0 declarations. Names no round.
    pub const MODULE_DECLS: QueryKind = QueryKind(0x2200);
    pub const SCC_STATE: QueryKind = QueryKind(0x2208);
    pub const FACT_HANDLE: QueryKind = QueryKind(0x2209);
    pub const MACRO_STEP: QueryKind = QueryKind(0x220a);
    /// `ModuleScope::name`: the per-name fact a resolution read of a module of another component.
    pub const MODULE_SCOPE_NAME: QueryKind = QueryKind(0x220b);
    /// `module_impls`: one module's impls in declaration order. Names no round.
    pub const MODULE_IMPLS: QueryKind = QueryKind(0x220c);

    /// The numbers the resolver may give (`hir_def::nameres::kinds::RANGE`): every one of its
    /// queries, those it adds later included.
    pub const RESOLVER: std::ops::Range<u16> = 0x2200..0x2240;
}

/// What a case says of one kind of key: how many keys of it with a result in both the build
/// before the edit and the incremental one after it come out with another fingerprint.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Expect {
    pub kind: QueryKind,
    pub what: &'static str,
    pub changed: RangeInclusive<u32>,
}

/// One slot of `harness/v2/fixture.py` and what an edit of it, between any two of its
/// variants, does to the trace.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Case {
    pub slot: &'static str,
    pub claim: &'static str,
    pub expect: &'static [Expect],
}

const fn expect(kind: QueryKind, what: &'static str, changed: RangeInclusive<u32>) -> Expect {
    Expect { kind, what, changed }
}

const NAMES: &str = "names resolved";
const DECLS: &str = "modules' declarations";
const BUCKETS: &str = "impl buckets (trait, simplified self type)";
const PROBES: &str = "method probes (simplified self type, name)";
const IMPLS: &str = "modules' impl lists";

pub static CASES: &[Case] = &[
    Case {
        slot: "nameres-item",
        claim: "an item or a `use` added to `oracle_base::a` answers no name anything resolved: \
                the root's `use a::triple` read `a`'s `triple`, not `a`; only `a`'s declarations \
                change",
        expect: &[expect(kinds::RESOLVES, NAMES, 0..=0), expect(kinds::MODULE_DECLS, DECLS, 1..=1)],
    },
    Case {
        slot: "nameres-glob",
        claim: "a name added to `helpers`, which `chooser` globs, is read by nothing: no resolved \
                name changes, only `helpers`' declarations, and only the glob's component reruns",
        expect: &[expect(kinds::RESOLVES, NAMES, 0..=0), expect(kinds::MODULE_DECLS, DECLS, 1..=1)],
    },
    Case {
        slot: "nameres-negative",
        claim: "`drop` appears in `helpers`, which `chooser` globs: the negative fact \"`drop` is \
                not in `chooser`\" (so the prelude's) flips, and `released`, which read it, reruns",
        expect: &[expect(kinds::RESOLVES, NAMES, 1..=1), expect(kinds::MODULE_DECLS, DECLS, 1..=1)],
    },
    Case {
        slot: "shadow",
        claim: "a local `pick` in `chooser` shadows the glob's: `pick` in `chooser` changes and \
                nothing else",
        expect: &[expect(kinds::RESOLVES, NAMES, 1..=1), expect(kinds::MODULE_DECLS, DECLS, 1..=1)],
    },
    Case {
        slot: "nameres-bucket",
        claim: "`impl Weight for u32` added, which nothing asks: the buckets asked, (Weight, u8) \
                and (Weight, u16), stay, and so do the probes that read them",
        expect: &[
            expect(kinds::IMPLEMENTS, BUCKETS, 0..=0),
            expect(kinds::METHOD, PROBES, 0..=0),
            expect(kinds::MODULE_IMPLS, IMPLS, 1..=1),
        ],
    },
    Case {
        slot: "specific",
        claim: "`impl Specific for Marker` added or removed: the bucket (Specific, Marker) flips, \
                the blanket `Fallback for &T` bucket stays, at most the probe of `which` on \
                `Marker` changes",
        expect: &[
            expect(kinds::IMPLEMENTS, BUCKETS, 1..=1),
            expect(kinds::METHOD, PROBES, 0..=1),
            expect(kinds::MODULE_IMPLS, IMPLS, 1..=1),
        ],
    },
    Case {
        slot: "nameres-inherent",
        claim: "an inherent `Probe::name` added: the probe of `name` on `Probe` changes and no \
                other probe, and no trait bucket",
        expect: &[
            expect(kinds::METHOD, PROBES, 1..=1),
            expect(kinds::IMPLEMENTS, BUCKETS, 0..=0),
            expect(kinds::MODULE_IMPLS, IMPLS, 1..=1),
        ],
    },
];

/// The case of a fixture slot, if it has one.
pub fn case(slot: &str) -> Option<&'static Case> {
    CASES.iter().find(|case| case.slot == slot)
}

/// Which way a finding is wrong.
#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum Side {
    /// The build reran more than the facts say.
    Over,
    /// The build reran less: a result is stale.
    Under,
    /// The traces cannot say.
    Unmeasured,
}

/// Where the traces are not what a [`Case`] says.
#[derive(Serialize, Clone, PartialEq, Eq, Debug)]
pub enum Finding {
    /// Another number of keys of the kind changed than the case allows; `keys` are those that
    /// did.
    Changed { what: &'static str, kind: QueryKind, changed: u32, min: u32, max: u32, keys: Vec<QueryKey> },
    /// Neither trace has a result of a kind the case counts.
    Unrecorded { what: &'static str, kind: QueryKind },
    /// Kept by the incremental build though `read`, of a kind the case watches, is not what it
    /// saw (`now`, `None` when the build has no result for it): its reader did not rerun
    /// ([`Stale::Unverified`] of a watched read).
    Kept { query: QueryKey, read: QueryKey, saw: Fingerprint, now: Option<Fingerprint> },
    /// Ran in the incremental build though everything it read in the build before is as it was.
    Unexplained { query: QueryKey },
    /// Ran, read and made what it did before ([`crate::over`]'s needless).
    Needless { query: QueryKey },
    /// A watched result stale against the build from scratch ([`under`]).
    Stale(Stale),
}

impl Finding {
    pub fn side(&self) -> Side {
        match self {
            Finding::Changed { changed, max, .. } if changed > max => Side::Over,
            Finding::Changed { .. } | Finding::Kept { .. } | Finding::Stale(_) => Side::Under,
            Finding::Unexplained { .. } | Finding::Needless { .. } => Side::Over,
            Finding::Unrecorded { .. } => Side::Unmeasured,
        }
    }
}

/// The indices of `trace`'s keys of `kind`: keys sort by kind first, so they are one run.
pub(crate) fn of_kind(trace: &Trace, kind: QueryKind) -> std::ops::Range<usize> {
    let start = trace.keys.partition_point(|k| k.kind < kind);
    start..start + trace.keys[start..].partition_point(|k| k.kind == kind)
}

/// The keys of `kind` with a result in both traces whose fingerprints differ, in key order, and
/// whether either trace has a result of the kind: [`crate::apart`]'s `Differ`s over the two runs of
/// the kind, by one merge of the sorted columns with nothing allocated but the answer.
fn differ(before: &Trace, after: &Trace, kind: QueryKind) -> (Vec<QueryKey>, bool) {
    let (xs, ys) = (of_kind(before, kind), of_kind(after, kind));
    let any = (xs.start..xs.end).any(|i| before.has.contains(i as u32))
        || (ys.start..ys.end).any(|i| after.has.contains(i as u32));
    let (mut x, mut y) = (xs.start, ys.start);
    // alloc: the answer, the keys that changed; unallocated while none did.
    let mut keys = Vec::new();
    while x < xs.end && y < ys.end {
        match before.keys[x].cmp(&after.keys[y]) {
            std::cmp::Ordering::Less => x += 1,
            std::cmp::Ordering::Greater => y += 1,
            std::cmp::Ordering::Equal => {
                if let (Some(was), Some(now)) = (before.result(x as u32), after.result(y as u32))
                    && was != now
                {
                    keys.push(before.keys[x]);
                }
                x += 1;
                y += 1;
            }
        }
    }
    (keys, any)
}

/// Where `before`, `incremental` and `scratch` are not what `case` says, in this order: the
/// counts of its [`Expect`]s; results that reran though nothing they read had changed, and
/// those that also made what they did before ([`crate::over`]'s needless); then what [`under`] finds,
/// a result kept though a watched read changed as [`Finding::Kept`]. A kind is watched when the
/// case counts it or it is the resolver's ([`kinds::RESOLVER`]).
///
/// One pass over each counted kind's keys, one join of the incremental build's reruns against
/// the build before, and [`under`]'s one pass: the traces are read once each.
pub fn check(case: &Case, before: &Trace, incremental: &Trace, scratch: &Trace) -> Vec<Finding> {
    let watched = |kind: QueryKind| kinds::RESOLVER.contains(&kind.0) || case.expect.iter().any(|e| e.kind == kind);
    // alloc: the answer, once a check of a fixture slot's edit; unallocated while nothing is found.
    let mut found = Vec::new();
    for e in case.expect {
        let (keys, any) = differ(before, incremental, e.kind);
        if !any {
            found.push(Finding::Unrecorded { what: e.what, kind: e.kind });
            continue;
        }
        let changed = u32::try_from(keys.len()).expect("a trace has fewer than 2^32 keys");
        if !e.changed.contains(&changed) {
            let (min, max) = (*e.changed.start(), *e.changed.end());
            found.push(Finding::Changed { what: e.what, kind: e.kind, changed, min, max, keys });
        }
    }
    // alloc: as above, the needless reruns, put after the rest.
    let mut needless = Vec::new();
    let mut cursor = 0;
    for i in 0..incremental.keys.len() as u32 {
        let query = incremental.keys[i as usize];
        if !incremental.ran.contains(i) || !watched(query.kind) {
            continue;
        }
        let Some(now) = incremental.result(i) else { continue };
        let Some(j) = before.index_from(&mut cursor, &query) else { continue };
        let Some(was) = before.result(j) else { continue };
        let changed = before.edges(j).any(|(read, saw)| {
            incremental.index(&before.keys[read as usize]).and_then(|r| incremental.result(r)) != Some(saw)
        });
        if !changed {
            found.push(Finding::Unexplained { query });
        }
        if was == now && same_edges(before, j, incremental, i) {
            needless.push(Finding::Needless { query });
        }
    }
    found.append(&mut needless);
    for stale in under(incremental, scratch, &[]) {
        match stale {
            Stale::Unverified { query, read, saw, now } if watched(read.kind) => {
                found.push(Finding::Kept { query, read, saw, now });
            }
            stale if watched(stale.query().kind) => found.push(Finding::Stale(stale)),
            _ => {}
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use blaze_ids::{ContentId, Fingerprint};
    use blaze_query::{Edge, QueryKey, QueryKind, kinds::FILE};

    use super::{kinds::*, *};
    use crate::{Trace, TraceBuilder};

    /// A query that reads facts and no resolver state: a body's inference.
    const INFER: QueryKind = QueryKind(0x2400);

    fn key(kind: QueryKind, name: &str) -> QueryKey {
        QueryKey { kind, input: ContentId::of("blaze v2 proof nameres test 1", name.as_bytes()) }
    }

    struct Node {
        key: QueryKey,
        was: u128,
        now: u128,
        reads: Vec<usize>,
    }

    /// A query graph with each result's fingerprint before the edit and after it, made into the
    /// traces of the three builds as a correct database would: the incremental build reruns an
    /// input that changed and a result one of whose reads did, and keeps the rest.
    #[derive(Default)]
    struct Model {
        nodes: Vec<Node>,
    }

    impl Model {
        fn add(&mut self, kind: QueryKind, name: &str, was: u128, now: u128, reads: &[usize]) -> usize {
            assert!(reads.iter().all(|&r| r < self.nodes.len()), "{name} reads a node added after it");
            self.nodes.push(Node { key: key(kind, name), was, now, reads: reads.to_vec() });
            self.nodes.len() - 1
        }

        fn at(&self, kind: QueryKind, name: &str) -> usize {
            let k = key(kind, name);
            self.nodes.iter().position(|n| n.key == k).unwrap_or_else(|| panic!("no node {name}"))
        }

        fn whole(&self, now: bool) -> Trace {
            let fp = |n: &Node| if now { n.now } else { n.was };
            let mut trace = TraceBuilder::default();
            for node in &self.nodes {
                let edges: Vec<Edge> =
                    node.reads.iter().map(|&r| Edge { read: self.nodes[r].key, saw: Fingerprint(fp(&self.nodes[r])) }).collect();
                trace.insert(node.key, Fingerprint(fp(node)), &edges, true);
            }
            trace.finish()
        }

        fn before(&self) -> Trace {
            self.whole(false)
        }

        fn scratch(&self) -> Trace {
            self.whole(true)
        }

        /// `forced` overrides whether a node reruns: `true` reruns it, `false` keeps it.
        fn incremental(&self, forced: &[(usize, bool)]) -> Trace {
            let mut fp = vec![0u128; self.nodes.len()];
            let mut trace = TraceBuilder::default();
            for (i, node) in self.nodes.iter().enumerate() {
                let natural = if node.reads.is_empty() {
                    node.was != node.now
                } else {
                    node.reads.iter().any(|&r| fp[r] != self.nodes[r].was)
                };
                let ran = forced.iter().find(|&&(at, _)| at == i).map_or(natural, |&(_, ran)| ran);
                fp[i] = if ran { node.now } else { node.was };
                let edges: Vec<Edge> = node
                    .reads
                    .iter()
                    .map(|&r| Edge { read: self.nodes[r].key, saw: Fingerprint(if ran { fp[r] } else { self.nodes[r].was }) })
                    .collect();
                trace.insert(node.key, Fingerprint(fp[i]), &edges, ran);
            }
            trace.finish()
        }

        fn check(&self, slot: &str, forced: &[(usize, bool)]) -> Vec<Finding> {
            check(case(slot).unwrap(), &self.before(), &self.incremental(forced), &self.scratch())
        }
    }

    fn per_round(m: &mut Model, rounds: u32, prev: &mut Option<usize>, mut add: impl FnMut(&mut Model, u32, &[usize]) -> usize) {
        for r in 0..rounds {
            let reads: Vec<usize> = prev.iter().copied().collect();
            *prev = Some(add(m, r, &reads));
        }
    }

    /// `nameres-item`: `a` gains an item; the root imports `a::triple` through the per-name fact.
    /// `coarse`: the root's `Resolves` fact reads `a`'s whole state and changes with it.
    fn item_model(rounds: u32, coarse: bool) -> Model {
        let mut m = Model::default();
        let file_a = m.add(FILE, "base/src/a.rs", 1, 2, &[]);
        let file_lib = m.add(FILE, "base/src/lib.rs", 3, 3, &[]);
        let decls_a = m.add(MODULE_DECLS, "a", 10, 11, &[file_a]);
        let decls_root = m.add(MODULE_DECLS, "root", 12, 12, &[file_lib]);
        let (mut scc_a, mut scc_root) = (None, None);
        for r in 0..rounds {
            let base = u128::from(r) * 1000;
            let reads: Vec<usize> = [decls_a].into_iter().chain(scc_a).collect();
            let state = m.add(SCC_STATE, &format!("a@{r}"), base + 20, base + 21, &reads);
            scc_a = Some(state);
            let handle = m.add(FACT_HANDLE, &format!("a@{r}"), base + 30, base + 31, &[state]);
            let triple = m.add(MODULE_SCOPE_NAME, &format!("a@{r} triple"), 40, 40, &[handle]);
            let reads: Vec<usize> = [decls_root, triple].into_iter().chain(scc_root).collect();
            let root = m.add(SCC_STATE, &format!("root@{r}"), base + 50, base + 50, &reads);
            scc_root = Some(root);
            m.add(MACRO_STEP, &format!("root@{r}"), base + 60, base + 60, &[root]);
        }
        let (scc_a, scc_root) = (scc_a.unwrap(), scc_root.unwrap());
        let resolves = match coarse {
            true => m.add(RESOLVES, "root triple", 70, 71, &[scc_a]),
            false => m.add(RESOLVES, "root triple", 70, 70, &[scc_root]),
        };
        let beta = m.add(RESOLVES, "b beta", 72, 72, &[decls_root]);
        m.add(INFER, "main", 80, 80, &[resolves, beta]);
        m
    }

    /// `nameres-negative`: `helpers` gains `drop`; `chooser` globs `helpers`, so its state reads
    /// `helpers`' whole scope; `released` reads `drop` in `chooser`. `recorded`: whether
    /// `released`'s read of `drop` is an edge.
    fn negative_model(rounds: u32, recorded: bool) -> Model {
        let mut m = Model::default();
        let file = m.add(FILE, "mid/src/lib.rs", 1, 2, &[]);
        let decls_helpers = m.add(MODULE_DECLS, "helpers", 10, 11, &[file]);
        let decls_chooser = m.add(MODULE_DECLS, "chooser", 12, 12, &[file]);
        let (mut helpers, mut chooser) = (None, None);
        for r in 0..rounds {
            let base = u128::from(r) * 1000;
            let reads: Vec<usize> = [decls_helpers].into_iter().chain(helpers).collect();
            let state = m.add(SCC_STATE, &format!("helpers@{r}"), base + 20, base + 21, &reads);
            helpers = Some(state);
            let handle = m.add(FACT_HANDLE, &format!("helpers@{r}"), base + 30, base + 31, &[state]);
            let reads: Vec<usize> = [decls_chooser, handle].into_iter().chain(chooser).collect();
            let glob = m.add(SCC_STATE, &format!("chooser@{r}"), base + 40, base + 41, &reads);
            chooser = Some(glob);
            m.add(MACRO_STEP, &format!("chooser@{r}"), base + 50, base + 50, &[glob]);
        }
        let chooser = chooser.unwrap();
        let pick = m.add(RESOLVES, "chooser pick", 62, 62, &[chooser]);
        m.add(INFER, "chosen", 64, 64, &[pick]);
        match recorded {
            true => {
                let drop = m.add(RESOLVES, "chooser drop", 60, 61, &[chooser]);
                m.add(INFER, "released", 70, 71, &[drop]);
            }
            false => {
                m.add(INFER, "released", 70, 71, &[pick]);
            }
        }
        m
    }

    /// `nameres-bucket`: `probe` gains `impl Weight for u32`. `coarse`: the (Weight, u8) bucket
    /// reads every impl of `Weight` and changes with them.
    fn bucket_model(coarse: bool) -> Model {
        let mut m = Model::default();
        let file = m.add(FILE, "base/src/probe.rs", 1, 2, &[]);
        let impls = m.add(MODULE_IMPLS, "probe", 10, 11, &[file]);
        let u8_ = m.add(IMPLEMENTS, "Weight u8", 20, if coarse { 21 } else { 20 }, &[impls]);
        let u16_ = m.add(IMPLEMENTS, "Weight u16", 22, 22, &[impls]);
        let method = m.add(METHOD, "u16 weight", 24, 24, &[u16_]);
        m.add(INFER, "main", 30, 30, &[u8_, method]);
        m
    }

    /// `nameres-inherent`: `probe` gains `impl Probe { fn name }`. `coarse`: the probe of `which`
    /// on `Marker` reads every inherent impl of the module and changes with them.
    fn inherent_model(coarse: bool) -> Model {
        let mut m = Model::default();
        let file = m.add(FILE, "base/src/probe.rs", 1, 2, &[]);
        let impls = m.add(MODULE_IMPLS, "probe", 10, 11, &[file]);
        let name = m.add(METHOD, "Probe name", 20, 21, &[impls]);
        let named = m.add(IMPLEMENTS, "Named Probe", 22, 22, &[impls]);
        let which = m.add(METHOD, "Marker which", 24, if coarse { 25 } else { 24 }, &[impls]);
        m.add(INFER, "main", 30, 31, &[name, named, which]);
        m
    }

    #[test]
    fn an_item_or_use_added_to_a_module_reruns_only_what_read_it_whatever_the_rounds() {
        for rounds in 1..=4 {
            assert_eq!(item_model(rounds, false).check("nameres-item", &[]), vec![], "{rounds} rounds");
        }
    }

    #[test]
    fn a_name_fact_that_reads_the_whole_module_is_over_the_same_at_every_round_count() {
        for rounds in 1..=4 {
            let found = item_model(rounds, true).check("nameres-item", &[]);
            let want = Finding::Changed {
                what: NAMES,
                kind: RESOLVES,
                changed: 1,
                min: 0,
                max: 0,
                keys: vec![key(RESOLVES, "root triple")],
            };
            assert_eq!(found, vec![want], "{rounds} rounds");
            assert_eq!(found[0].side(), Side::Over);
        }
    }

    #[test]
    fn a_name_added_to_a_glob_source_reruns_only_the_glob_s_component() {
        // The glob case is the negative one without the reader: nothing asks the new name.
        let mut m = Model::default();
        let file = m.add(FILE, "mid/src/lib.rs", 1, 2, &[]);
        let decls_helpers = m.add(MODULE_DECLS, "helpers", 10, 11, &[file]);
        let decls_chooser = m.add(MODULE_DECLS, "chooser", 12, 12, &[file]);
        let decls_left = m.add(MODULE_DECLS, "left", 14, 14, &[file]);
        let (mut helpers, mut chooser, mut left) = (None, None, None);
        per_round(&mut m, 3, &mut helpers, |m, r, prev| {
            let reads: Vec<usize> = [decls_helpers].iter().chain(prev).copied().collect();
            m.add(SCC_STATE, &format!("helpers@{r}"), 20 + u128::from(r), 120 + u128::from(r), &reads)
        });
        per_round(&mut m, 3, &mut chooser, |m, r, prev| {
            let reads: Vec<usize> = [decls_chooser, helpers.unwrap()].iter().chain(prev).copied().collect();
            m.add(SCC_STATE, &format!("chooser@{r}"), 40 + u128::from(r), 140 + u128::from(r), &reads)
        });
        per_round(&mut m, 3, &mut left, |m, r, prev| {
            let reads: Vec<usize> = [decls_left].iter().chain(prev).copied().collect();
            m.add(SCC_STATE, &format!("left@{r}"), 60 + u128::from(r), 60 + u128::from(r), &reads)
        });
        let pick = m.add(RESOLVES, "chooser pick", 70, 70, &[chooser.unwrap()]);
        let side = m.add(RESOLVES, "left side", 72, 72, &[left.unwrap()]);
        m.add(INFER, "main", 80, 80, &[pick, side]);
        assert_eq!(m.check("nameres-glob", &[]), vec![]);
        // `left` is another component: rerunning its state is a rerun the facts don't explain.
        let left = m.at(SCC_STATE, "left@0");
        let found = m.check("nameres-glob", &[(left, true)]);
        let k = key(SCC_STATE, "left@0");
        assert_eq!(found, vec![Finding::Unexplained { query: k }, Finding::Needless { query: k }]);
        assert!(found.iter().all(|f| f.side() == Side::Over));
    }

    #[test]
    fn a_negative_fact_flips_and_its_reader_reruns() {
        for rounds in 1..=3 {
            assert_eq!(negative_model(rounds, true).check("nameres-negative", &[]), vec![], "{rounds} rounds");
        }
    }

    #[test]
    fn a_reader_kept_though_its_negative_fact_flipped_is_under() {
        let m = negative_model(2, true);
        let released = m.at(INFER, "released");
        let found = m.check("nameres-negative", &[(released, false)]);
        let (query, read) = (key(INFER, "released"), key(RESOLVES, "chooser drop"));
        let (saw, now) = (Fingerprint(60), Some(Fingerprint(61)));
        assert_eq!(found, vec![Finding::Kept { query, read, saw, now }]);
        assert_eq!(found[0].side(), Side::Under);
    }

    #[test]
    fn a_negative_read_with_no_edge_is_under() {
        let found = negative_model(2, false).check("nameres-negative", &[]);
        assert_eq!(found, vec![Finding::Changed { what: NAMES, kind: RESOLVES, changed: 0, min: 1, max: 1, keys: vec![] }]);
        assert_eq!(found[0].side(), Side::Under);
    }

    #[test]
    fn a_rerun_of_a_module_whose_source_did_not_change_is_over() {
        let m = item_model(2, false);
        let root = m.at(MODULE_DECLS, "root");
        let k = key(MODULE_DECLS, "root");
        assert_eq!(m.check("nameres-item", &[(root, true)]), vec![Finding::Unexplained { query: k }, Finding::Needless { query: k }]);
    }

    #[test]
    fn a_kind_neither_trace_has_is_unmeasured() {
        let mut m = Model::default();
        let file = m.add(FILE, "base/src/a.rs", 1, 2, &[]);
        m.add(MODULE_DECLS, "a", 10, 11, &[file]);
        let found = m.check("nameres-item", &[]);
        assert_eq!(found, vec![Finding::Unrecorded { what: NAMES, kind: RESOLVES }]);
        assert_eq!(found[0].side(), Side::Unmeasured);
    }

    #[test]
    fn an_impl_reruns_only_goals_of_its_bucket() {
        assert_eq!(bucket_model(false).check("nameres-bucket", &[]), vec![]);
        let found = bucket_model(true).check("nameres-bucket", &[]);
        assert_eq!(
            found,
            vec![Finding::Changed { what: BUCKETS, kind: IMPLEMENTS, changed: 1, min: 0, max: 0, keys: vec![key(IMPLEMENTS, "Weight u8")] }]
        );
        assert_eq!(found[0].side(), Side::Over);
    }

    #[test]
    fn an_inherent_method_reruns_only_probes_of_its_self_type() {
        assert_eq!(inherent_model(false).check("nameres-inherent", &[]), vec![]);
        let found = inherent_model(true).check("nameres-inherent", &[]);
        let mut keys = vec![key(METHOD, "Probe name"), key(METHOD, "Marker which")];
        keys.sort();
        assert_eq!(found, vec![Finding::Changed { what: PROBES, kind: METHOD, changed: 2, min: 1, max: 1, keys }]);
        assert_eq!(found[0].side(), Side::Over);
    }

    #[test]
    fn a_key_only_one_build_has_is_not_a_change() {
        // A fact asked only after the edit (a new probe) is new, not changed.
        let mut m = item_model(1, false);
        let before = m.before();
        let reads = [m.at(SCC_STATE, "root@0")];
        let gamma = m.add(RESOLVES, "root gamma", 90, 90, &reads);
        let found = check(case("nameres-item").unwrap(), &before, &m.incremental(&[(gamma, true)]), &m.scratch());
        assert_eq!(found, vec![]);
    }

    #[test]
    fn differ_finds_what_apart_finds() {
        use std::collections::BTreeMap;

        let build = |rng: &mut crate::Rng| -> Trace {
            let mut trace = TraceBuilder::default();
            for _ in 0..rng.below(60) {
                let kind = [RESOLVES, METHOD, IMPLEMENTS][rng.below(3) as usize];
                let k = key(kind, &rng.below(40).to_string());
                // Some read a key no build has a result for: named, not counted.
                let read = key(kind, "only read");
                let edges = if rng.below(4) == 0 { vec![Edge { read, saw: Fingerprint(0) }] } else { Vec::new() };
                trace.insert(k, Fingerprint(u128::from(rng.below(3) as u8)), &edges, true);
            }
            trace.finish()
        };
        let map = |trace: &Trace, kind: QueryKind| -> BTreeMap<QueryKey, Fingerprint> {
            (0..trace.keys.len() as u32)
                .filter(|&i| trace.keys[i as usize].kind == kind)
                .filter_map(|i| Some((trace.keys[i as usize], trace.result(i)?)))
                .collect()
        };
        for seed in 0..200 {
            let mut rng = crate::Rng::new(seed);
            let (a, b) = (build(&mut rng), build(&mut rng));
            for kind in [RESOLVES, METHOD, IMPLEMENTS, MODULE_DECLS] {
                let (x, y) = (map(&a, kind), map(&b, kind));
                let want: Vec<QueryKey> = crate::apart(&x, &y)
                    .into_iter()
                    .filter_map(|it| match it {
                        crate::Apart::Differ { key, .. } => Some(key),
                        crate::Apart::OnlyFirst { .. } | crate::Apart::OnlySecond { .. } => None,
                    })
                    .collect();
                assert_eq!(differ(&a, &b, kind), (want, !x.is_empty() || !y.is_empty()), "seed {seed}");
            }
        }
    }

    #[test]
    fn the_kinds_are_distinct_and_in_their_ranges() {
        let named = [MODULE_DECLS, SCC_STATE, FACT_HANDLE, MACRO_STEP, MODULE_SCOPE_NAME, MODULE_IMPLS];
        let mut all: Vec<QueryKind> = named.to_vec();
        all.extend([METHOD, IMPLEMENTS, RESOLVES]);
        let mut sorted = all.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), all.len());
        assert!(named.iter().all(|k| RESOLVER.contains(&k.0)));
        assert!(blaze_query::kinds::FRONT.contains(&RESOLVER.start));
        assert!(blaze_query::kinds::FRONT.contains(&(RESOLVER.end - 1)));
        assert!([METHOD, IMPLEMENTS, RESOLVES].iter().all(|k| blaze_query::kinds::FACTS.contains(&k.0)));
    }

    #[test]
    fn every_case_is_a_slot_of_the_fixture_and_in_its_nameres_regression() {
        let fixture = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../harness/v2/fixture.py")).unwrap();
        let start = fixture.find("\"nameres\": (").expect("fixture.py has a `nameres` regression");
        let regression = &fixture[start..start + fixture[start..].find(")),\n").unwrap()];
        for case in CASES {
            assert!(fixture.contains(&format!("Slot(\"{}\"", case.slot)), "no slot {} in fixture.py", case.slot);
            assert!(regression.contains(&format!("(\"{}\",", case.slot)), "the nameres regression never edits {}", case.slot);
            assert!(!case.expect.is_empty() && !case.claim.is_empty());
        }
        assert_eq!(CASES.iter().map(|c| c.slot).collect::<std::collections::BTreeSet<_>>().len(), CASES.len());
    }
}
