---
status: plan
summary: "How records use presence conditions (crates/blaze-cond) so one `blaze check` answers for every target, feature set and switch: an entry carries a `Cond` (a hash-consed decision diagram over atoms with stable keys), a task runs under a path condition and forks only on distinct outcomes of a lookup, a record is stored with the condition it holds under and that condition's support as its cfg read list. Replaces the fork-per-variable `Condition` of crates/blaze-drive/src/condition.rs and its caps (16 atoms, 64 conditions). Open: checking one body under several outcomes."
date: 2026-11-15
---
# Presence conditions in records

bottom.md 11, "One check covers every configuration". Today's design
(crates/blaze-drive/src/condition.rs) re-enters a task once per value of each cfg variable it
reads, and a record's condition is a partial assignment. A module's scope record reads every
switch that gates one of its items: measured with `crates/blaze-cond/examples/cost.rs`, the
read list of tokio 1.43's root is 24 variables, rustix 0.38's 47, so the fork design makes up
to 2^24 and 2^47 records, or refuses past 64. The fix is known from variability-aware analysis
of C (TypeChef, Kästner et al., OOPSLA 2011; SuperC, Gazzillo and Grimm, PLDI 2012): never fork
the record; each entry carries its presence condition. blaze-cond builds the condition.

## The values (crates/blaze-cond)

- A variable is a boolean with a name: `feature:<package>/<feature>`, `cfg:<name>`,
  `cfg:<name>="<value>"`. Stored conditions name it by its `VarKey` (64 bits of BLAKE3's key
  derivation of the name), never its number in a request (rule 25). The space orders variables
  by name, so every request orders shared variables alike.
- A target is not a variable: its atoms are (`unix`, `target_os = "linux"`, ...), and the
  request's `valid` is the disjunction over its targets of each one's full cube over the target
  atoms, conjoined with Cargo's feature implications. A record whose item is `#[cfg(unix)]`
  then holds `unix`, not "targets 0 and 2", and stays true when a target is added.
- `Cond(u32)`: an edge into a task's arena, complement in the low bit; `Cond(0)` is always,
  `Cond(1)` never. Equal functions are equal ids in one arena. A record holds the portable form:
  rows `(key, low, high)` in one canonical order, 16 bytes a row (28 to 49 bytes a condition on
  average on the measured roots); import re-interns and refuses malformed rows by row.

## (i) An entry carries a condition

A scope record's entry (a name, an impl, a macro) carries the `Cond` under which it exists:
the conjunction of its own `#[cfg]` and its module's. `0` is always and costs nothing. Two
entries of one name are both kept; that their conditions intersect within `valid` is a finding
("`f` is defined twice with feature `a` on, target x86_64-pc-windows-msvc": the text is
`any_sat` through `Space::describe`). The record stays linear in its entries.

## (ii) A task forks on distinct outcomes, never per variable

A task runs under a path condition `P`, starting at `valid`. A lookup answers a short list of
alternatives `[(c_i, r_i)]`, the `c_i` disjoint: the entries of the name, each with its
condition. The task drops each alternative with `disjoint(c_i, P)` (no node is made), groups the
rest by equal result (or-ing their conditions), and then:

- one outcome left: it continues under `P` (a lookup of an entry that exists everywhere forks
  nothing, whatever variables gate its neighbours);
- k outcomes: it continues k times, under `P & c_i` for each distinct outcome.

Continuations that reach equal results merge again: the record keeps one entry under the `or`
of their conditions. The bound (rule 18) is the number of distinct outcomes live in one task,
not variables: a task past `MOST_OUTCOMES` is a typed finding naming the item and the lookups
that forked, plus the arena's node cap (`Limits::nodes`, a typed `CondError::NodeCap`).

## (iii) Stored under the condition it holds under; its support is its cfg read list

