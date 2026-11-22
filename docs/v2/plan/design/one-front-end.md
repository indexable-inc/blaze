---
status: plan
summary: "Three programs answer the back half's seam (crate::seam::FrontEnd): the old Front, the whole Front2Program and the lazy LazyProgram. This page is their inventory (who makes each, which answers are one shared rule and which are written twice and how they differ, what only one of them can do), the test that holds the whole and the lazy program equal (tests/front_equal.rs), and the order in which the shared rules are made one and the two older programs are deleted, leaving the lazy program alone (rules 30, 78)."
date: 2026-11-18
---
# One front end

The back half (run.rs `build_with`, run/make_one.rs `make_instance`, the record walk) asks every
question of a program through one trait, `seam::FrontEnd` (crates/blaze-ladder/src/seam.rs:143).
Three programs implement it. The end state is one: `LazyProgram`, which makes one body at a time
from records (D106), is the program `blaze run` already uses. The other two are deleted once
nothing needs them and the equality test is green.

Line numbers are as of 2026-11-18 01:30 (program.rs is being edited by lane V10: grep the name).

## 1. Inventory

### (a) Who makes each program

| program | made at | by | path |
|---|---|---|---|
| old `Front` (front.rs, `impl FrontEnd for Front` seam.rs:428-571) | run.rs:1712 `build_noting` (`Front::new`) | `run::run` (run.rs:1433, `FrontChoice::Old`); `build_checked`/`build` (run.rs:1566, 1580); src/tests.rs:41; tests/hosted.rs:32; blaze-real/examples/check.rs:457, 516, 616 | the oldest ladder runs and the hosted test |
| `Front2Program` (program.rs:292; seam impl program.rs:1555-1988; rows by tabled.rs) | `new` 627, `survey` 639, `survey_from` 653, `survey_within` 670, `make_from` 740, `suite` | run.rs:1601 `build_over` and 1585 `build_new` (`FrontChoice::New`: tests/ladder_front2.rs:73, examples/front2_rungs.rs:288, 372, every `run_with` test); run.rs:1688 `build_stubbed` (`survey`: `run_program`, tests/std_ladder.rs:172, stub_run.rs, trap_run.rs, examples/hello_stops.rs:69, 90); run.rs:1757 `build_records_new` (tests/code_records.rs:128, 432; examples/fast_census.rs:428); program/walk_report.rs:401, 407; blaze-real/examples/tempo_build.rs:380, 383 (`build_survey`: the tempo build census); about thirty front2_* tests | the ladders (x-rungs and std ladder), the build censuses, `run_program` |
| `LazyProgram` (program/lazy/mod.rs; seam impl lazy/front_end.rs; rows by lazy/fill.rs) | lazy/mod.rs:291 `new`; `from_records` | run/make_one.rs:1018 `Maker::construct` (`RecordsProgram::from_records`), which blaze-cli run_parent.rs:572 `CommandFront::maker` makes (program.rs:147): **`blaze run`**; tests/front2_lazy.rs:170, 224; front2_lazy_data.rs:103, 130, 153; every `walk_records` test (local_statics.rs:200, front2_cast_infer.rs:759, match_layouts_run.rs:418, front2_ctor_fn.rs:417, d106_intrinsic_fallback.rs:226) | `blaze run` |

