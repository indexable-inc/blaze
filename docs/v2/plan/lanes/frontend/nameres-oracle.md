---
status: plan
summary: frontend-nameres-oracle — the invalidation oracle's cases for frontend.md items 1-3, fixture slots and blaze-v2-proof's `nameres` checks on build traces, rounds-independent.
date: 2026-10-29
---
# frontend-nameres-oracle: oracle cases for name resolution and the impl buckets

[frontend.md](../../design/frontend.md) items 1 to 3 claim that an edit reruns only what read a
fact it changed. The oracle (`harness/v2/oracle.py`, [proof.md](../proof.md)) already checks
every edit of the fixture against a build from scratch (`blaze-v2-proof`'s `under`) and counts
reruns for nothing (`over`). Those checks are generic: they catch stale results, but not a
resolver that reruns a whole crate correctly. This part adds, per fixture slot aimed at items 1
to 3, what the facts say the edit changes, and checks the traces against it.

## 1. What a real build's trace holds

`blaze trace` writes the blaze-query memos of the view's last build (`blaze-cargo`
`serve/job.rs` `trace`). A name answer or an impl bucket enters them as a fact edge:
`Cx::read(Fact::key(), fingerprint)`, keyed by content (`blaze_items::Fact::{Resolves,
Implements, Method}`, kinds `0x1003..0x1005`). The resolver's own queries (`module_decls`,
`scc_state`, `macro_step`, ...) are salsa queries today and appear in no trace. They get kind
numbers by request (section 5) and appear once hir-def runs on blaze-query (lane db). Until
then a case reports their kinds as unmeasured, never as passing.

Keys are content hashes, so the oracle cannot name the module a key belongs to. A case therefore
says *how many* keys of a kind may change, never which ones. It counts only kinds whose keys
name no expansion round: `Resolves`, `Implements`, `Method`, `module_decls`, `module_impls`. A
count of `scc_state` or `macro_step` keys would scale with the number of rounds a crate needs,
so those kinds are only *watched*. That makes every case independent of the round count, which
the tests check by building the same model with 1 to 4 rounds.

## 2. The checks (`crates/blaze-v2-proof/src/nameres.rs`, `check`)

Given the traces of the build before the edit, the incremental build after it and the build
from scratch:

