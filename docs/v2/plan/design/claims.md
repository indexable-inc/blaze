---
status: plan
summary: A promise in a doc comment is derived by the compiler, checked by an attribute, or points at what proves it; never only asserted. The one written form (`# Guarantees` / `# Invariants` items ending in `Evidence:` or `Unverified:`), the six lints that would enforce it (declared in blaze-lint-defs, not built), and the ledger in scripts/src/bin/invariants.rs (built) that lists every claim with its evidence and level and that the gate holds.
date: 2026-11-13
---
# Claims

Asked for by Andrew, 2026-11-13: a claim a doc makes should point directly at what proves it,
"rather than just being vibes", so an LLM reading the doc reads what the tree holds. AGENTS.md
rule 53 (`invariants-are-stated`) said where a promise is stated; this says what holds each one.

## The end state

Every promise of a doc comment is one of four, and a reader can tell which from the doc:

1. **Derived**: the compiler decides it, so the doc says nothing. An effect (`nopanic`,
   `noalloc`, `nonblocking`, `pure`) the effect walk proves is an attribute, not a sentence.
2. **Checked**: an attribute or contract the build checks (`#[blaze::nopanic]`, `requires`,
   `#[blaze::invariant]`). `trusted = "why"` is a declared hole, counted as unverified.
3. **Evidenced**: an item of `# Guarantees` (a function's) or `# Invariants` (a type's) that
   ends in `Evidence:` naming a test, oracle or simulation by an intra-doc link, or a Lean
   theorem by name: ``Evidence: [`tests::sorted_by_key`]``, ``Evidence: Lean `Blaze.Mir.step` ``.
4. **Declared unverified**: an item that ends in `Unverified: <why>`.

A sentence that is none of these, a pointer that does not resolve or does not reach the item,
and a doc the facts refute, are findings.

## Why this syntax

- A rustdoc section with intra-doc links is plain Rust: rustc ignores it, so it holds while
  cargo builds blaze (rule 6 forbids a `cfg` guard, and a new attribute would need one); rustdoc
  renders it and checks the links; an agent reads the promise and its proof where the item is.
- It extends the `/// The invariant:` statements the ledger already reads, with one marker
  each: one spelling (`Tested by`, `Proof:`, lowercase `evidence:` are findings, with a fix).
- The level is not written: the ledger derives it from what the pointer is (below), so it
  cannot drift from the evidence.
- `# Safety` stays the caller's obligation (a precondition; its checked home is `requires`).

## The lints (declared in crates/blaze-lint-defs/src/lints.rs, not built)

The six names are registered at `allow`; no checker implements them yet.

| lint | blaze's level | fix |
| --- | --- | --- |
| `claim_contradicts_facts` | deny | none: the doc or the code is wrong |
| `claim_is_derivable` | warn | write the attribute, delete the sentence (`MachineApplicable`) |
| `claim_without_evidence` | deny, baseline | the one marker for a misspelling (`MaybeIncorrect`) |
| `evidence_is_stale` | deny, baseline | none |
| `safety_without_invariant` | deny, baseline | none |
| `comment_claims` | allow (advice) | none: names the checked form, never rewrites |

All six are in `blaze::internal`: another project opts in at the adoption ladder's `suggest`
step. Nothing here depends on the ladder's code.

**Promise patterns.** A prose effect is read only in the narrow form: a clause opening (after
`It`/`This`/`This function`, or after `: ` or `; `) with a negation (`never`, `does not`,
`cannot`, `can't`, `will not`, `won't`, `can never`) of `panic`, `allocate` or `block` (joined
by `or`, `,`, `nor`), or `allocates nothing`, `is pure`, `has no side effects`, `is
panic-free`; or a later such phrase of a clause opening with the item's verb; ended by the
clause's end, never by a narrowing word (`once`, `when`, `until`, `per`, `without`, ...). A
broad keyword list (`never`/`cannot` + effect, `is thread-safe`, `guarantees`, `safe because`)
was measured on the tree's docs (2026-11-13): about 21 of 40 sampled lines were whole-item
promises, 53%. The narrow form matched 14 lines, each read as a whole-item promise; its recall is
low by design, and the rest of a doc's promises go into `# Guarantees`.

**Decided only exactly.** A contradiction is a refusal for what the code does (`Panic`,
`Overflow`, `Alloc`, `AllocIn`, `Block`, `Io`, `Env`, `Clock`, `Thread`, `MutableState`,
`Address`, `Random`); what the walk can't follow (`Unchecked`, `Open`, `Foreign*`, `Indirect`,
`Loop`, `Recursion`, `Unsafe`) is undecided, so the sentence needs evidence. `nopanic` is
derived only in a crate built with overflow checks. Not built: `# Errors` against the error
set (docs/v2/plan/design/error-sets.md is a plan; nothing infers a set), complexity (nothing
decides it exactly); both are evidenced items.

**Reach.** A pointer must name a declaration from the item's module (module segments, then
the item; visibility not asked, every configuration of the crate searched, so a private test
under `#[cfg(test)]` is found from the lib; the item matched by where it is written), be a test (`#[test]`, or `cfg(test)` code: an oracle, a
simulation), and reach the item: a call of the function, or a value of the type, within two
calls into the crate's own code. Which part of a result the test observes is the
test-observation lane's analysis; when it lands, a claim about a result asks it.

`doc_names_nothing` (a backticked path that names nothing) skips the text after `Evidence:`,
which these lints check.

## The ledger (built)

scripts/src/bin/invariants.rs (`check`, `write`, `claims`) reads every claim with its evidence and
level into the Claims section of docs/v2/plan/invariants.md, and its `check` holds the gate: every
pointer found, no crate's unverified count up, no claim's level down.

## Floor

Each lint is one pass over a doc's lines and, for a function, one effect query per stated
effect that the effect lints already make (`check_generic` is memoized), so its cost is
within the noise of the body pass. The ledger reads each source file once.