`blaze check` and `blaze engine-check` use none of the three: the engine (blaze-cli engine_*.rs over
crates/blaze-engine and front2's tables) reads front2's heads and bodies directly, and touches the
program layer only through `front2::program::run_entry::root_main` (engine_bodies.rs:2085,
body_types/covered.rs:48). The ladders use `Front2Program` (whole), the old ladder and hosted
tests use the old `Front`, and only `blaze run` and the record-walk tests use `LazyProgram`.

### (b) Each seam answer: one rule or two bodies

**One shared rule** (lazy/seam_rules.rs, called by both):
`fn_abi` (program.rs:1593, lazy front_end.rs:188), `track_caller` (1940; 199), `caller_site`
(1946; 206), `source_position` (1954; 220); `fmt_argument_new` through
fmt_template::argument_new_answered; the row's ABI through head_abi.rs `head_abi` and the
intrinsic rule through intrinsic_rule.rs `built` (both called by tabled.rs `row` and fill.rs
`row`). **program/seam_rules.rs is an orphan**: a second copy of lazy/seam_rules.rs that no `mod`
line names (program.rs declares only `mod lazy`; lazy/mod.rs:40 declares `seam_rules`). It is dead
text and is deleted in step S0.

**Two bodies, the same rule today** (each must become one call):

| method | whole (program.rs) | lazy (front_end.rs) | how written |
|---|---|---|---|
| `code_of` | 1980 | 275 | same; the library text by `names.item_text` against `NamesSource::text_of` |
| `adt` | 1780 | 371 | same row; the whole lends `self.tys.view()`, the lazy `store.tys.view()` and notes a `Fill::Store(Want::Adt)` need |
| `normalize`, `holds`, `adt_drop_impl` | 1851, 1639, 1650 | 289, 122, 127 | the first tabled body's world against the first live body's (`in_world`, front_end.rs:23) |
| literals (`int_lit` .. `str_pat`, `format_template`) | 1802-1844, 1817 | 431-461 | the tabled row's `Literals` by `fn_index`, against the live body's (`lits_of`, front_end.rs:61) |
| `sig`, `function` | 1676, 1563 | 155, 74 | the row's fields; the lazy row is columns (`rows.*`) |
| `c_variadic`, `legacy_const_generics`, `repr`, `discriminant`, `ctor_of`, `opaque_of`, `object_trait`, `lang_item`, `lang_named`, `fn_lang`, `drop_trait`, `intrinsic`, `static_item`, constants | 1580-1590, 1915, 1969-1975, 1766-1770, 1724-1743, 1926, 1959, 1864-1907 | 91-96, 391-411, 308-327, 270, 468-514 | same lookups into each program's own store |

**Two bodies that differ today** (found by reading; tests/front_equal.rs shows which a ladder program reaches):

1. `resolve` (program.rs:1611 against front_end.rs:109): the whole program binds an `extern "Rust"`
   `#[rustc_std_internal_symbol]` declaration (`__rust_start_panic`, `__rust_drop_panic`) to its
   definition by the panic strategy (internal_symbol::bound); **the lazy one has no such arm**, so on
   `std` such a call resolves to the bare declaration.
2. `row` (tabled.rs:72 against fill.rs:168), the one rule for a fn's head facts, is written twice and differs in four places:
   - `extern "llvm-intrinsic"` declarations: tabled.rs:99-121 routes them to `Intrinsic::Target` (ABI C, nounwind); **fill.rs has no such route**, so the lazy row asks `head_abi`, which has no ABI for the block, and refuses.
   - `#[rustc_std_internal_symbol]` declarations: tabled.rs:140, 151 table them as `Free` with no symbol; **fill.rs:205 does not ask `internal_symbol::declares`**, so a body-less internal symbol is tabled `Free` by the last arm without being told apart, and item 1 then has no binding.
   - a body-less fn that is none of the known kinds: tabled.rs:164 refuses `NoBody`; fill.rs:212 tables it `Free` (its body is `check_one`'s): the refusal moves from the row to the first `check_one`.
   - an intrinsic with a fallback body the walk did not lower: tabled.rs:175 refuses `NoBody`; fill.rs:224 tables it with no table entry, built from its body: here the lazy rule is the right one.
   - placement: tabled.rs:87 `placed_fn`, fill.rs:195 `placed_of`; and ctors: fill.rs:178 tables a ctor row itself (`placed_ctor`), the whole program in its census.
3. `lang_trait_fn` (1776 against 340): the lazy one also answers a member the keys never filed,
   from the trait's member list (`member_key_of`); the whole one answers only `filed_member`.
4. `item_named` (1790 against 417): the whole one formats `module::name` itself and names an
   impl's member by its container's path; the lazy one uses the one printer (item_shown.rs, rule
   30), which names an impl by its header. The lazy rule is right.
5. `nounwind` (1933 against 180): the same value (the row's), read from `Tabled` against a column.
6. `left_out` (1785 against 425): the whole one answers a survey's causes; the lazy one always
   `None` (a run leaves nothing out).
7. `valid_range` (1921, lane V10's): only the whole program answers it (the ADT row's
   `rustc_layout_scalar_valid_range_*` bounds); the lazy program has none, so a layout over it
   takes the default range: a niche the whole program sees, the lazy one does not.
8. `check_all` (1710 against 254): the lazy one refuses (`NotLazyYet`), so `build_with` (run.rs:1725)
   runs over the whole program only; `check_one` (1719 against 258): the whole one hands over a
   tabled body, the lazy one lowers and checks one.
9. Statics: the lazy program evaluates a static when a checked body names it (fill.rs:571
   `body_statics`) and lends it by `static_value` (fill.rs:610), which the maker calls outside the
   seam (make_one.rs:1000); the whole one evaluates every reached static when made.

**front2/reach.rs against the lazy making.** reach.rs `reach_all` (reach.rs:541) is a census: a
work list over each checked body's callees from the roots, capped (`cap_of`), every outcome filed
by class, the edges kept (`Census::edges`, program.rs:322, read by suite.rs `reach_of`). The lazy
program has no walk: `check_one` (fill.rs:21) makes the one body and the rows of what it calls
(`row` per callee head, `adt_rows`, `body_consts`, `body_statics`, `body_objects`), and the record
walk (make_one.rs:1103) is the only traversal. They share the per-body steps (`lower_made`,
`check_body`, `callee_target`) but not the walk; what only the census gives is the class-grouped
"left out" report and the static reach edges.

### (c) The gaps

Only the whole path can:
- run `build_with` (every body checked first: `check_all`), so `run_with`/`build_over`, the
  x-rung ladder and `FrontChoice::New` need it today;
- survey: leave a failing fn out and go on (`survey*`, `left_out`, `build_survey`, the tempo build
  census in blaze-real/examples/tempo_build.rs, walk_report.rs);
- make a test unit's suite (`Front2Program::suite`, suite.rs `reach_of` over the census edges);
- answer `valid_range` (until the lazy program reads the ADT row's bounds);
- bind `#[rustc_std_internal_symbol]` declarations and build `extern "llvm-intrinsic"` ones (b.1, b.2).

Only the lazy path can:
- make a program from names RECORDS without the solve (`from_records`, `NamesSource` over
  `Records`): `blaze run` in its own process;
- check one body (`check_one`), make a ctor's or a held fn item's row from its head alone (`head_row`), drop a done body (`done`);
- evaluate a static on demand (`static_value`), name a member the keys never filed
  (`lang_trait_fn`), name an impl's member by its header (`item_named`), build an intrinsic from
  its fallback body when the walk had not lowered it.

## 2. The equality test

tests/front_equal.rs (with front_equal/asked.rs, over.rs, shown.rs, fixtures.rs). Each program
(every `x0-*` twin rung, then a fixture of niches, a `rustc_layout_scalar_valid_range_start`
struct and a negative `repr(i8)` enum) is solved once; both programs are made over the same
names, each behind a recording `FrontEnd`, and both are driven by the SAME record walk with the
same maker (`Over`: `MakerState::new` then `make_instance`, as `Maker` does) and builder. Every
question both were asked is compared by its last answer, a type as its tree, a row as its key;
every walk row both made is compared by its code record's key and bytes. It prints
`asked: whole W, lazy L, common C, differing D (floor 0)` and
`code records: both built B, equal E, differing F (floor 0)`, and each program's first
difference as `<program>: <question> <key>: whole <answer> / lazy <answer>`. The floor is 0 on
both counts; the ratio is the count itself. The control (rule 23) flips the lazy side's `fn_abi`
for one key and must see that question differ.

## 3. The order of removal

### The shared-rule steps (each one small step, its two files)

- **S0** delete program/seam_rules.rs (the orphan; no file names it).
- **S1 `row`**: one fn `head_row(names: S, keys, store, key, of_trait) -> Result<HeadRow, Front2Refusal>`
  in program/head_abi.rs holding tabled.rs `row`'s head part (symbols, placement, the llvm-intrinsic
  route, panic entry, alloc shim, internal symbol, kind and symbol, the intrinsic rule, the ABI);
  tabled.rs `row` and fill.rs `row` each call it and add only the body (tabled) or `push` (lazy).
  The `NoBody` and fallback-body arms take the lazy rule (a body-less fn is `check_one`'s refusal).
  Files: program/tabled.rs, program/lazy/fill.rs (head_abi.rs is the home).
- **S2 `resolve`**: one fn `bound_declaration(names, index, store, item, alloc) -> Option<ItemKey>`
  in lazy/seam_rules.rs (panic entry, alloc shim, internal symbol by the program's strategy), called
  first by both `resolve`s. Files: program.rs, lazy/front_end.rs.
- **S3 `item_named`**: the whole program answers through item_shown.rs as the lazy one does. Files:
  program.rs, lazy/front_end.rs (only program.rs changes).
- **S4 `lang_trait_fn`**: one fn in lazy/seam_rules.rs (`filed_member`, else the member list).
  Files: program.rs, lazy/front_end.rs.
- **S5 `valid_range`**: the lazy program answers from its store's ADT row
  (`store.adts.get(&key).map_or(ValidRange::default(), |a| a.valid)`, the whole program's rule),
  moved to lazy/seam_rules.rs and called by both. Files: program.rs, lazy/front_end.rs.
- **S6 `code_of`**: one fn in lazy/seam_rules.rs over `NamesSource::text_of` (`Names` implements
  it). Files: program.rs, lazy/front_end.rs.

### The lazy gaps that close first

1. S1 and S2 (internal symbols and llvm intrinsics on `std`), S5 (`valid_range`).
2. A lazy `build_with`: `run_with`/`build_over` drive the record walk over a `LazyProgram` (as
   `Maker` does) instead of `check_all`; then `FrontChoice::New` and the x-rung ladder run lazy.
3. A lazy survey: the walk files a refused instance by class and goes on (make_one.rs's maker
   already returns the refusal per instance; `walk_records` stops at the first): `build_survey`,
   walk_report.rs and the tempo build census then run over the lazy program.
4. A lazy suite: a test unit's roots walked by the record walk; its static reach read from the
   walk's rows instead of the census edges.

### The deletions, in order (not this piece)

1. **Old `Front` and front.rs's program** (seam.rs:427-571 impl, `build_noting`, `run::run`,
   `FrontChoice::Old`). Makes dead: `FrontChoice`, the `Old` arm of `run_with`, `eval_consts`, the
   trait's "old front" defaults (`c_variadic`, `left_out`, `nounwind` .. answering `None`). First
   green: tests/hosted.rs and src/tests.rs moved to `FrontChoice::New`; ladder_front2.rs.
2. **`Front2Program` and tabled.rs** (program.rs's struct, `make`, `survey*`, `suite`,
   walk_report.rs, left_out.rs, and the `FrontEnd` impl). Makes dead: `check_all` in the trait
   and `build_with`, `left_out`, `last_projection`'s census use, `Tabled`, `Verdict`, `class`.
   First green: tests/front_equal.rs (differing 0, code records differing 0, no GAP on any twin),
   then the x-rung ladder, std_ladder and the tempo build census over the lazy program (gaps 2-4).
3. **The reach walk** (front2/reach.rs `reach_all`, `Census`, reach/instances.rs,
   reach/initializers.rs). Makes dead: `MOST_FNS`, `cap_of`, `Within`, the census report.
   First green: the suite over the record walk (gap 4) and blaze-real's tempo census.
