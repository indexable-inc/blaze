---
status: plan
summary: Function contracts (nightly's `#![feature(contracts)]`) in blaze — the same as the pinned nightly first, then checked by default in dev and test profiles, tested by Hegel-generated property tests, proved in Lean (formal-verification.md; the Z3 tier was retired on 2026-11-10), and used by the optimizer only once proved.
date: 2026-11-02
---
# Contracts: checked, generated, proved, used

Lane `contracts`, approved by Andrew on 2026-11-02 (tier 4, "proved", added the same day). The
handoff is `/tmp/blaze-lanes/v2-contracts.md`.

**One spec form (Andrew, 2026-11-02).** blaze's contracts are Verus's syntax made native
grammar, gated per crate, parsed and lowered to clause groups in hir-def
([verus.md](verus.md), which records the lane that built it):

```text
fn f(x: u32) -> (r: u32) requires x < 10 ensures r > x { .. }
```

The same lane owns loop `invariant`, `spec fn`, `forall` and `==>`. This lane parses no second
syntax. Parts 1 to 3 read the native clauses from HIR, in the shape hir-def's clause groups have in its
`## Interface`.

Nightly's `#[core::contracts::requires(expr)]` / `#[core::contracts::ensures(|ret| expr)]`
(`#![feature(contracts)]`) stay only for compatibility:
- they behave exactly as on the pinned nightly (part 0, rule 4);
- a lint rewrites them into the native clauses (part 0b).

The parts:

0. **Nightly parity.** The pinned nightly's behaviour for `#[core::contracts]`, exactly
   (rule 4).
0b. **Port lint.** `blaze::native_contracts`, with a MachineApplicable `blaze check --fix`
   that rewrites the nightly attributes to native clauses, in crates that opted into the
   extension.
1. **Checked in debug.** A dev or test profile checks native clauses by default, with an
   opt-out per profile.
2. **Tested by generation.** A function with native clauses gets a Hegel property test.
3. **Used by the optimizer, once proved.** A fact is used only when a proof stands behind it.
4. **Proved.** Lean checks a proof of `ensures` from `requires` and the model of the body
   ([formal-verification.md](formal-verification.md)); the Z3 tier that proved it with no
   proof written was retired on 2026-11-10.

## 0. What the pinned nightly does (nightly-2026-10-27, rustc 16d1ffac17ad)

Everything below was observed with `rustc -Zunpretty=expanded,hir` and by running binaries
built with and without `-Zcontract-checks`. rustc's sources at that commit say the same:

- `rustc_builtin_macros/src/contracts.rs`
- `rustc_parse/src/parser/generics.rs` (`parse_contract`)
- `rustc_ast_lowering/src/contract.rs`
- `with_new_scopes` in `rustc_ast_lowering/src/lib.rs`
- `checked_return` in `rustc_ast_lowering/src/expr.rs`

- **The attributes.** `core::contracts::{requires, ensures}` re-export the builtin attribute
  macros `contracts_requires` and `contracts_ensures`. Each one copies the item's tokens up to
  its `where` or its body and puts in the clause: `contract_requires { ARGS }` or
  `contract_ensures { ARGS }`.
- **Refused at expansion**, with rustc's messages:
  - an item with no `fn`: "contract annotations can only be used on functions";
  - an `async` or `gen` function: "contract annotations are not yet supported on async or gen
    functions";
  - a function with no body: "contract annotations is only supported in functions with bodies";
  - an empty argument: "`requires` attribute requires an argument, e.g.,
    `#[requires(condition)]`". The item is kept as written.
- **Which clauses parse.** The parser takes at most one `requires`, then at most one `ensures`.
  So two `requires` fail, two `ensures` fail, and an `ensures` written above a `requires` fails.
  The outer attribute expands first, so in that last case the `requires` clause lands after the
  `ensures` clause.
- **What a `requires` is.** It is a block, `{ DECLS; PRECOND }`. The tail becomes the closure
  `|| PRECOND`. Earlier statements are "contract declarations", which run before the check. A
  block with no tail gives `|| ()`, which type checking refuses.
- **What an `ensures` is.** It is the expression `{ ARGS }` given to
  `contract_build_check_ensures`, whose argument must be `Fn(&Ret) -> bool + Copy + 'static`.
- **Lowering.** `CC` stands for the literal `sess.contract_checks()`, which is
  `-Zcontract-checks` and defaults to `false`. Neither `debug-assertions` nor `opt-level`
  changes it. With both clauses, the body becomes:

  ```text
  let __ensures_checker = if CC { DECLS; contract_check_requires(|| PRECOND);
                                  Some(contract_build_check_ensures({ ENS })) } else { None };
  { let __ret = { BODY }; if CC { contract_check_ensures(__ensures_checker, __ret) } else { __ret } }
  ```

  The other combinations:
  - `requires` alone: `{ if CC { DECLS; contract_check_requires(|| PRECOND); } BODY }`.
  - `ensures` alone: the first form, without the `requires` call.
- **Returns are checked too.** Each `return e`, each `?` that returns, and each `do yeet` in the
  function's own scope becomes
  `return { let __ret = e; if CC { contract_check_ensures(__ensures_checker, __ret) } else { __ret } }`.
  This does not reach inside closures, `async` blocks or `const` blocks (`with_new_scopes`
  clears it), nor a `?` inside a `try` block.
- **Checks off.** The clauses are still type checked and borrow checked, but never run.
- **Checks on.** A false clause calls `panic_nounwind("failed requires check")` or
  `panic_nounwind("failed ensures check")`, so the process aborts with exit 134 and does not
  unwind.
- **Const evaluation** runs the `const` arm of `const_eval_select`, which does nothing. A `const`
  that breaks a `requires` still evaluates.
- **std's own contracts.** The pinned library has none: `rg 'contracts::'` over `library/` finds
  only the module's declaration. The intrinsics keep their fallback bodies, and blaze already
  compiles those (`Intrinsic::ContractCheckRequires`, `ContractCheckEnsures`).

blaze before this lane:
- It had no expander for these two builtin attributes (`MacroExpander::UnimplementedBuiltIn`),
  so a crate using them did not build as nightly builds it.
- It refused `-Zcontract-checks` as `OffOnly`.

### What blaze does

- **The expanders.** `hir-expand` gets `BuiltinAttrExpander::{ContractsRequires,
  ContractsEnsures}`, with rustc's checks and messages.
  - blaze's parser has no `contract_requires` syntax, so the clause goes in as a marker
    statement at the head of the body, after its inner attributes:
    `#[rustc_contract_requires] { ARGS };` or `#[rustc_contract_ensures] { ARGS };`.
  - The argument tokens keep their spans, so diagnostics point at what the user wrote.
  - A `requires` that finds a marker already there, or an `ensures` that finds an `ensures`
    marker, is refused. That is the same set of programs rustc's parser refuses, with an error
    that says why.
- **The lowering.** `hir-def`'s body lowering (`expr_store/lower/contract.rs`) reads the
  markers of a function's body and lowers the clauses exactly as `rustc_ast_lowering/contract.rs`
  does, in the same order, so closures are numbered as rustc numbers them.
  - `CC` is the crate's `cfg(contract_checks)`. The crate's cfg is an input of body lowering, so
    every key over the body covers it (rule 18).
  - The `return` and `?` rewrites hold for the function's own scope. Closures, `async` and
    `gen` blocks and consts clear them, as `with_new_scopes` does. `do yeet` is not rewritten:
    blaze's MIR building refuses `do yeet` in every function, with or without a contract.
  - Known gap: a user who writes `#[rustc_contract_requires] { .. };` as a body's first
    statement gets it lowered as a clause; rustc refuses that attribute without
    `#![feature(rustc_attrs)]`, which blaze does not check yet.
- **The intrinsics.** Lang items `contract_build_check_ensures`, `contract_check_requires` and
  `contract_check_ensures` are core's own functions. Their const arm does nothing and their
  runtime arm checks, as nightly's do.
- **The flag.** `-Zcontract-checks[=yes|no]` sets or clears `cfg(contract_checks)`
  (`blaze-units` `CfgArgs`). `blaze-cargo` no longer refuses it.

## 0b. The port lint (`blaze::native_contracts`)

- **Where it fires.** The lint is warn-by-default in a crate that opted into the native
  syntax, and silent elsewhere: such a crate still builds under rustc, which knows only the
  attributes. It fires on each function with `#[core::contracts::requires]` or `ensures`.
- **The fix.** It is MachineApplicable and keeps the meaning exactly:
  - `#[requires(DECLS; P)]` becomes `requires { DECLS; P }`.
  - `#[ensures(|r| E)]` names the return value: `-> T` becomes `-> (r: T)`, and the clause
    becomes `ensures { let r = &r; E }`. That keeps `E`'s view of `r` as a reference, as the
    closure had it.
  - A closure whose parameter is not a plain name (`|&r|`, `|(a, b)|`) gets
    `ensures (E_CLOSURE)(&r)` with a fresh name, which is always correct.
  - A function whose return value already has a name uses that name.
- **How it is applied.** Through `blaze check --fix` (rule 11). The edit is found from the
  attribute's resolved path (`core::contracts::requires`, through any `use` and rename), never
  from its text. The result compiles and is lint-clean.
- **Status (2026-11-02, in code, type-checked, not yet run).** `crates/blaze-lints/src/blaze/
  native_contracts.rs`, registered in `blaze-lint-defs` (group `blaze::general`). It finds the
  attributes through the expansion chain, by the macro name resolution chose
  (`BuiltinAttrExpander::ContractsRequires`/`ContractsEnsures`). As built:
  - a clause that starts with `{` is wrapped in `( )`, since the clause list ends at `{`:
    `requires ({ DECLS; P })`, `ensures ({ let PAT[: TY] = &NAME; E })` for any closure pattern;
    the call form `((CLOSURE)(&NAME))` only for a closure with `return`, `?`, a return type or
    `for<..>`; `ensures path(&NAME)` for a named function or const;
  - a function without `->` keeps its signature and the clause reads `&()`;
  - NAME is the existing named return, else the closure's plain parameter, else the first free
    of `ret`, `result`, `ret_`, `ret0`..`ret63`.
  - No fix, with a note naming why, where the meaning could change: the ensures reads a
    `requires` declaration, a `mut` or non-`Copy` parameter; the requires declares an item or
    can leave the function; another attribute macro or `cfg_attr` is in the chain; the function
    already has native clauses; the return type has `impl Trait`.
  - The `use core::contracts::{..}` import is left for `unused_imports`.

## 1. Checked in debug

- **The setting.** A profile gets `contract_checks: bool`.
  - Defaults: `dev` true and `release` false. `test` inherits `dev` and `bench` inherits
    `release`, as cargo's built-in profiles do.
  - The manifest key is `contract-checks = false` under `[profile.<name>]`, so a profile can opt
    out (or a release profile can opt in). Real cargo ignores the key with an "unused manifest
    key" warning, so the manifest still builds under cargo.
  - `-Zcontract-checks` is nightly's flag for nightly's attributes, and does not change this
    setting.
- **Release** checks nothing. Native clauses are type checked and never run, as nightly's are
  with checks off.
- **Not `cfg(contract_checks)`.** The setting governs the native clauses only. Nightly's
  attributes keep nightly's default and follow only `-Zcontract-checks` (part 0). The setting
  reaches the front end as the crate cfg atom `blaze::contract_checks`, a name no source can
  write, since a cfg name is never a path. `blaze-lower`'s `cfg` adds it after the target's cfgs,
  so `--print cfg` is unchanged. hir-def's lowering of a native clause reads it, as rustc's
  lowering reads `contract_checks`.
- **Keys.** The atom is part of the crate's cfg, which is an input of every body lowering. The
  unit's profile hash (`blaze-units` `graph.rs`, `load/plan/hash.rs`) includes it, so switching
  it rebuilds exactly the crates whose lowered bodies change.
- **No difference from nightly.** The setting reaches only native clauses, which only crates
  that opted into the extension have. Nightly's attributes are checked only under
  `-Zcontract-checks`, as on the pinned nightly.

### Messages

These are blaze's, under the rule useful-not-parity.
- **A broken `requires`** says:
  "`krate::path::f` was called with its contract broken: `requires(i < xs.len())`, at
  src/lib.rs:12 (contract src/lib.rs:4)".
- **A broken `ensures`** says:
  "`krate::path::f` returned a value that breaks its contract: `ensures(|r| *r < xs.len())`,
  returned `7`". The value is shown with its `Debug` when it has one.

How they are made:
- **Who emits the check.** hir-def lowers a native clause to HIR, as a clause group.
  This lane emits the run-time check from that HIR, after type inference, while building MIR
  (`hir-ty-mir` lowering of a function with clauses).
  - A `requires` is checked at the top of the body.
  - An `ensures` is checked at every return, as part 0 does for nightly's attributes.
  - The failing branch calls `panic_nounwind` with a constant message built from the
    instance's path, the clause's source text and its span. A native clause has no nightly
    meaning to keep, so the check also runs during const evaluation: a broken contract there is
    a const-eval error naming it, which is more useful than nightly's no-op.
- **The caller's file and line** need the check to happen at the call. Direct calls with checks
  on also evaluate the `requires` in the caller, on references to its argument temporaries,
  through part 2's predicate function.
  - The callee's own check stays, so calls through function pointers and vtables are checked
    too.
  - A clause with declarations, or whose arguments have drop glue and whose expression may
    panic (lane pure's `EffectSet::has(Rule::Panic)`), keeps only the callee's check. That keeps
    the order of unwinding the same. Its message names the function and the clause, and a span
    (`contracts.caller_check`) says why the caller check was left out.
- **`Debug` of the returned value.** In monomorphized MIR, the message formats through
  `<Ret as Debug>::fmt` when the trait solver proves `Ret: Debug`, and prints the type's name
  otherwise.
- **Status (2026-11-02, in code, type-checked, not yet run).**
  - The setting: `blaze_manifest::profiles::Profile.contract_checks` (serde-skipped, so the
    unit-graph JSON matches cargo's), read from the raw manifest by `read_contract_checks`
    (cargo's `TomlProfile` drops unknown keys) and applied by `Profiles::set_contract_checks`
    along the `inherits` chain. A non-bool value, or the key under `build-override` or
    `package.<spec>`, is refused naming the profile. Config profiles (`CARGO_PROFILE_*`,
    `.cargo/config.toml`) are not read yet.
  - `blaze_units::schema::Profile.contract_checks` (serialized only when true); the persisted
    schemas `planned`, `units` and `std planned` are bumped. The unit's class hash includes it;
    the `-C metadata` hash adds it only where it differs from `debug-assertions`, so default
    dev and release keep cargo's file and symbol names.
  - `cfg::CONTRACT_CHECKS` and `CfgOptions::contract_checks`; `blaze-lower`'s `cfg` adds the
    atom.
  - The reader: `hir-ty-mir` `mir/lower/contract.rs`. Messages are
    "`path` was called with its contract broken: requires `TEXT` (file:line:col)" and
    "`path` broke its contract: ensures `TEXT` on return (file:line:col)".
  - Refused, not skipped: an `ensures` check in a function with a `mut` parameter (the clause
    would read the changed value), a named return that is a pattern, and `become` in a function
    whose `ensures` is checked.
  - Not built yet: the caller-side check (and so the caller's file and line), the `Debug` of
    the returned value, and spec arithmetic: a clause runs as machine arithmetic (`n + 1` can
    overflow), where a proof reads it as an unbounded `int`.

## 2. Tested by generation

- **Opt-in per crate.** `#![blaze::contract_tests]` in the crate root generates the tests.
  `#[blaze::contract_test]` on one function generates one, and `#[blaze::contract_test(skip)]`
  leaves a function out.
- **Why opt-in first.** A generated test runs code the author never called with random inputs.
  - A function with side effects (files, a global) would do them in the test run.
  - Input generation needs a generator for each argument type, and many types have none yet.
  - Opting in per crate lets a crate turn on the tests where they make sense, without blocking
    every crate that uses contracts.
  - Once lane pure's check is in, a crate-wide default for pure functions is the next step.
- **The test.** For each opted-in function with contracts, the front end synthesizes a test
  item in a `#[cfg(test)]` module, as `#[test]`'s expansion makes one. It is
  `#[hegel::test]`-shaped and runs under `blaze test` with the crate's other tests. Each case:
  1. draws each argument with `hegel::generators` for its type: integers, `bool`, `char`,
     `String`, `Vec<T>`, slices by reference, tuples, arrays, `Option` and `Result`, and
     `#[derive(blaze::Arbitrary)]` types; a type with no generator makes the generation an
     error naming the argument and its type;
  2. calls `requires` as a predicate on the drawn inputs and rejects the case if it is false
     (`tc.assume`). A `requires` of a supported form (`a < b`, `a <= b`, `!x.is_empty()`,
     `i < xs.len()`, conjunctions) narrows the generator itself, so few cases are thrown away;
  3. calls the function and checks `ensures` on the result.
- **A failure** shrinks (Hegel's shrinker) to a minimal case. It prints a plain `#[test]` with
  the shrunk arguments as literals, ready to paste and keep, and names the function and the
  clause.
- **The predicates.** The same front-end lowering that makes the runtime check gives each
  function two synthesized predicates, `requires(&args) -> bool` and
  `ensures(&args, &ret) -> bool`. The generated test, the caller-side check (part 1) and the
  verifier (part 4) share them.
- **Status.** Part 2 lands after parity. The planted-bug test is in the handoff's test list.

### What is built (2026-11-02): `#[blaze::contract_test]`

- **The mechanism.** A builtin attribute macro, `core::blaze::contract_test`
  (`library/core/src/blaze.rs`, `#[rustc_builtin_macro]`, unstable under `blaze_verify`),
  expanded by `hir-expand`'s `builtin/attr_macro/contract_test.rs`. Name resolution reads the
  attribute path `blaze::contract_test` as `::core::blaze::contract_test` in every crate
  (`hir-def` `nameres/path_resolution.rs`, `attr_path_alias`), so a crate needs no `blaze`
  dependency. It is a macro, not a synthesis in `hir-def`, because the test is ordinary syntax:
  name resolution, inference, diagnostics, the IDE and `blaze test`'s harness see it as any
  `#[test]`, and its spans are the function's own, so an error in a clause points at the clause.
- **The expansion.** The function as written, then `#[cfg(test)] #[test] fn <f>_contract()`.
  Each argument is drawn with `hegel::generators::default::<T>()`; `&T` is lent from a drawn
  `<T as ToOwned>::Owned` (`&str` from a `String`). Owned arguments are passed as clones, so the
  drawn values stay for the `ensures` and the report: they must be `Clone`, and every argument
  `Debug`. Each missing capability (a Hegel default generator, `Debug`, `Clone`, `ToOwned`) is a
  compile error from a per-argument trait whose `on_unimplemented` names the function, the
  argument and the type. The `requires` are `tc.assume`d, the call runs under `catch_unwind`
  (a panic fails the case, named as the function's), and each `ensures` is checked with the
  named return bound to the result. A failure, on Hegel's last replay of the shrunk case,
  panics with ``#[blaze::contract_test] on `f`: `ensures r > x` does not hold`` and a plain
  `#[test] fn f_contract_case()` with the arguments as literals (their `Debug`, with
  `String::from(..)` and `vec!` around a `String` or `Vec`).
- **Refused, naming the function and why:** generic, `async`, `gen` and `unsafe` functions,
  methods and associated functions, a function inside a block, `&mut`, `impl Trait` and
  pattern arguments, a function without `requires` or `ensures`, an argument to the attribute
  other than `skip`, and a crate built with `cfg(test)` without a `hegel` dependency. On a
  refusal the function is kept as written. `recommends` and `decreases` are not checked.
- **Not yet:** the crate-wide `#![blaze::contract_tests]`, generators narrowed by a `requires`,
  and the shared predicates above (the test splices the clauses' own tokens).
- **Contract checks abort.** Part 1's checks (on in the dev and test profiles) call
  `panic_nounwind`, so a broken `ensures` aborts the test process (exit 134) inside the callee
  before the test sees the result, and Hegel never shrinks it. Until part 3's unchecked entry
  exists, a crate with generated tests sets `[profile.test] contract-checks = false`.
- **Using it.** `#![feature(blaze_verify)]`, the attribute on the function, and
  `hegeltest = { version or path, default-features = false, features = ["static-engine"] }`
  under `[dev-dependencies]` (its library is `hegel`).

## 3. Used by the optimizer, once proved

- **Never `assume` an unproved contract.** A wrong contract would turn into undefined behaviour,
  which rule 4 forbids.
- **The facts the optimizer may use:**
  - **Proved by existing passes.** blaze's MIR range facts (the ranges that
    `hir-ty-mir/src/mir/optimize/inst_simplify.rs` and copy propagation already derive: constant
    operands, `x & mask`, `x % n`, comparisons that dominate a use). An `ensures` whose
    expression reduces to such a fact about the return place at every `Return` is proved. A
    caller then uses it: `let r = f(xs); xs[r]` drops the bounds check when
    `ensures(|r| *r < xs.len())` is proved and `xs` is the same place.
  - **Proved in Lean** (`proved(..)` in part 4: a proof the pinned Lean checked, nothing less).
  - **Requires discharged at a call site.** When the caller proves the `requires` at a direct
    call, it calls the callee's unchecked entry. The callee then has two entries: its checked
    entry, for indirect calls, and the unchecked one.
- **Identity (rule 18).** Each use is a fact in the caller's code identity:
  `(callee instance identity, clause hash, how proved)`, where "how proved" is either the pass
  name or the Lean proof's `ProofKey`. The facts feed `hir-ty-identity`'s facts, and are folded as lane
  identity folds lane pure's memory facts. A change to the callee's contract or body changes its
  instance identity, and with it every caller that used the fact.
- **Report.** A span `contracts.fact` (target `blaze`) records the callee, the clause, how it
  was proved and the use (`bounds_check_removed`, `requires_discharged`). The same records go to
  lane vecreport's lint output, should it expose a hook (none is published yet).
- **Status (2026-11-02, in code, type-checked, not yet run or wired).** `crates/hir-ty-contracts`:
  `use_proved_contracts(db, &mut MirBody) -> Vec<Fact>` removes a bounds check
  `assert(idx < LEN)` on a constant-length array when `idx` comes (through single-assignment
  copies and unsigned casts) from a call of a non-generic function whose `ensures` has an item
  `r < K` (or `<=`, `>`, `>=` forms) with `K <= LEN`, and the prover it is given
  (`hir_ty_lean::proved`) answers `Ok`. An `ensures` that is not proved is reported by name with
  why (the event `contracts.not_proved`) and its check stays. A callee with a `requires` qualifies only when its crate has `contract_checks()`
  (the `requires` is then checked at its entry, part 1). Each use is a `Fact { callee: ItemKey,
  clause, how: How::Lean(ProofKey), entry, used }` recorded under the span `contracts.fact`.
  - Not wired into `optimize::passes`. It needs `proved` as a cached query with the Lean
    toolchain as a tracked input (today each call runs Lean, which reads `BLAZE_LEAN_PROVER`
    itself), the place of each crate's `lean/`, and `contract_facts` in `Inlined` hashed by
    `identity_of`, from lane identity.
  - Not built: `requires_discharged` and the unchecked entry (`proved` says a `requires` is not
    proved), generic callees (not translated), slices, and the range-fact prover.
  - **Since 2026-11-10 (the Z3 tier retired).** The clause this part reads, `r < K` over
    integers, was proved by Z3 with no proof written. Lean proves it only where the callee's
    crate has a proof file, so until one is written each such clause is "not proved: there is
    no proof file" and the bounds check stays.

## 4. Proved: Lean

The proof tier is [formal-verification.md](formal-verification.md): blaze translates a
function's MIR to a Lean model, states its clauses over the model, and the pinned Lean checks
the proof the crate keeps in its `lean/`. Until 2026-11-10 a fork of Verus (VIR, AIR, Z3)
proved arithmetic clauses with no proof written ([verus.md](verus.md) records it and what it
did that nothing does now).

The interface between the two is one fact each way.

- **Specs in.** One parse of the clauses: the clause groups in the function's body
  (`ExpressionStore::spec_clauses`), which this lane reads for its checks and facts and
  `hir-ty-lean` for the Lean statement.
- **Verdicts out.** Part 3 reads only a proof, through one function:

  ```rust
  /// Whether `clause` of `func<args>` is proved, and the key of that proof (which a caller
  /// optimised on it puts in its own code identity); else why it is not.
  pub fn proved(db: &Db, func: FunctionId, args: StoredGenericArgs, clause: Clause, proofs: &Proofs)
      -> Result<ProofKey, NotProved>;
  pub enum Clause { Requires, Ensures }
  pub struct ProofKey { pub how: Prover, pub id: ContentId }
  ```

  `NotProved` names the function, the clause and why: no proof file, a construct v0 does not
  translate, a generic function, a `requires` (proved at call sites, which v0 does not lower),
  Lean refusing the proof, an axiom that is not allowed. It is never a pass.
- **Hegel stays the cheap first pass.** Part 2's generated test runs first, and a counterexample
  it finds is reported before a proof is tried. It is now the only source of counterexamples.

### Prior art

- **Verus**: SMT, Rust-syntax specs, modular verification by callee contracts, a deterministic
  resource limit in place of a timeout. Its syntax is blaze's spec form; its prover was used
  until 2026-11-10.
- **Creusot**: conditions from MIR, prophecies for `&mut`, loop `#[invariant]`.
- **Prusti**: `old(e)`, pure functions usable in specs (here lane pure's `#[blaze::pure]`).
- **Kani**: bounded checking. It drives rustc, so it is not used.
- **Ada/SPARK and Eiffel**: one contract both checked at run time and proved; SPARK drops the
  run-time check of a proved contract; Eiffel's message names the routine and the clause.
- **The Rust project goal** ("contracts and invariants", rust-lang/rust#128044, compiler-team
  MCP 759): the attribute syntax this lane implements.
- **Crates such as `contracts` (x52dev/contracts)**: proc-macro `debug_requires`. blaze keeps
  nightly's built-in attributes instead, so the same source works under the pinned nightly.

## Tests (few, named for the property)

- `contract_clauses_lower_as_nightly_off_and_on` (hir-def `expr_store/tests/body.rs`, in code):
  the desugar has rustc's calls and guards with checks off and on, including the `return` and
  `?` rewrites and a closure's `return` that is left alone. A run-time test (exit 134 and
  `failed requires check` with `-Zcontract-checks`, nothing without it) needs a `blaze run` of a
  small crate and is the coordinator's.
- `contract_checks_leave_a_body_without_clauses_byte_identical` and
  `requires_and_ensures_abort_only_with_contract_checks` (hir-ty-mir `tests/mir/contracts.rs`,
  in code). The run-time e2e (a broken `requires` exits 134 in dev, 0 with
  `contract-checks = false`) is the coordinator's `blaze run`.
- `broken_requires_names_function_and_clause` and `broken_ensures_names_function_and_value`.
- `generated_test_finds_planted_bug` (blaze-engine `tests/repro.rs`, crate
  `tests/repro/generated_test_finds_planted_bug`, in code), and
  `contract_test_expands_to_a_test_of_the_clauses` (hir-def `expr_store/tests/body.rs`, in
  code): the alias resolves and the expansion assumes the `requires` and checks the `ensures`.
- `proved_contract_removes_bounds_check` and `unproved_contract_keeps_bounds_check`.
- `contract_must_be_pure`: a clause that reads the environment is an error naming it.
