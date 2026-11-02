//! The oracle's cases for lane dbsp's maintained name resolution and impl tables
//! (docs/v2/plan/design/dbsp.md section 12, "a reader sees only its slice"; notes in
//! docs/v2/plan/lanes/dbsp/proof.md). Each [`Case`] is one kind of edit of
//! `harness/v2/dbsp/edits.py` and says what the build after it may rerun. [`check`] holds two
//! traces to it, the build before the edit and the incremental build after it, given what the
//! edit [`Touched`]: how many traits' impl sets, and how many (trait, self type) buckets, it
//! changed, which the harness knows from its model of what it wrote.
//!
//! A rerun is a key that ran in the build after the edit and has a result in the build before
//! it; a key first asked after the edit is new, not a rerun. Every case holds that:
//!
//! - no round query reruns ([`kinds::ROUNDS`]): the maintainers replace the round driver
//!   (design sections 4 and 10), whatever the edit;
//! - a trait's slice reruns only for a trait the edit touched, a bucket only for a bucket it
//!   touched, and no inherent slice at all (the harness writes no inherent impl): after an impl
//!   edit no query of another trait's slice reruns;
//! - no sliced query reads a whole crate's table ([`kinds::WHOLE`]), which would make it rerun
//!   with every impl of the crate ([`Finding::Coarse`]);
//! - no watched query (the resolver's, the slices' and those a case counts) reruns though
//!   nothing it read changed, nor reruns to read and make what it did before: after an item
//!   edit no resolver query of another module reruns, since no other module's source changed.

use blaze_query::{QueryKey, QueryKind};
use serde::{Deserialize, Serialize};

use crate::{
    join::{self, Columns, of_kind, result},
    nameres::Side,
};

/// The kinds the cases read. Kind numbers are a schema: one number is one query for every
/// blaze, never given to another, so these are the defining crates' numbers.
pub mod kinds {
    use blaze_query::QueryKind;

    pub use crate::nameres::kinds::{FACT_HANDLE, MACRO_STEP, MODULE_DECLS, MODULE_IMPLS, RESOLVER, SCC_STATE};

    // hir-def's resolver: the numbers of `hir_def::nameres::kinds`
    // (`crates/hir-def/src/nameres/facts/kinds.rs`) that name an expansion round.
    pub const MODULE_ROUND: QueryKind = QueryKind(0x2203);
    pub const IMPORT_SHAPE: QueryKind = QueryKind(0x2204);
    pub const PARTITION: QueryKind = QueryKind(0x2205);
    pub const LATEST: QueryKind = QueryKind(0x2207);
    pub const ROUND_MODULES: QueryKind = QueryKind(0x2212);
    pub const ROUND_ENV: QueryKind = QueryKind(0x2213);
    pub const UNRESOLVED_NAMED: QueryKind = QueryKind(0x2214);
    pub const ROUND_PROGRESS: QueryKind = QueryKind(0x2215);
    pub const ROUND_KIND: QueryKind = QueryKind(0x2216);
    pub const ROUND_ACTIVE: QueryKind = QueryKind(0x2218);

    /// The round driver's queries, which no edit reruns once the maintainers publish the scopes.
    pub const ROUNDS: [QueryKind; 13] = [
        MODULE_ROUND,
        IMPORT_SHAPE,
        PARTITION,
        LATEST,
        SCC_STATE,
        FACT_HANDLE,
        MACRO_STEP,
        ROUND_MODULES,
        ROUND_ENV,
        UNRESOLVED_NAMED,
        ROUND_PROGRESS,
        ROUND_KIND,
        ROUND_ACTIVE,
    ];

