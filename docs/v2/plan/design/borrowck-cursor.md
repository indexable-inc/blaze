---
status: plan
summary: BLAZE_BORROWCK=polonius-flow, a borrow-check mode that accepts what datalog Polonius accepts and the alpha rejects (linked-list cursors, flow-sensitive invariance), and never less than the alpha.
date: 2026-11-02
---
# Lane borrowck-cursor: `polonius-flow`

## What it is for

The default checker is rustc's Polonius alpha ([borrowck.md](../../../borrowck.md)). Section 2.5
of the research lists five tests that the alpha rejects and
legacy (datalog) Polonius accepts:

- `nll/polonius/iterating-updating-cursor-issue-57165.rs`
- `nll/polonius/iterating-updating-cursor-issue-63908.rs`
- `nll/polonius/iterating-updating-cursor-issue-108704.rs`
- `nll/polonius/flow-sensitive-invariance.rs`
- `nll/issue-46589.rs`

The first three are a linked-list cursor with a conditional reborrow. The fourth is an invariant
value returned on two branches, as two different lifetimes. The fifth is
`*x = match (*x).get() { Some(s) => s, None => (*x).new() }`.

`BLAZE_BORROWCK=polonius-flow` accepts all five. It is opt-in, so programs that don't set it are
unchanged. When it is set, it only ever accepts more than `polonius`, never less.

## Design

**Two passes.** The alpha runs first, in a frame of its own. If it accepts the body, or skips it,
or doesn't check it, its result is returned as it is. A body the alpha accepts therefore costs
nothing extra and gets the same result, byte for byte. If the alpha reports an error, the body is
checked again in `Mode::PoloniusFlow` under a `borrowck.flow` span, and that result is returned.
rustc's Polonius team floated the same shape, "running the more expensive analysis only if the
cheaper NLLs find possible issues" (lqd, rust-lang/goals#118).

**The second pass.** `crates/hir-ty-borrowck/src/flow.rs` computes loan liveness and the subset
errors among universal regions as polonius-engine's naive rules do
(`fed1eb428d:docs/upstream/borrowck/polonius/book_src_rules_relations.md`). The rules run over blaze's
points, liveness and constraints:

- A subset holds at the point of its constraint, or everywhere.
- Subsets are transitive within a point.
- A subset flows along a CFG edge while both of its regions are live.
- A loan flows from a region along a CFG edge while the region is live, unless the point kills
  the loan (`Loans::killed_at`, the relation the `Borrows` dataflow kills by).
- A loan is live where a live region holds it.
- A universal pair is an error where it holds and the signature doesn't give it.

A subset made only from constraints that hold everywhere holds at every point, so it isn't
carried. `Flow::carried` keeps only pairs whose path uses a located constraint, which keeps the
carried sets small.

**Combining.** Each loan's live points are the alpha's intersected with the datalog's. The
universal region errors are dropped only when the datalog finds no failing pair, and only for
bodies whose universal check the datalog fully covers: not a closure or inline `const` (their
errors go to the creator), no placeholders, no member constraints (`flow::universals_checkable`).

**Bound.** The datalog takes at most `flow::BUDGET` = 2^24 deterministic steps. When it runs out,
nothing is narrowed or dropped: the body keeps the alpha's errors, and each error gets a note
naming the phase and the step count. That is the default checker's answer stated plainly, not a
quiet change of path.

## Why it is sound

Let T be the set of points where a loan's reference can really still be used. The alpha's
liveness contains T, and so does the datalog's, so their intersection contains T. T is connected
from the issue point. A loan's scope runs from where it is issued to its first point outside the
set (`compute_polonius_scopes`), so the narrowed scope still covers T. Narrowing a scope can only
remove conflicts that happen outside T.

For universal regions, a body only drops errors where the datalog's location-sensitive check
holds. That check is what legacy Polonius uses as its verdict.

The argument inherits the alpha's own liveness soundness. rustc's one known hole there (Jack
Huey, "Fixing an unsoundness with the Polonius borrow checker", 2026-06-26;
`borrowck/alias-liveness/drop-liveness-invariance-issue-160670.rs`) is fixed or not in exactly
the same way in both modes.

## Proof on the tests

- `harness/v2/frontend/borrowck_flow.py` runs the 46 tests with a Polonius revision. They are
  vendored verbatim in `fed1eb428d:docs/upstream/borrowck/rust-lang-rust/tests-ui` (32 of them added by this
  lane; SOURCE.md). The script runs each in both modes and fails on any of these:
  - `polonius-flow` rejects something `polonius` accepts.
  - `polonius-flow` accepts something every upstream revision rejects.
  - A target is not accepted.
- `differential::analyses::polonius_flow_accepts_more_never_less`: on every differential fixture
  function, `polonius-flow`'s result equals the alpha's where the alpha accepts. Where the alpha
  rejects, `polonius-flow`'s errors are a subset of the alpha's.
- `flow_tests`: std-free versions of the five targets must be accepted. The tests every Polonius
  rejects must stay rejected: polonius-smoke-test, subset-relations,
  lending-iterator-sanity-checks, assignment-to-differing-field and get_default.

The vendored suite is all of rustc's borrowck UI tests that live in `fed1eb428d:docs/upstream`. The wider
suite of about 1590 units, which the old harness scored, was deleted on 2026-10-30
([borrowck.md](../../../borrowck.md#how-it-is-tested)).

## Cost

The added cost is zero for bodies the alpha accepts. A body the alpha rejects pays for one more
check plus `Phase::Flow`. `flow_tests::flow_cost_per_body` (ignored, run with `--ignored
--nocapture`) prints the per-body times. In a build, the `borrowck.flow` spans in
`<BLAZE_HOME>/v2/profile/*.spans.parquet` give them. No numbers yet: this lane only type-checks.

## Prior art

- **polonius-engine** (rust-lang/polonius, 1758 stars, pushed 2026-10): the naive rules ported
  here. The rules, not the engine: blaze has its own points and liveness, and the datalog runs
  over them with a budget.
- **rustc's earlier alpha.** Before rust-lang/rust#143093, the localized graph stopped its
  traversal at kills ("kills stopping traversal in the new localized constraint graph", lqd,
  rust-lang/goals#118). The current alpha handles kills in the scope dataflow only.
  `polonius-flow` puts kills back into the traversal, but only to narrow the alpha.
- **"Full Polonius"** in the 2026 roadmap ("The Borrow Checker Within", rust-lang/goals): "extend
  the alpha analysis with full flow-sensitivity, handling patterns like linked-list traversal
  with conditional reborrowing". That is exactly this lane's target, and upstream has not
  scheduled it.
- **polonius-the-crab** (danielhenrymantilla, 131 stars): a stable-Rust crate that works around
  NLL problem case 3 with a higher-order API. A blaze program in `polonius-flow` mode needs no
  such workaround for the cursor cases.
- The Niko Matsakis "Polonius revisited" posts (2023;
  `fed1eb428d:docs/upstream/borrowck/blog/babysteps-2023-polonius-revisited-excerpts.md`) set out the
  location-sensitive design the alpha implements.

A GitHub search (`polonius borrow checker pushed:>2025-06-01`) found no other implementation
that combines the alpha with the datalog.

## Not done

- No runs. The lane rules allow only type-checking, so none of the tests, the harness or the
  benchmark has run.
- The datalog doesn't model placeholders or member constraints. Bodies that have them keep the
  alpha's universal check, though their loans are still narrowed.