Decided 2026-11-15 (coordinator, for the drive's port):

- **One record a key; the key is text only** (rule 65): `base`, no condition in it. A record is
  variational: it holds `cover`, the condition it was computed under, its entries carry their
  own conditions, and its read list carries the stable keys of `support` of what it read. A
  record that read no cfg has cover true, the zero id, at no cost.
- **A lookup under `P` is a hit** when the read list holds and `implies(P, cover)`; nothing is
  searched among several records. When `P` is not inside `cover`, the part `P & !cover` is a
  miss: the task runs under `cover | P` (first recompute; splicing the old entries for the
  covered part comes after it is measured) and writes the record that supersedes the old one
  under the same key, as a read-list miss replaces a record today. Coverage only grows, so two
  requests with different spaces converge. `blaze check` asks under all of `valid`; a
  single-configuration `blaze run` reads those records by restriction, and when first, writes a
  narrower cover the next check widens.
- **One arena a request, on the coordinator**: a `Cond` id means something only in it.
  Workers build in arenas of their own and hand back the portable form, which the coordinator
  re-interns (139 to 208 ns an import); there is no shared mutable arena.
- **A certificate that stood imports nothing**: conditions are imported only for the records a
  miss makes the machine read.

An atom the space does not hold lowers to false and is reported by `cfg::lower` as an absent
key: the record lists it in its read list as absent, and a request whose space holds it does
not take the record.

## (iv) What changes in crates/blaze-drive (its lane edits; this doc does not)

`MOST_ATOMS` (16 variables in one condition) goes: a `Cond` fixes no count of variables.
`MOST_CONDITIONS` (64 conditions an item) becomes `MOST_OUTCOMES`, counting distinct outcomes in
one task. `OVER_CAP_RULE` stays, for the outcome cap.

- condition.rs: `Condition` -> `blaze_cond::Cond` plus the task's `Bdd`; `Var`/`Value` ->
  `blaze_cond::Var` (in a request) and `VarKey` (stored); `Condition::with` -> `and` with a
  literal; `Condition::and` -> `Bdd::and` (false instead of `None`); `Condition::implies` ->
  `Bdd::implies`; `Condition::get`, `atoms`, `vars` -> `Bdd::support`, `restrict`;
  `Space { values, implications }` -> `blaze_cond::Space` (targets as atom cubes, implications
  in `valid`); `Space::allows` -> `!disjoint(cond, valid)`; `Space::closed` goes (`valid` is
  exact without a closure); `Space::span` goes (`sat_count` where a report wants a size);
  `Space::fork` goes (forks come from a lookup's alternatives); `ConditionError::TooManyAtoms`
  goes, `UnknownVar` -> `CondError::UnknownVar`, `OverCap` -> an outcome-cap error; the model
  test is rewritten with alternatives and keeps its oracle (the whole space equals each
  configuration) and its control.
- request.rs: `Ran::Cfg(Var)` -> a lookup answering alternatives; `forks`, `fork_of`,
  `resolve`, `resolve_settled` lose their `0..=MOST_ATOMS` walks; `Task::condition` and the
  `conditioned` table hold a `Cond`'s portable form.
- key.rs: `at_condition` hashes the condition's portable bytes, not `(var, value)` pairs;
  `space_digest` hashes the space's sorted variable keys, targets and implications.
- answers.rs and profile.rs: a row's condition is the portable form; `conditions_hist` counts
  outcomes.

## What is better than TypeChef and SuperC, and why

Variables are atoms with stable keys, so a stored condition means the same in every request and
two builders write equal bytes; they keep one in-memory formula per run. A task forks per
distinct outcome, where SuperC forks its parser per conditional branch. `implies` and
`disjoint` make no node. The printed form (an irredundant sum of products) may use the
configurations no request has as don't-care, so a finding says `unix`, not a list of targets.

## (v) Open

- Checking one body under several outcomes: a body whose lookups give two outcomes is checked
  twice today. Checking once with choice types (variational typing, Chen, Erwig and
  Walkingshaw, 2012) would share the common part; when it pays is not measured.
- Outcomes multiply across the lookups of one body (two lookups of two outcomes each are up to
  four continuations); whether `MOST_OUTCOMES` is reached on real code is not measured.
- Layout and constant evaluation that read a target's size read target atoms: their read list
  is the target's atoms, which may be most of them.
- `sat_count` refuses past 127 variables (a `u128`); a workspace's features can pass that.
- A build script's cfgs are per target, so its outputs are reads under a target's cube.
