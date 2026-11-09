---
status: plan
summary: blaze's `blaze::borrow_workaround` and `blaze::nll_workaround` — code written around a weaker borrow checker than blaze's (a second lookup, a clone to end a borrow, an index loop for a cursor, take and put back), found only when the direct form passes blaze's checker on the rewritten MIR and behaves the same; what is built (the double lookup), how a rewrite is checked, how published crates are kept building on stable Rust, and the shapes still to build.
date: 2026-11-02
---
# Borrow-checker workarounds

`crates/blaze-lints/src/blaze/borrow_workaround.rs`; the user's docs are
[docs/lints.md#borrow_workaround](../../../lints.md#borrow_workaround) and
[#nll_workaround](../../../lints.md#nll_workaround).

blaze's default borrow checker is the Polonius alpha (`BLAZE_BORROWCK`, `hir_ty_borrowck::mode`),
which accepts NLL problem case #3: a loan whose region flows to the return on one branch and is
dead on the other. Code written for stable Rust's checker (NLL) works around that, and the
workaround costs at run time. These lints find the workaround and offer the direct form, but
only after proving it.

## The rule for a finding

1. **It passes the borrow checker on this body.** A lint query has `&Db` and cannot edit the
   source speculatively, so the rewrite is made on the body's MIR and checked with
   `hir_ty_borrowck::borrowck_rewritten(db, def, &body, mode)`, the borrow check of `def` with its
   own MIR replaced (closures and `const` blocks from their queries). It is checked in the
   database's mode, so the lint follows the checker: when lane borrowck-cursor makes the default
   accept the cursor cases (rust-lang/rust#57165, #63908, #108704), a cursor shape built here
   fires with no change to the lint.
2. **It is checked again in `Mode::Nll`.** That says which lint: `borrow_workaround`
   (`blaze::general`, warn) when stable Rust accepts the direct form too, so the fix is right
   for every crate; `nll_workaround` (`blaze::internal`, allow) when only blaze's (and
   nightly's) checker does.
3. **It behaves the same**: no side effect lost or reordered, the same drops in the same order.
   Each shape below says why.
4. **The fix is re-checked.** `blaze check --fix` settles the crate after applying fixes
   (`crates/blaze-lints/src/fix.rs`), re-running `blaze_diagnostics::check_crate`, borrowck
   included, and restores the texts when a fix adds an error. The MIR rewrite is the form the
   fix's borrows take, not the fix's own MIR, so this catches any gap between them.

## Published crates

A crate graph does not say whether a crate is published or must build with stable Rust
(`package.publish` and the MSRV are not in `base_db`, and threading them through `Setup` and
`Conf` would touch every lint's input for this one). The split by checker answers it instead:
a fix stable Rust accepts is safe for any crate, so `borrow_workaround` warns everywhere; a fix
only blaze accepts is `nll_workaround`, allowed by default, on in a workspace that turns on
`blaze::internal` (blaze's does) or names it. A published crate that leaves the default keeps
building on stable Rust. If `base_db` later records `publish`, `nll_workaround` can warn in
unpublished crates of any workspace.

## Shapes

### Built: a second lookup (S1)

`if m.contains_key(&k) { .. m.get_mut(&k).unwrap() .. }`, `if m.get(&k).is_some() { ..
&m[&k] .. }`, `if o.is_some() { .. o.as_mut().unwrap() .. }`, and their shared forms, for std's
`HashMap` and `BTreeMap`, hashbrown's, `blaze_hash`'s (not papaya's or dashmap's, whose `get`
returns a guard with a drop). The fix is `if let Some(found) = <second lookup>`, the
`unwrap()`/`expect("..")` becoming `found`, any other use `Some(found)`, `&m[&k]` `found`,
`m[&k]` `*found`. This is NLL problem case #3 (rust-lang/rust#54663, #21906).

The MIR rewrite: find the `if`'s `SwitchInt` (span the `if`, discriminant `Copy(flag)` spanned
by the condition, `static_if(1, then, else)`), the probe's call that writes `flag` (its unwind
target), and the path from `then` to the lookup's call: every block with one way in, not a
cleanup block, ending in `Goto`s, with only `StorageLive`, `Nop`, `FakeRead` and assignments
of `Use(Copy | Constant | Move of a temporary made here)`, `Ref` or `CopyForDeref` to
temporaries, and no statement that may panic (`panic_unwinds`). Those statements move to the
end of the switch's block, which then calls the lookup into a new local, unwinding where the
probe did, and goes on to a new block holding the switch; the lookup's block becomes
`dest = move kept; goto next`.

Why it behaves the same: the probe is `contains_key` (std: `get_inner(k).is_some()`, `BTreeMap`:
`get(k).is_some()`), `get(..).is_some()` or `Option::is_some`, so dropping it removes a subset
of the lookup's own `Hash` and `Eq` calls, whose number std leaves unspecified; the keys and
places are literals, paths and fields (no calls); nothing runs between the test and the
lookup; the lookup returns a reference or an `Option` of one, which has no drop glue.

### Designed, not built

- **A `clone()` / `to_owned()` that only ends a borrow**: `let v
  = m.get(&k).cloned(); m.insert(..); use(v)` where a reference would pass. Rewrite: replace
  the clone's call with its receiver in MIR (the destination's type changes, so the rewrite
  needs a reborrow and a type check of every use) and check; behaviour needs the clone to be a
  derived or std `Clone` (no user effects) and the dropped value's drop to be glue-free or to
  move with it. The hard part is the types; the borrow check is the same entry point.
- **An index loop or `unsafe { &mut *ptr }` standing in for a cursor**: `while let Some(i) = ..; node = &mut node.children[i]` instead of
  `node = &mut node.next`. The alpha still rejects these (#57165, #63908, #108704); lane borrowck-cursor makes the default
  accept them, and then this shape rewrites the place chain in MIR and checks. Behaviour: the
  index form panics where the cursor form cannot, so the rewrite must also prove the index in
  range (the `checkfree` proofs).
- **`mem::take` / `Option::take` and put back**: `let x =
  self.f.take(); .. self.f = x;` around a call that borrows `self`. Rewrite: borrow `self.f` in
  place; behaviour needs no panic between the take and the put-back (else the value is lost
  differently) and a `Default` that runs no user code.
- **The crate's own function called twice** (`if bar(x).is_some() { bar(x).unwrap() }`): left out, since the function may have effects;
  a `#[blaze::pure]` function (lane pure) would let it in.

The rest of the issues in the brief (rust-lang/rust#51526, #51545, #58787, #68934, #92985,
#112087, #123839, #124070, #124254) and the upstream polonius tests are still to be sorted into
these shapes, each with a test, as each shape is built.

## Tests

`crates/blaze-lints/src/blaze/borrow_workaround_tests.rs`: one per shape that fires (problem
case #3 as `nll_workaround`; a shared `get` and `&m[&k]` as `borrow_workaround`; an option's
`is_some` then `as_ref`), and near misses (the database in `Mode::Nll`, so the fix fails the
checker; another key; a call between the test and the lookup).

## Baseline

A new lint starts with its count on blaze's tree recorded: `blaze check --workspace --target
aarch64-unknown-linux-gnu --baselines crates/blaze-lints/baselines --new-baseline
blaze::borrow_workaround,blaze::nll_workaround` writes
`crates/blaze-lints/baselines/blaze.borrow_workaround.txt` and `blaze.nll_workaround.txt`
(`crate<TAB>count`), which may only shrink. Neither is made yet, as no baseline is
([lints.md](../../../lints.md) says why).