    // hir-ty's impl tables: the numbers of `crates/hir-ty/src/db.rs`.
    /// `InherentImpls::for_crate`: every inherent impl of a crate.
    pub const INHERENT_IMPLS_FOR_CRATE: QueryKind = QueryKind(0x2468);
    /// `InherentImpls::in_crate_for_self_ty`: one `InherentSlice(krate, head)`.
    pub const INHERENT_IMPLS_IN_CRATE_FOR_SELF_TY: QueryKind = QueryKind(0x2469);
    /// `TraitImpls::for_crate`: every trait impl of a crate.
    pub const TRAIT_IMPLS_FOR_CRATE: QueryKind = QueryKind(0x246c);
    /// `TraitImpls::for_crate_and_deps`: every trait impl of a crate and its dependencies.
    pub const TRAIT_IMPLS_FOR_CRATE_AND_DEPS: QueryKind = QueryKind(0x246e);
    /// `TraitImpls::in_crate_for_trait_and_self_ty`: one bucket of a `TraitImplsSlice`.
    pub const TRAIT_IMPLS_IN_CRATE_FOR_TRAIT_AND_SELF_TY: QueryKind = QueryKind(0x2471);
    /// `TraitImpls::in_crate_for_trait`: one `TraitImplsSlice(krate, trait)`.
    pub const TRAIT_IMPLS_IN_CRATE_FOR_TRAIT: QueryKind = QueryKind(0x2473);

    /// The queries that read one slice (design section 7).
    pub const SLICED: [QueryKind; 3] = [
        TRAIT_IMPLS_IN_CRATE_FOR_TRAIT,
        TRAIT_IMPLS_IN_CRATE_FOR_TRAIT_AND_SELF_TY,
        INHERENT_IMPLS_IN_CRATE_FOR_SELF_TY,
    ];
    /// The tables of a whole crate, or of each of its modules, that a sliced query must not read.
    pub const WHOLE: [QueryKind; 4] =
        [TRAIT_IMPLS_FOR_CRATE, TRAIT_IMPLS_FOR_CRATE_AND_DEPS, INHERENT_IMPLS_FOR_CRATE, MODULE_IMPLS];
}

/// How many keys of a kind may rerun after an edit.
#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum Most {
    Zero,
    /// One for each trait whose impl set the edit changed.
    Traits,
    /// One for each (trait, self type) bucket the edit changed.
    Buckets,
    /// No count: each rerun is held to having read something that changed.
    Any,
}

/// What an edit changed, by the harness's model of what it wrote: the traits whose impls (by
/// module, trait and self type, derives included) differ, and the (trait, self type) buckets.
#[derive(Deserialize, Serialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(deny_unknown_fields)]
pub struct Touched {
    pub traits: u32,
    pub buckets: u32,
}

impl Touched {
    fn most(self, most: Most) -> Option<u32> {
        match most {
            Most::Zero => Some(0),
            Most::Traits => Some(self.traits),
            Most::Buckets => Some(self.buckets),
            Most::Any => None,
        }
    }
}

/// What a case says of one kind of key: at most how many of it rerun, and whether a trace
/// with none of it measures nothing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Bound {
    pub kind: QueryKind,
    pub what: &'static str,
    pub most: Most,
    pub measured: bool,
}

/// One kind of edit of `harness/v2/dbsp/edits.py` (its `KINDS`) and what the build after it
/// may rerun, beside [`ROUND_BOUNDS`], which every case holds.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Case {
    pub edit: &'static str,
    pub claim: &'static str,
    pub bounds: &'static [Bound],
}

const fn bound(kind: QueryKind, what: &'static str, most: Most, measured: bool) -> Bound {
    Bound { kind, what, most, measured }
}

const SLICES: &str = "trait impl slices (crate, trait)";
const BUCKETS: &str = "trait impl buckets (crate, trait, self type)";
const INHERENT: &str = "inherent impl slices (crate, self type)";
const DECLS: &str = "modules' declarations";
const ROUND: &str = "round queries";