| finding | side | rule |
|---|---|---|
| `Changed` | over if more, under if fewer | the number of keys of a counted kind, with a result in both builds, whose fingerprint changed, is outside the case's range (`differ`: one merge of the kind's two sorted runs) |
| `Unrecorded` | unmeasured | neither trace has a result of a counted kind |
| `Unexplained` | over | a watched result reran though every edge it had in the build before reads what it saw |
| `Needless` | over | a watched result reran and made the same value from the same reads (`over`'s needless) |
| `Kept` | under | a result was kept though a watched read changed: the reader of a flipped fact did not rerun (`under`'s `Unverified`) |
| `Stale` | under | a watched result differs from the build from scratch (`under`) |

A kind is watched when a case counts it or it is one of the resolver's (`kinds::RESOLVER`).
Together the checks say: the facts that changed are exactly as many as the edit answers
differently (`Changed`), what reran read one of them (`Unexplained`, `Needless`), and what read
one reran (`Kept`, `Stale`).

## 3. The cases

Each case holds for an edit of its slot between any two of its variants, so random sequences use
them as well as the `nameres` regression (`fixture.REGRESSIONS["nameres"]`, which edits every
case's slot there and back).

| slot | edit | counted | frontend.md |
|---|---|---|---|
| `nameres-item` (new) | `oracle_base::a` gains `fn gamma` or `pub use crate::b::beta as gamma`; the root imports `a::triple` | `Resolves` 0, `module_decls` 1 | 1: a `use` or item reruns only modules whose facts read the name |
| `nameres-glob` (new) | `helpers`, globbed by `chooser`, gains `spare`, read by nothing | `Resolves` 0, `module_decls` 1 | 1: only the glob's component reruns |
| `nameres-negative` (new) | `helpers` gains `fn drop(String) -> usize`; `chooser::released` calls `drop`, the prelude's until the glob brings one | `Resolves` 1, `module_decls` 1 | 1: a negative fact flips and its reader reruns |
| `shadow` | `chooser` gains a local `pick` over the glob's | `Resolves` 1, `module_decls` 1 | 1: glob shadowing |
| `nameres-bucket` (new) | `probe` gains `impl Weight for u32`; goals ask (Weight, u8) and (Weight, u16) only | `Implements` 0, `Method` 0, `module_impls` 1 | 2: only goals of the impl's (trait, self type) bucket |
| `specific` | `impl Specific for Marker` added or removed | `Implements` 1, `Method` 0 to 1, `module_impls` 1 | 2: the (Specific, Marker) bucket flips, the blanket `Fallback for &T` one does not |
| `nameres-inherent` (new) | `probe` gains inherent `Probe::name` over `Named::name` | `Method` 1, `Implements` 0, `module_impls` 1 | 3: only probes of that self type |

Every new variant builds under rustc and changes what the program prints (`released: ()` or
`4`, `named: trait` or `inherent`), except `nameres-item`, `nameres-glob` and `nameres-bucket`,
which only add what nothing reads. Those three are the over-invalidation probes.

The SCC-level claim (only the glob's component's `scc_state` reruns) needs module names, which
content keys don't carry. It is checked in hir-def by lane frontend-nameres-diff's
`facts/tests/trace.rs` over `TestDB::log_executed`. Here it shows as: no name answer changed,
and nothing reran without a changed read.

## 4. What proves it (the tests in `nameres.rs`)

A `Model` is a query graph with each result's fingerprint before and after the edit. From it
come the three traces a correct database makes: before and from scratch run everything; the
incremental build reruns an input that changed or a result one of whose reads did, and keeps
the rest. A test can force a result to rerun or to be kept.

- Correct builds give no finding for each case's model, with 1 to 4 rounds.
- Over: a `Resolves` fact reading a module's whole state gives `Changed` over, the same at every
  round count. So do an `Implements` bucket reading every impl of its trait and a probe cache
  reading every inherent impl of the module. Rerunning another component's `scc_state`, or a
  module whose source didn't change, gives `Unexplained` and `Needless`.
- Under: a reader kept though its negative fact flipped gives `Kept`. A negative read with no
  edge gives `Changed` under (0 changed, 1 expected).
- A kind neither trace has gives `Unrecorded`.
- A fact asked only after the edit is new, not changed.
- `differ` equals `apart`'s `Differ`s over the same kind on 200 random trace pairs.
- Every case is a slot of `fixture.py` and in its `nameres` regression. The kinds are distinct
  and in `blaze_query::kinds::{FACTS, FRONT}`.

## 5. Requests (`v2/requests/frontend-nameres.md`, `## oracle`)

1. Core, done: `crates/hir-def/src/nameres/facts/kinds.rs`, the resolver's kind table at
   `0x2200..0x2240`. `nameres::kinds` names the kinds the cases read and watches the whole
   range.
2. Coordinator, done: the proof binary takes `"slot"` and runs the slot's case. `oracle.py`
   passes the edit's slot, fails on over and under findings and records unmeasured ones as a
   gap.

## 6. Speed of light

`check` reads what it must and nothing twice:

- each counted kind's run of keys in both traces, found by two binary searches and merged once
  (`differ`), allocating only the changed keys;
- one join of the incremental build's reruns against the build before (`Trace::index_from`'s
  gallop, amortized one comparison a key), with a binary search per edge of a rerun's old memo;
- `under`'s one pass over the build from scratch.

For a trace of nushell (about 10⁷ keys, 5·10⁷ edges: 0.34 GB of keys and 1 GB of edges), the
floor is reading each trace's columns once: about 1.3 GB a trace at about 50 GB/s, so roughly
25 ms. `under` already costs that. The counted runs are a small part of the keys, and the join
touches only reruns, which an incremental build keeps few. So `check` adds well under 10% to
the comparison the oracle already makes. The state of the art it measures against is rustc's
red-green graph and salsa's backdating, which say *that* something reran. This says whether the
rerun was one the facts called for, as Datalog provenance does for derived tuples.

## 7. State

Written and tested in source; nothing built (common.md). Both requests are applied. The cases
that count resolver kinds stay unmeasured until hir-def's queries run on blaze-query.
