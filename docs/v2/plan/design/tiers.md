---
status: plan
summary: "One build goal, edit or max (D18; step 7 of scheduler.md): what done means for each, how a Cargo profile maps to one, what the status says, and the shape of the two tiers' keys. Built - the goal as one type from --goal to the policy, the goal in the record key, the Finished line. Not built - the goal on the live board, tier 0 for code a package override or std fixes at an LLVM level, optimized code stored by trace, background promotion (deferred; what the patch path lacks is listed)."
date: 2026-11-11
---
# Tiers: one goal, `edit` or `max`

A request waits for one of two things (scheduler.md, "The model", rule 8).

| goal | done when | never |
|---|---|---|
| `edit` | every function the request needs has tier 0 code (copy-and-patch stencils) and the program or test can start | waits for LLVM |
| `max` | every function the request needs has its optimized code (LLVM at the profile's level; `-O3` where the profile says 0 or 1) | answers with tier 0 code: a function that cannot be optimized fails the build, naming it and the rule (`PlanError::NoLlvm`; `--goal max` against `codegen-backend = "stencil"`) |

**Profile to goal** (`blaze_api::Goal::of_profile`, the one rule): `release`, `bench` and any
profile at `opt-level` 2, 3, `s` or `z` are `max`; every other (`dev`, `test`) is `edit`; a
profile that asks for `codegen-backend = "stencil"` is `edit`. `--goal edit|max` on `build`,
`run`, `test` and `check` overrides it (`Targets::goal`; agents: `goal` in blaze-client's
options). Build-time code (build scripts, proc macros) keeps its own profile's goal: it is not
the program.

**What the goal changes.** Only the level the profile's own code is compiled at
(`serve/unit/codegen.rs` `policy`): `OptLevel::tier_zero` under `edit`, `OptLevel::optimized`
under `max`. A profile already at its goal's tier (dev under `edit`, release under `max`)
gives the same `Policy` as before the goal, so the same keys and bytes. Overflow checks, debug
assertions and debug info stay the profile's. A package cargo gives its own level
(`[profile.*.package.*]`) and std's own functions (`-O3`, as shipped) keep that level under
both goals, as today; so under `edit` they are LLVM's at once (not tier 0 first), and under
`max` a package held at 0 or 1 stays stencils, which the status says and never counts as
optimized. Losing alternative: a third `goal` field on `Policy` read by `choose`; it would
give `dev --goal max` and `dev` at `opt-level = 3` two keys for the same code.

**What is said** (`blaze_api::Tiers`, in `Worked::tiers`): the `Finished` line ends
`(goal edit: tier 0 · 38% optimized)` or `(goal max: optimized)`; the share is of the functions
of the images this build linked whose code is LLVM at `-O2` or `-O3`, rounded down, and is left
out when the build linked nothing. Not built: the same on the live board (it needs a `goal`
field in `blaze_protocol::now::Now`, filled by the daemon's model, and a place in
`blaze_ui::board`'s line), and in `work_json`.

## Where the idea was, and what replaced it

| was | now |
|---|---|
| `serve::work::Goal` (Check, Build, Test, Run): a job's kind under the goal's name | `JobKind`; `Goal` is `blaze_api::Goal` |
| the profile's `opt-level` picking the backend alone (`plan.rs` `OptLevel::backend`) | the goal picks the level first; `OptLevel::backend` is unchanged |
| `blaze_lazy::allow`'s release rule (`mode.rs:78`: named `release`/`bench`, or optimized) | still there, and agrees with `of_profile` on every profile; it reads the policy's level, which the goal now sets. To merge into `Goal` when lazy runs are wired |
| `goal = fast\|balanced\|max` of blaze-opt (branch `opt-regions-1111`, `crates/blaze-opt/src/tier.rs`, optimizer.md decision 6): not in this tree | that is an optimizer effort inside the optimized tier, not a goal: rename it (`Effort`) when that branch lands; its `max` is not this `max` |
| `blaze_stencil::tier::Tier` (Stencil, Cranelift, Llvm) and `plan::Mode::Tiered`, count-driven tier-up of in-process runs; `Choice::then` and `Build::tier_up` (#807) | left: no path of blaze-cargo runs them (`serve/unit/codegen.rs:2721`, "a whole build tiers nothing up"). They are the second tier ladder; promotion (below) replaces them or is built on them, then one is deleted |

## Key shape

`code(f, tier 0)` and `code(f, optimized)` are two store keys: the backend is in `Spec::settings`
(`settings.rs:278`), the settings in `Instance::key` (`blaze-codegen/src/lib.rs:72`). Test:
`plan.rs` `the_two_tiers_of_a_function_are_two_store_keys`. A unit's record holds one tier's
code, and its unit and plan are the same under both goals, so the goal is in the record key
(`compile.rs` `keyed_goal`; test `the_goal_moves_a_units_record_key`).

**Tier 0 has an up-front key.** `code(f, tier 0)` depends on f's own MIR, the signatures and
layouts it names (`Instance::args`) and the backend; never on a callee's body, an effect
summary or an inlining decision, so a body edit dirties one tier 0 node. Today, for a body
written in the workspace or a dependency: true. A tier 0 unit asks for MIR level 1, no inliner,
no devirtualization (`hir-ty-codegen/v2/build.rs:86` `Build::of`; test
`a_tier_zero_unit_builds_no_callee_into_its_bodies`), and `hir-ty-mir`'s `optimize` runs
`Inline` only with `options.inline` (`optimize.rs:232`) and `SpendFacts` only at `Release`
(`optimize.rs:249`, `:282`). Until 2026-11-13 a body written in core, alloc or std was at
level 2 and MIR-inlined under every unit, "as shipped", so a std generic instantiated at tier
0 was inlined and had `SpendFacts` run on its MIR, which asked the effect summary of every
callee: a cold hello-world spent 17.6 s of one codegen task in `SpendFacts unwind` under std's
`__rg_oom`. That exception is gone (below). The tier 0 template key also hashes `refs` (the
callees' keys it calls, for placement), not their bodies.

**The tier is in the MIR key, and decides the passes** (2026-11-13). `MirOptions::tier`
(`hir_ty::facts::Tier`) keys every optimized body; `hir-ty-mir`'s `passes_at(tier)` is the one
declaration of which passes that read past the body may run (each still under its settings),
`hir_ty::facts::proofs_at(tier)` of whether the facts they read spend proofs, and
`hir-ty-codegen`'s `Build::tier_of` of which tier a unit builds a body at:

| tier | who | passes past the body | proofs |
|---|---|---|---|
| `Zero` | every body of a unit that does not optimize (stencils, Cranelift, LLVM `-O0`), the library's included, and `Build::WALK` | none: no MIR `Inline` (`#[inline(always)]` callees are the code generator's), no `Devirtualize`, no `SpendFacts`, no `Specialize` | none |
| `Shipped` | the library's bodies under an optimizing unit, the copy the MIR inliner builds in, `shipped_cross_crate_inlinable` | `Inline`, `Specialize` (rustc's own) | none |
| `Optimizing` | the program's bodies under an optimizing unit, the interpreter at level 2 | `Inline`, `Devirtualize`, `SpendFacts` (unwind edges, repeated calls), `Specialize` | spent |

The library's bodies are MIR-inlined only where the unit MIR-inlines (`Spec::mir_inline`), so
`WALK` and `WALKS_INLINED` still list what every unit asks for. The checks (`nopanic`, `pure`,
`noalloc`, `nonblocking`, the role lints, `unreachable`) are no tier: they ask summaries on
demand with proofs spent. Every summary key holds its proofs (the sealed tables,
`summary_in`, `fn_facts`). Under `Spent` a function is proved once per definition where its
instances keep its blocks (`walk.rs` `proof_of`).

The floor (rule 67): optimizing a tier 0 body is a pass over its own statements per pass, and
nothing that grows with its callees: no summary, no proof, no callee's body is read. Tests:
`each_tier_runs_the_passes_its_declaration_lists`, `a_tier_zero_body_computes_no_effect_summary`
(hir-ty-pure), `a_tier_zero_unit_spends_no_proof` (hir-ty-codegen). Not measured on this tree.

**What making tier 0 hold for the library's bodies takes** (read from the code and a profile
of a new hello-world, 2026-11-12, lane newproj-1111; not built). Changing `Build::options` so a
library body follows a unit that is not optimized is the small part. Three more things read a
callee's optimized body under a tier 0 unit today:

1. *The walk is one answer for every requester.* An instance its users compile (a generic, a
   closure, an `#[inline]` function) is walked with the inlined passes when any origin's
   units inline (`Units::any`, `frontend.rs` `inlines_as`), and std's own units are `-O3` in
   every dev build, so every such instance a tier 0 member asks for is described under
   `Build::WALKS_INLINED` too, and its tier 0 template key hashes the references those passes
   list. A per-requester walk needs the origin where a function is prepared
   (`Prepare::prepare` has the unit and its spec, not its origin) and one rule for two origins
   of equal settings whose later specs differ (a node merges their describes by key).
2. *`Build::WALK` is every unit's pass.* It lists what a unit that optimizes and does not
   MIR-inline (`opt-level` 1, `s`, `z`, a release build that patches) builds: its own body
   plain, the library's inlined. With the library at level 1 under `WALK`, such a unit needs
   a pass of its own (optimized, not inlining), or its references are not listed.
3. *Which library functions a user compiles is inferred from optimized bodies.*
   `home::compiled_by_user` asks `shipped_cross_crate_inlinable` of each library function no
   attribute decides, which builds its body at level 2 with the inliner
   (`optimize/inline.rs`), and that reads its callees' and their effect summaries: in the
   profile, `key cross_crate_inlinable` held the walk up 2.7 s and `summary_in` ran 58,602
   times. The answer is one for the whole program (std's own units and the user's must
   agree), a function of the library crate's identity and the front end's code, so it is a
   record to keep under those (rule 52), a table a library crate, not a thing to compute in
   each database.

On Linux a dev program is walked whole (14,202 describes for hello-world, 12,043 functions
reached, all but `main` a store hit by a key that needs the describe first), so there the
describes are std's own `-O3` ones and only a kept describe (R7) or the trace-keyed entry
below takes them away.

**Optimized code cannot have one.** What it inlined and which summaries it read are found while
compiling. The design: store it under (f, tier, stage identity) with a recorded trace of
(dependency, hash) pairs, and reuse it when the trace verifies. What the tree records now, to
build that from: the bodies the MIR inliner built in (`CompiledFunction::inlined`, `refs.rs`
`built_in`), the facts `SpendFacts` and `Inline` relied on (`body.inlined.rely`,
`optimize/facts.rs:131`, `:140`, `:326`, `inline.rs:373`), the dyn facts devirtualization read
(`body.inlined.devirtualized`), and LLVM's imports with their units, refs and built-in bodies.
Today these are folded into an up-front key after the describe step
(`release_template_key`, `driver.rs:438`), which is why describing must finish before a store
lookup. As a trace they become the entry's recorded reads, as the store's records already keep
for a unit (`RecordInputs`, the env reads of `reads.rs`): look up by (f, optimized, toolchain),
verify each (dependency, hash), reuse or recompile. Not built.

## Promotion (later; not built)

Under `edit`, after the answer, the daemon makes `code(f, optimized)` for the same functions in
band 3 (scheduler.md rule 6), cancelled when an input changes, each patched in as it lands. The
promoted key is the key a `max` build makes (`OptLevel::optimized`), so a tier's bytes do not
depend on whether promotion ran. Deferred until edits cost only what changed and the patch path
is redesigned (entry slots, append-only code). What the path lacks today:

- **A body that outgrows its place.** New code never goes where old code was; it goes in the
  patch region's room (`blaze-link/src/place.rs:40`, `:54`: as large as the code, 64 MiB to
  1 GiB). Promoting a whole program fills it: `Refusal::NoRoom` (`lib.rs:200`, `file.rs:378`)
  and the image is linked from scratch. A kept AArch64 branch that no longer reaches is
  `Refusal::NoThunk` (`lib.rs:227`), also a link from scratch.
- **Who can be switched.** One function replaces another only through a slot
  (`Change::slots`, `lib.rs:107`, "only an image with a jump table has slots"). Only a live
  image's members' functions, and those with a `then`, are called through slots
  (`plan.rs` `patchable = live && member`); every other call is `Target::Direct`
  (`blaze-codegen/src/lib.rs:436`), so its callers must be compiled or relocated again.
- **Threads in the old body.** Old code is retired, not freed, until every thread that could
  be in it has left (`blaze-inproc/src/lib.rs:515` `retire`, the fence); a process that never
  reports holds it (`tests.rs:871`). A file on disk has no fence: a running copy keeps its
  clone, as a client's file is never written in place (`patch.rs:12`).
- **Signed Mach-O pages.** Every patched page is hashed again and the signature rewritten
  (`macho/sign.rs:112`, `:324`; only pages of room nothing used are free).
- **No band, no caller.** `blaze-graph`'s `sched` has no bands (scheduler.md step 5), and
  nothing in blaze-cargo calls `Build::tier_up` (`driver.rs:3229`).

Proof when built: a DST (`dst-not-review`) whose oracle is "an edit that lands while a
promotion is in flight never publishes code of the old input".