/// An edit of impls: the trait slice kind must be in the traces.
const IMPL_BOUNDS: &[Bound] = &[
    bound(kinds::TRAIT_IMPLS_IN_CRATE_FOR_TRAIT, SLICES, Most::Traits, true),
    bound(kinds::TRAIT_IMPLS_IN_CRATE_FOR_TRAIT_AND_SELF_TY, BUCKETS, Most::Buckets, false),
    bound(kinds::INHERENT_IMPLS_IN_CRATE_FOR_SELF_TY, INHERENT, Most::Zero, false),
];

/// An edit of names: the resolver's declarations must be in the traces.
const ITEM_BOUNDS: &[Bound] = &[
    bound(kinds::MODULE_DECLS, DECLS, Most::Any, true),
    bound(kinds::TRAIT_IMPLS_IN_CRATE_FOR_TRAIT, SLICES, Most::Traits, false),
    bound(kinds::TRAIT_IMPLS_IN_CRATE_FOR_TRAIT_AND_SELF_TY, BUCKETS, Most::Buckets, false),
    bound(kinds::INHERENT_IMPLS_IN_CRATE_FOR_SELF_TY, INHERENT, Most::Zero, false),
];

/// No round query reruns, whatever the edit.
pub static ROUND_BOUNDS: [Bound; kinds::ROUNDS.len()] = {
    let mut out = [bound(kinds::ROUNDS[0], ROUND, Most::Zero, false); kinds::ROUNDS.len()];
    let mut i = 1;
    while i < out.len() {
        out[i] = bound(kinds::ROUNDS[i], ROUND, Most::Zero, false);
        i += 1;
    }
    out
};

pub static CASES: &[Case] = &[
    Case {
        edit: "impl",
        claim: "an impl added or removed, of a trait or through a `use` of one, for a struct or through \
                an alias of one: only that trait's slice and that bucket rerun",
        bounds: IMPL_BOUNDS,
    },
    Case {
        edit: "derive",
        claim: "a derive added or removed: only the slices of the traits it implements rerun \
                (`PartialEq` two, with `StructuralPartialEq`)",
        bounds: IMPL_BOUNDS,
    },
    Case {
        edit: "alias",
        claim: "an alias retargeted: only the slices of the traits implemented through it rerun, and \
                the buckets of its old and new type",
        bounds: IMPL_BOUNDS,
    },
    Case {
        edit: "repoint",
        claim: "a `use` of a trait repointed: only the slices of the old and the new trait of the \
                impls through it rerun",
        bounds: IMPL_BOUNDS,
    },
    Case {
        edit: "method",
        claim: "a trait method added or removed: no impl slice reruns, and of the resolver only what \
                read the trait's file",
        bounds: ITEM_BOUNDS,
    },
    Case {
        edit: "mod",
        claim: "a module added or removed: the slices of the traits its impls implement rerun, and of \
                the resolver only what read the edited file",
        bounds: ITEM_BOUNDS,
    },
    Case {
        edit: "item",
        claim: "an item, `use`, glob, probe or `macro_rules!` added, removed or renamed: no resolver \
                query of another module reruns, and an impl slice only where an impl's trait or self \
                type now resolves otherwise",
        bounds: ITEM_BOUNDS,
    },
];

/// The case of an edit kind, if it has one.
pub fn case(edit: &str) -> Option<&'static Case> {
    CASES.iter().find(|case| case.edit == edit)
}

/// Where the traces are not what a [`Case`] says.
#[derive(Serialize, Clone, PartialEq, Eq, Debug)]
pub enum Finding {
    /// More keys of the kind reran than the edit touched; `keys` are those that did.
    Reran { what: &'static str, kind: QueryKind, reran: u32, most: u32, keys: Vec<QueryKey> },
    /// Neither trace has a result of a kind the case measures by.
    Unrecorded { what: &'static str, kind: QueryKind },
    /// A sliced query read `read`, a whole crate's table.
    Coarse { query: QueryKey, read: QueryKey },
    /// Ran after the edit though everything it read in the build before is as it was.
    Unexplained { query: QueryKey },
    /// Ran, read and made what it did before.
    Needless { query: QueryKey },
}

impl Finding {
    pub fn side(&self) -> Side {
        match self {
            Finding::Reran { .. } | Finding::Coarse { .. } | Finding::Unexplained { .. } | Finding::Needless { .. } => {
                Side::Over
            }
            Finding::Unrecorded { .. } => Side::Unmeasured,
        }
    }
}

/// The keys of `kind` that reran (ran in `after`, with a result in `before`), in key order, and
/// whether either trace has a result of the kind: one merge of the two runs of the kind.
fn reran<A: Columns, B: Columns>(before: &A, after: &B, kind: QueryKind) -> (Vec<QueryKey>, bool) {
    let (xs, ys) = (of_kind(before, kind), of_kind(after, kind));
    let any = (xs.start..xs.end).any(|i| before.has(i)) || (ys.start..ys.end).any(|i| after.has(i));
    let mut cursor = xs.start;
    // alloc: the answer, the keys that reran; unallocated while none did.
    let mut keys = Vec::new();
    for i in ys {
        if !after.ran(i) || !after.has(i) {
            continue;
        }
        let query = after.key(i);
        if let Some(j) = join::index_from(before, &mut cursor, &query)
            && before.has(j)
        {
            keys.push(query);
        }
    }
    (keys, any)
}

/// The index of `key` in `c`: a gallop and a binary search from the start.
fn index<C: Columns>(c: &C, key: &QueryKey) -> Option<usize> {
    let at = join::seek(c, 0, key);
    (at < c.keys() && c.key(at) == *key).then_some(at)
}

/// Where `before` and `after` are not what `case` says of an edit that touched `touched`, in this
/// order: the counts of its bounds and of [`ROUND_BOUNDS`]; sliced queries that read a whole
/// table; reruns of watched kinds though nothing they read had changed, and those that also made
/// what they did before. Each counted kind's keys are read once, `after`'s sliced keys once, and
/// `after`'s reruns are joined once against `before`, found a bitset word at a time. Owned traces
/// and [`crate::wire::View`]s of mapped files are read alike, so a check copies no column.
pub fn check<A: Columns, B: Columns>(case: &Case, touched: Touched, before: &A, after: &B) -> Vec<Finding> {
    let watched = |kind: QueryKind| {
        kinds::RESOLVER.contains(&kind.0) || kinds::SLICED.contains(&kind) || case.bounds.iter().any(|b| b.kind == kind)
    };
    // alloc: the answer, once a check of an edit; unallocated while nothing is found.
    let mut found = Vec::new();
    for b in case.bounds.iter().chain(&ROUND_BOUNDS) {
        let (keys, any) = reran(before, after, b.kind);
        if b.measured && !any {
            found.push(Finding::Unrecorded { what: b.what, kind: b.kind });
            continue;
        }
        let reran = u32::try_from(keys.len()).expect("a trace has fewer than 2^32 keys");
        if let Some(most) = touched.most(b.most)
            && reran > most
        {
            found.push(Finding::Reran { what: b.what, kind: b.kind, reran, most, keys });
        }
    }
    for kind in kinds::SLICED {
        for i in of_kind(after, kind) {
            if !after.has(i) {
                continue;
            }
            let read = after
                .edges_of(i)
                .map(|e| after.read(e) as usize)
                .find(|&read| kinds::WHOLE.contains(&after.key(read).kind));
            if let Some(read) = read {
                found.push(Finding::Coarse { query: after.key(i), read: after.key(read) });
            }
        }
    }
    // alloc: as above, the needless reruns, put after the rest.
    let mut needless = Vec::new();
    let mut cursor = 0;
    for i in join::ran_results(after) {
        let query = after.key(i);
        if !watched(query.kind) {
            continue;
        }
        let now = after.fingerprint(i);
        let Some(j) = join::index_from(before, &mut cursor, &query) else { continue };
        let Some(was) = result(before, j) else { continue };
        let changed = before.edges_of(j).any(|e| {
            index(after, &before.key(before.read(e) as usize)).and_then(|r| result(after, r)) != Some(before.saw(e))
        });
        if !changed {
            found.push(Finding::Unexplained { query });
        }
        if was == now && join::same_edges(before, j, after, i) {
            needless.push(Finding::Needless { query });
        }
    }
    found.append(&mut needless);
    found
}

#[cfg(test)]
mod tests {
    use blaze_ids::{ContentId, Fingerprint};
    use blaze_query::{Edge, QueryKey, QueryKind, kinds::FILE};

    use super::{kinds::*, *};
    use crate::{Rng, Trace, TraceBuilder, wire::View};

    fn key(kind: QueryKind, name: &str) -> QueryKey {
        QueryKey { kind, input: ContentId::of("blaze v2 proof dbsp test 1", name.as_bytes()) }
    }

    /// One result of a build: its key, its fingerprint, what it read (with the fingerprint seen)
    /// and whether the build ran it.
    struct Row {
        key: QueryKey,
        fp: u128,
        reads: Vec<(QueryKey, u128)>,
        ran: bool,
    }

    fn row(key: QueryKey, fp: u128, reads: &[(QueryKey, u128)], ran: bool) -> Row {
        Row { key, fp, reads: reads.to_vec(), ran }
    }

    fn trace(rows: &[Row]) -> Trace {
        let mut trace = TraceBuilder::default();
        for r in rows {
            let edges: Vec<Edge> = r.reads.iter().map(|&(read, saw)| Edge { read, saw: Fingerprint(saw) }).collect();
            trace.insert(r.key, Fingerprint(r.fp), &edges, r.ran);
        }
        trace.finish()
    }

    fn check_of(edit: &str, touched: Touched, before: &[Row], after: &[Row]) -> Vec<Finding> {
        check(case(edit).unwrap(), touched, &trace(before), &trace(after))
    }

    const ONE: Touched = Touched { traits: 1, buckets: 1 };

    /// `impl T1 for S` added to module `m` of file `f`: before the edit and after it, as a
    /// maintained build makes them. `other` is a module of a file `g` the edit did not write,
    /// with the slice of `T2`. `extra` rows are added to the build after the edit.
    fn impl_traces(extra: Vec<Row>) -> (Vec<Row>, Vec<Row>) {
        let (f, g) = (key(FILE, "src/lib.rs"), key(FILE, "src/other.rs"));
        let (decls_m, decls_other) = (key(MODULE_DECLS, "m"), key(MODULE_DECLS, "other"));
        let (t1, t2) = (key(TRAIT_IMPLS_IN_CRATE_FOR_TRAIT, "T1"), key(TRAIT_IMPLS_IN_CRATE_FOR_TRAIT, "T2"));
        let bucket = key(TRAIT_IMPLS_IN_CRATE_FOR_TRAIT_AND_SELF_TY, "T1 S");
        let before = vec![
            row(f, 1, &[], true),
            row(g, 2, &[], true),
            row(decls_m, 10, &[(f, 1)], true),
            row(decls_other, 12, &[(g, 2)], true),
            row(t1, 20, &[(decls_m, 10)], true),
            row(t2, 22, &[(decls_other, 12)], true),
            row(bucket, 30, &[(t1, 20)], true),
        ];
        let mut after = vec![
            row(f, 3, &[], true),
            row(g, 2, &[], false),
            row(decls_m, 11, &[(f, 3)], true),
            row(decls_other, 12, &[(g, 2)], false),
            row(t1, 21, &[(decls_m, 11)], true),
            row(t2, 22, &[(decls_other, 12)], false),
            row(bucket, 31, &[(t1, 21)], true),
        ];
        after.extend(extra);
        (before, after)
    }

    #[test]
    fn an_impl_reruns_only_its_trait_s_slice_and_bucket() {
        let (before, after) = impl_traces(Vec::new());
        for edit in ["impl", "derive", "alias", "repoint"] {
            assert_eq!(check_of(edit, ONE, &before, &after), vec![], "{edit}");
        }
    }

    #[test]
    fn another_trait_s_slice_rerunning_is_over() {
        let (mut before, mut after) = impl_traces(Vec::new());
        let (t2, decls_m) = (key(TRAIT_IMPLS_IN_CRATE_FOR_TRAIT, "T2"), key(MODULE_DECLS, "m"));
        // T2's slice reads `m`'s declarations, which changed: its rerun is explained, and over.
        let at = before.iter().position(|r| r.key == t2).unwrap();
        before[at] = row(t2, 22, &[(decls_m, 10)], true);
        after[at] = row(t2, 22, &[(decls_m, 11)], true);
        let found = check_of("impl", ONE, &before, &after);
        let mut keys = vec![key(TRAIT_IMPLS_IN_CRATE_FOR_TRAIT, "T1"), t2];
        keys.sort();
        assert_eq!(
            found,
            vec![Finding::Reran { what: SLICES, kind: TRAIT_IMPLS_IN_CRATE_FOR_TRAIT, reran: 2, most: 1, keys }]
        );
        assert_eq!(found[0].side(), Side::Over);
        assert_eq!(
            check_of("impl", Touched { traits: 2, buckets: 1 }, &before, &after),
            vec![],
            "an edit that touched two"
        );
    }

    #[test]
    fn a_slice_that_reads_the_crate_s_table_is_coarse() {
        let (before, mut after) = impl_traces(Vec::new());
        let (t1, table, decls_m) =
            (key(TRAIT_IMPLS_IN_CRATE_FOR_TRAIT, "T1"), key(TRAIT_IMPLS_FOR_CRATE, "crate"), key(MODULE_DECLS, "m"));
        let at = after.iter().position(|r| r.key == t1).unwrap();
        after[at] = row(t1, 21, &[(table, 50)], true);
        after.push(row(table, 50, &[(decls_m, 11)], true));
        let found = check_of("impl", ONE, &before, &after);
        assert_eq!(found, vec![Finding::Coarse { query: t1, read: table }]);
        assert_eq!(found[0].side(), Side::Over);
    }

    #[test]
    fn a_round_query_that_reruns_is_over_whatever_the_edit() {
        let decls_m = key(MODULE_DECLS, "m");
        let scc = key(SCC_STATE, "m@0");
        let (mut before, after) = impl_traces(vec![row(scc, 41, &[(decls_m, 11)], true)]);
        before.push(row(scc, 40, &[(decls_m, 10)], true));
        let want = Finding::Reran { what: ROUND, kind: SCC_STATE, reran: 1, most: 0, keys: vec![scc] };
        for case in CASES {
            let found = check(case, ONE, &trace(&before), &trace(&after));
            assert!(found.contains(&want), "{}: {found:?}", case.edit);
        }
    }

    #[test]
    fn a_resolver_query_of_a_module_whose_file_did_not_change_is_over() {
        let (before, mut after) = impl_traces(Vec::new());
        let decls_other = key(MODULE_DECLS, "other");
        let at = after.iter().position(|r| r.key == decls_other).unwrap();
        after[at].ran = true;
        let found = check_of("item", ONE, &before, &after);
        assert_eq!(found, vec![Finding::Unexplained { query: decls_other }, Finding::Needless { query: decls_other }]);
        assert!(found.iter().all(|f| f.side() == Side::Over));
    }

    #[test]
    fn an_item_edit_that_touched_no_impl_reruns_no_slice() {
        let (before, after) = impl_traces(Vec::new());
        let found = check_of("item", Touched::default(), &before, &after);
        assert_eq!(
            found,
            vec![
                Finding::Reran {
                    what: SLICES,
                    kind: TRAIT_IMPLS_IN_CRATE_FOR_TRAIT,
                    reran: 1,
                    most: 0,
                    keys: vec![key(TRAIT_IMPLS_IN_CRATE_FOR_TRAIT, "T1")]
                },
                Finding::Reran {
                    what: BUCKETS,
                    kind: TRAIT_IMPLS_IN_CRATE_FOR_TRAIT_AND_SELF_TY,
                    reran: 1,
                    most: 0,
                    keys: vec![key(TRAIT_IMPLS_IN_CRATE_FOR_TRAIT_AND_SELF_TY, "T1 S")]
                },
            ]
        );
    }

    #[test]
    fn a_key_first_asked_after_the_edit_is_not_a_rerun() {
        let decls_m = key(MODULE_DECLS, "m");
        let t3 = key(TRAIT_IMPLS_IN_CRATE_FOR_TRAIT, "T3");
        let (before, after) = impl_traces(vec![row(t3, 60, &[(decls_m, 11)], true)]);
        assert_eq!(check_of("impl", ONE, &before, &after), vec![]);
    }

    #[test]
    fn a_case_s_kind_neither_trace_has_is_unmeasured() {
        let f = key(FILE, "src/lib.rs");
        let decls = key(MODULE_DECLS, "m");
        let before = [row(f, 1, &[], true), row(decls, 10, &[(f, 1)], true)];
        let after = [row(f, 2, &[], true), row(decls, 11, &[(f, 2)], true)];
        let found = check_of("impl", ONE, &before, &after);
        assert_eq!(found, vec![Finding::Unrecorded { what: SLICES, kind: TRAIT_IMPLS_IN_CRATE_FOR_TRAIT }]);
        assert_eq!(found[0].side(), Side::Unmeasured);
        assert_eq!(check_of("item", ONE, &before, &after), vec![], "an item edit measures by the declarations");
    }

    #[test]
    fn every_edit_kind_of_the_harness_has_one_case() {
        let edits =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../harness/v2/dbsp/edits.py")).unwrap();
        let line = edits.lines().find(|l| l.starts_with("KINDS = (")).expect("edits.py has `KINDS = (...)`");
        let named: std::collections::BTreeSet<&str> = line.split('"').skip(1).step_by(2).collect();
        let cased: std::collections::BTreeSet<&str> = CASES.iter().map(|c| c.edit).collect();
        assert_eq!(named, cased);
        assert_eq!(cased.len(), CASES.len());
        assert!(CASES.iter().all(|c| !c.claim.is_empty() && c.bounds.iter().any(|b| b.measured)));
    }

    /// A build before and after an edit over the kinds the cases read, by `seed`: `keys` keys of
    /// [`ROUNDS`], [`SLICED`], [`WHOLE`] and [`MODULE_DECLS`], each read by up to four others, some
    /// results changed, dropped or rerun after the edit, so every finding comes up.
    fn seeded_pair(seed: u64, keys: u64) -> (Trace, Trace) {
        let mut rng = Rng::new(seed);
        let all: Vec<QueryKind> = ROUNDS.iter().chain(&SLICED).chain(&WHOLE).copied().chain([MODULE_DECLS]).collect();
        let key = |n: u64| QueryKey {
            kind: all[(n % all.len() as u64) as usize],
            input: ContentId::of("blaze v2 proof dbsp seeded 1", &n.to_le_bytes()),
        };
        let (mut before, mut after) = (TraceBuilder::default(), TraceBuilder::default());
        for n in 0..keys {
            let reads: Vec<(u64, u128)> =
                (0..rng.below(5)).map(|_| (rng.below(keys), u128::from(rng.below(3)))).collect();
            let fp = u128::from(rng.below(3));
            let edges = |shift: u128| -> Vec<Edge> {
                reads.iter().map(|&(r, saw)| Edge { read: key(r), saw: Fingerprint(saw + shift) }).collect()
            };
            if rng.below(6) != 0 {
                before.insert(key(n), Fingerprint(fp), &edges(0), true);
            }
            match rng.below(4) {
                0 => {}
                1 => after.insert(key(n), Fingerprint(fp), &edges(0), true),
                2 => after.insert(key(n), Fingerprint(fp + 1), &edges(1), true),
                _ => after.insert(key(n), Fingerprint(fp), &edges(0), false),
            }
        }
        (before.finish(), after.finish())
    }

    /// The check of views read in place answers what the check of owned traces does, for every
    /// case, and both find what a walk over every key finds of the reruns.
    #[test]
    fn a_check_in_place_answers_as_on_owned_traces() {
        for seed in 0..24 {
            let (before, after) = seeded_pair(seed, 30 + seed * 37);
            let bytes = [&before, &after].map(|t| blaze_protocol::cbor::to_vec(t).unwrap());
            let [b, a] = [0, 1].map(|k| View::parse(&bytes[k]).unwrap());
            for case in CASES {
                for touched in [Touched::default(), ONE, Touched { traits: 5, buckets: 9 }] {
                    let want = check(case, touched, &before, &after);
                    assert_eq!(check(case, touched, &b, &a), want, "seed {seed}, {}", case.edit);
                    assert_eq!(check(case, touched, &before, &a), want, "seed {seed}, {} mixed", case.edit);
                }
            }
            let walked: Vec<usize> = (0..after.keys.len()).filter(|&i| after.ran(i) && after.has(i)).collect();
            assert_eq!(join::ran_results(&after).collect::<Vec<_>>(), walked, "seed {seed}");
            assert_eq!(join::ran_results(&a).collect::<Vec<_>>(), walked, "seed {seed} in place");
        }
    }

    /// The oracle's steady state for dbsp: a build that reran nothing, checked in place, allocates
    /// nothing.
    #[test]
    fn a_check_of_a_build_that_reran_nothing_allocates_nothing() {
        let kinds =
            [MODULE_DECLS, TRAIT_IMPLS_IN_CRATE_FOR_TRAIT, TRAIT_IMPLS_IN_CRATE_FOR_TRAIT_AND_SELF_TY, SCC_STATE];
        let key = |n: u64| key(kinds[(n % 4) as usize], &n.to_string());
        let mut rows = Vec::new();
        for n in 0..400u64 {
            // Only declarations are read, so no slice reads a whole table.
            let reads: Vec<(QueryKey, u128)> = (0..n % 3).map(|r| (key((n / 4 + r) * 4), 1)).collect();
            rows.push(row(key(n), 1, &reads, false));
        }
        let settled = trace(&rows);
        let bytes = blaze_protocol::cbor::to_vec(&settled).unwrap();
        let view = View::parse(&bytes).unwrap();
        for case in CASES {
            let found =
                blaze_alloc_count::assert_at_most(0, "a dbsp check, nothing reran", || check(case, ONE, &view, &view));
            assert_eq!(found, vec![], "{}", case.edit);
        }
    }

    #[test]
    fn the_kinds_are_distinct_and_in_their_ranges() {
        let mut all: Vec<QueryKind> = ROUNDS.to_vec();
        all.extend(SLICED);
        all.extend(WHOLE);
        all.push(MODULE_DECLS);
        let mut sorted = all.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), all.len());
        assert!(ROUNDS.iter().chain([&MODULE_DECLS, &MODULE_IMPLS]).all(|k| RESOLVER.contains(&k.0)));
        for k in SLICED.iter().chain(&WHOLE[..3]) {
            assert!(blaze_query::kinds::FRONT.contains(&k.0) && !RESOLVER.contains(&k.0), "{k:?}");
        }
        assert!(ROUND_BOUNDS.iter().zip(ROUNDS).all(|(b, k)| b.kind == k && b.most == Most::Zero));
    }
}
