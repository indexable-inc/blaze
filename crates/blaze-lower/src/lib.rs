//! # For a successor
//!
//! State (2026-11-16, ~10:45 by the build clock, relay runner after the one below; handing on).
//! STEP 1 PROVEN: blaze-lower "110 passed"; `a_pattern_type_expands_to_its_one_form` fails with
//! the `PATTERN_TYPE` arm removed ("left: 0 right: 2"); front2_bounds "1 passed", front2_main
//! "5 passed"; front2_items "7 passed, 2 failed" (`a_refused_impl_is_a_named_gap`,
//! `a_negative_impl_is_a_head`: impls, not pattern types; other lanes' WIP). Census NOT rerun.
//! STEP 2 FIXED: `$crate` in a body expansion lost its crate at the text boundary: blaze-expand
//! `body_words` wrote `$crate`, and the ladder's `to_path` read it as the BODY's crate. Now a
//! marked `$crate` keeps its MARK (token-mark-1/-2, drive.md "A token keeps its mark"; no
//! spelled token; one resolver, blaze-scope crate_roots `root_by_mark`); machine.rs
//! `library_macro` accepts it. Fixture scoped_tests `a_value_path_a_nested_expansion_writes_
//! names_the_defining_crate` failed without it (`$crate::deep::f` -> user's ItemKey(d1fa2cbf2a39),
//! dep's is 5650bb2226d6); blaze-lower "113 passed", blaze-expand "56 passed" (mixcheck: blaze-
//! solve alias.rs:526 E0004 broke lanecheck). front2_panics: only `a2` left (`if false {
//! core::intrinsics::abort() }` then `panic!`: its printout not seen: run with --nocapture).
//! Coordinator's 275 x E0425 in core `ptr::add` (`assert_unsafe_precondition!`): fixtures show
//! `::dep` through `extern crate self as dep;` and an `$kind` path segment RESOLVE; the third
//! suspect is the cause by reading: the macro defines `const fn precondition_check` IN the body's
//! block, and no body-local item exists in blaze-typeck, typeck.rs or front2 (`BodyWorld::
//! resolve_path` ignores its `ScopeId`): the type checker's lanes. Steps 3, 4 NOT started.
//!
//! State (2026-11-16, ~13:00, relay runner after the one below; budget spent, handing on).
//! FIRST (coordinator, ahead of everything): a `panic!`/`assert!` in any crate OTHER than core
//! leaves `$crate::panicking::panic_fmt` unresolved (front2_panics.rs, user crate on
//! blaze-2026.11: `UnresolvedPath`, hash 8263167416644613366 = `Name::of("panic_fmt")`; bstd
//! env.rs:74 E0425, hash 9982642335519403907: print it). NOT STARTED. Suspects: the `$crate`
//! mark of a path written by the second-level expansion (`panic!` -> `panic_2021!`); a body's
//! by-name `$crate` lookup starting in the user crate; a visibility test on `core::panicking`.
//! Fixture of two crates, then `lanecheck.sh blaze-ladder test "--test front2_panics"`.
//! STEP 1 (pattern types) WRITTEN, COMPILES, NOT YET TESTED: `TypeRef::Pat { base, pat: TyPat }`
//! (form.rs, tag 14, flag bit 8 = `TyPat::NotNull`, else `TyPat::Of(PatId)`); machine.rs lowers
//! `PATTERN_TYPE` to it and `NOT_NULL` to a `V::Tok(NOT_NULL)` value; blaze-surface
//! `BuiltinFn::PatternType = 26`; blaze-expand builtin.rs `pattern_type` writes
//! `builtin # pattern_type(input)`; `lower_type_text_expanding` (type_text.rs); dump.rs arm.
//! front2: pattern_ty.rs rewritten (`macro_ty` expands a SIGNATURE's type macro from its own
//! text with `Scoped { prepared: None }` + body.rs's `NoHost` (made `pub(super)`), refuses a
//! leftover macro; `pat_ty` reads `TypeRef::Pat`, the pattern's text through
//! `parse_pattern_type`); types.rs dispatch + `children` arms; front.rs refuses `Pat`.
//! `lanecheck.sh blaze-ladder` compiled everything but front2_checks.rs:86 (E0308, array size
//! 30 vs 29: the second checker lane's test file, not this change). Owed: run blaze-lower's
//! suite (new fixture `a_pattern_type_expands_to_its_one_form` in expand_tests.rs; run it once
//! with the `PATTERN_TYPE` arm removed and quote the failure), front2_items/main/bounds, the
//! census (failed-by-macro-4.txt), and ONE message to the ITEMS lane (pattern_ty.rs ported).
//! Step 2 survey: the edition_panic flag needs `DefAt.edition_panic` (built in blaze-scope
//! resolve.rs:1995 from `MacroSrc`, whose record in crate_scope.rs:246/593 must carry it), and
//! `Mark` (blaze-expand tokens.rs:265) a `panic_edition` set from the call's when the def has
//! the flag; by_name.rs:449 then reads `mark.panic_edition`. Steps 3-5 not started.
//!
//! State (2026-11-16, ~11:30, relay runner after the one below). DONE: (1) D60 includes: blaze-
//! expand `ReadRoot { origin: SourceOrigin, package }` given per unit by the host;
//! `include_target(root, folder, written)` gives root-relative segments, bound = whole root for
//! Checkout/Workspace/GitCheckout, the package for `Package`; refusal `OutsideRoot` (D60 in its
//! Display; the finding says "Include (the path leaves the unit's source root: decision D60)").
//! blaze-real names_load gives each crate its root (`source_root`: nearest `.git` above the
//! workspace, else the workspace; git packs: the checkout; registry: the package) and
//! `Loaded::root_bytes`. blaze-scope's item-position host keeps the package bound (its
//! SourceTexts hold the package only: names lane's to widen). Read subject is still the WRITTEN
//! path (D60 wants root-relative: needs `Included::Bytes` to carry the resolved path; 8 sites).
//! (3) `Expand::expand` takes `cfg: &mut dyn Cfg`: item-position `cfg!`/`cfg_select!` answered by
//! the file's set. (4) `panic_edition` = the path's mark's edition (defining crate's); gap:
//! `allow_internal_unstable(edition_panic)` is not on `DefAt` (core `debug_assert!` from a
//! 2015/2018 caller now takes core's edition). Fixtures each failed with the cause removed.
//! NOT DONE: (2) pattern types as one form `TypeRef::Pat` (plan in the report); (5) tables.
//!
//! State (2026-11-16, ~10:40, relay runner after the one below; budget spent). (1) Tempo's
//! table: blaze-real stages h' prints "BY MACRO AND CAUSE" (`scoped_body`'s `by_macro`); run
//! port27/expand-1116/tempo-run-1.txt, table tempo-by-macro-1.txt: "11720 bodies: 11303
//! clean (96.4%)", "stopped only by macro calls: 278"; 592 calls in 49 kinds, nearly all
//! `path: NotFound`/`NotRead` (address 148, quote 142, vec 49, parse_quote 44 ...: the names'
//! resolution, not expansion) plus 11 `include_str` "Include" refusals of paths outside the
//! package (`../../crates/chainspec/..`: the roots rule refuses what Rust allows: decide).
//! (2) panic arm: fixtures `a_panic_arm_is_its_block_at_the_call_s_range` and
//! `an_expanded_panic_block_checks_as_never` (lowered with expansion, then to_typeck+check)
//! PASS: the expansion's block, its range and divergence are right for `({ stop(..); })`.
//! The empty place is the checker's: blaze-typeck machine.rs `coerce_many_push(site, None,
//! ..)` gives `At::Body` (empty range) for a tail-less block's forced `()`; it fires when the
//! block's statement did not set `diverges`: suspect `panic_fmt(const_format_args!(..))`
//! (a `$crate::` path, a format_args argument): checker lane's. (3) `pattern_type!`: NOT a
//! builtin to expand: front2's pattern_ty.rs reads the UNEXPANDED `TypeRef::Macro` (resolved
//! as `MacroBody::BuiltinFn("pattern_type")`); a builtin expanding to `builtin # pattern_type`
//! needs `TypeRef::Pat { base, pat }` and pattern_ty ported to it first (tried, reverted).
//! The census row is only the expander being asked: keep the call as the type's form, no
//! finding. (4) not started. (5) fixture `a_braced_cfg_select_in_a_block_is_answered_by_the_
//! body_s_cfg_set` (inst_partition's shape) PASSES here: the host is reached by another path
//! in the census (suspect the item-position `Expand::expand`, which still asks the host).
//! (g) owed line: with the finding push removed, `a_failed_body_call_is_a_finding_naming_its_
//! path_and_error` failed "assertion `left == right` failed: [] left: 0 right: 1"; restored.
//! The checker lane added `Types::const_arg`: impls in tests.rs and expand_tests.rs updated.
//!
//! State (2026-11-16, ~09:30, relay runner on the walk's "42 x lower: Lowered ... MACRO_CALL,
//! ExpansionFailed"). CAUSE: not `panic!`: `cfg!` (core `panic_bounds_check`'s range 294..325 is
//! `cfg!(panic = "immediate-abort")`), refused by front2's `NoHost`. (1) A failed body call is
//! now a finding: `by_name::NotExpanded::Failed(Failure)` (`Diagnostic { diagnostic, text }`,
//! `Expand(ExpandError)`, `Lex`, `Stream`, `NoDelimiters`, `NoExpansion`, `EagerNotFnLike`,
//! `EagerAgain`); front2 prints it through `BodyFinding::Macro` unchanged. (2) A body's `cfg!`
//! (and `cfg_select!`) is answered by the body's own cfg set, never the host: `Expand::
//! expand_body` takes `cfg: &mut dyn Cfg` (whole port: tests.rs, lower_std, blaze-proc-macro's
//! tempo_pipeline), `Machine::lend_cfg`/`return_cfg`, `by_name::BodyCx` wraps the host with
//! `cfg_outcome`. Fixtures: `a_body_s_cfg_is_answered_by_the_body_s_cfg_set_not_the_host` (cause
//! removed: "a `cfg!` the body's set answers true expanded to `false`"),
//! `a_failed_body_call_is_a_finding_naming_its_path_and_error`. Census (blaze-ladder
//! tests/expand_census.rs, this lane's file; `EXPAND_CENSUS_OUT`): port27/expand-1116/
//! failed-by-macro-1.txt "34347 bodies, 665 with a macro call not expanded" -> -2.txt "23".
//! The panic family, assert*, unreachable!, format_args! etc.: no failing row in core, alloc or
//! bstd. OWED: `pattern_type!` 21 (BuiltinUnknown: a type-position builtin), one `cfg_select!`
//! in core `quicksort::inst_partition` still "Host(Cfg Refused)" (`feature = ".."` arm then
//! `_`: find why it reaches the host), `asm!` 1 (no lowering); `panic_edition` in `ByName::call`
//! is the BODY's edition for calls inside expansions too: should be the defining crate's
//! (via `PathAt`'s mark), except under `allow_internal_unstable(edition_panic)`; an item-position
//! `cfg!` (`Expand::expand`) still asks the host. NOT SENT (no message tool in this runner): to
//! the RUN-path lane (OWNERS.md), "the 42 MACRO_CALL bodies: rerun the owner's walk".
//!
//! State (2026-11-16, ~05:40, relay runner after the one below; handed on at its context
//! limit). Verified: "test blaze-lower: exit 0; 86 passed, 0 failed" before this runner's
//! changes, "mixcheck test blaze-lower: exit 0; 88 passed, 0 failed" after. Step 1 DONE:
//! (a) a call whose reads cannot join the body's read list is a finding, not a drop:
//! `MacroFinding::why` is now `by_name::NotExpanded` (`NoMacro(..)` | `ReadList(ReadLimit)`),
//! `ReadLimit::{Cap(u32), Table(StreamError)}`, cap `BODY_READ_CAP` (1 << 16) or
//! `ByName::with_read_cap`; `ByName::join_reads` is the one place reads join (attr and derive
//! too, which have no range and stay `Expansion::Failed`); stages' `miss_cause` takes
//! `NotExpanded`; test `a_call_past_the_read_list_s_cap_is_a_finding_naming_the_cap`.
//! (b) test `an_include_of_out_dir_reads_the_script_record_s_file` (`include!(concat!(env!(
//! "OUT_DIR"), "/x.rs"))` over blaze-build-script's `ScriptCx` with a record; host alone: the
//! call fails); blaze-build-script is a dev-dependency; the scoped fixture's `dep` gained
//! `env!` and `concat!`. Step 2 PARTLY: h' prints "ROW library and binary targets: N bodies:
//! Y clean (set: ...)" and "ROW test, bench and example targets: not measured (set: ...;
//! missing: ...)": blaze-unit-graph has no unit for integration tests, benches or examples
//! (lib.rs "# Not covered"; resolve.rs `Kind` = TestLib/TestBin only) and names_load asks
//! `tests: false`. NOT DONE: retire stage h (stages.rs `bodies`, `lower_bodies`,
//! `set_body_texts`, `body_rules`, `body_def_at`, `BODY_TEXTS`/`BODY_RULES` and the
//! `bodies(..)` call in `main`; `lower_one`/`BodyTally` stay, h' uses them; then the library
//! argument is used only by the startup check); NOT RUN on tempo: this runner could not
//! find the module-store `<out>` argument of hprime-8's command (the scratchpad holds
//! `pm/store-c` and several `out-*`): find it before the run. blaze-real's lanecheck was
//! blocked by blaze-build walk.rs:4592 (another lane's): use mixcheck. Steps 3, 4, 5 not
//! started.
//!
//! State (2026-11-16, ~05:10, relay runner after the one below; handed on at its context
//! limit). Step 0 EXPLAINED, neither suspect: hprime-6 was run with the library argument
//! `.../rust/lib` (not `.../rust/library`): its line 175 reads "28 definitions ... from 0 files
//! of .../rust/lib", against hprime-5's "455 definitions ... from 198 files"; rerun with the
//! right folder and the read-list step in (hprime-7, green snapshot has `ByName::reads`):
//! "15045 bodies: 13090 clean (87.0%)", equal to hprime-5. stages.rs now exits on a library
//! folder without `core/src/lib.rs`. Step 1 DONE: the names lane's render_with fixes are in
//! (uncommitted); both `dollar_crate_*` fixtures pass ("test blaze-lower: ... 11 passed, 1
//! failed" with only the new include test failing then). Step 2 written: blaze-expand
//! `include_target` (path relative to the calling file, `..` lexical, kept in the roots; test
//! `an_include_path_is_relative_to_the_calling_file_and_stays_in_its_roots` passes),
//! `Included::Outside` / `IncludeFailure::Outside` (record byte 3); `ByName` keeps an
//! expansion's reads on an error too (a missing file is a read); fixture
//! `includes_read_beside_the_calling_file_and_never_leave_the_package` (scoped_tests.rs).
//! blaze-real: `names_load::load(.., scripts)` opens a folder of script records through
//! blaze-world's `ScriptRecords` (also given to the names as `CrateInput::script`);
//! stages' 7th argument is that folder (port27/scripts-1116/exported4); `BodyHost::include`
//! reads in sandbox space and h' wraps the host in `ScriptCx` when the crate has a record.
//! scoped_tests: "mixcheck test blaze-lower: exit 0; 12 passed, 0 failed". Run hprime-8.txt:
//! h' "11720 bodies: 11297 clean (96.4%)", "stopped only by macro calls: 281" (was 407);
//! `crate :: hex/select/try_join` gone; left: `env (std)` 28 (src/version.rs:18: `VERGEN_*`,
//! no record exported for that script) and 92 `to_typeck` const-argument stops (checker's). Also done for the names lane: blaze-expand
//! `derive_meta` accepts `[::](core|std|alloc)::prelude::<m>::derive` (libc's `s!`), test
//! `derive_named_by_its_prelude_path_is_a_derive`. Not started: steps 3, 4, 5; a failed
//! `Reads::extend` in `ByName` is still `Refused::Failed`/`Expansion::Failed` (a limit, not
//! the drop's cause; make it a typed finding). The names lane runner five handed on (its note:
//! bottom.md section 6): read OWNERS.md for the next id.
//!
//! State (2026-11-16, relay runner after the one below; handed on at its context limit).
//! Step 1 (`$crate` paths that miss): reduced to two fixtures in `scoped_tests.rs` from the
//! real sources (alloy-primitives 1.7.3 `fixed_bytes_macros!` writing `address!` whose body
//! calls `$crate::hex!`; tokio 1.53.2 `doc!` handing `macro_rules! try_join` through an
//! `$item` fragment, calling `$crate::try_join!(@ ..)`): tests
//! `dollar_crate_in_a_macro_written_by_a_macro_names_the_defining_crate` and
//! `dollar_crate_in_a_macro_passed_through_an_item_fragment` FAIL (by design until the fix
//! lands): "crate :: hex ... segment 0: not found". CAUSE: the names library, blaze-scope
//! resolve.rs `render_with` (the expansion-to-text printer), writes `$` `crate` and a
//! `$crate` identifier as plain `crate` when it names the expanding crate; inside a
//! `macro_rules!` that expansion WRITES, `$crate` is a metavariable of the new definition and
//! must stay `$crate` (the Reference, Hygiene: `$crate` names the defining crate; the path
//! readers already read `$crate` back: `task/lower.rs` `to_path`). NOT YET SENT to the names
//! lane (this runner had no message tool): send acd2f3dd892b1b198 (check OWNERS.md) the two
//! fixtures and the line. Body side done: `Scoped::resolve` reads blaze-scope's
//! item text's root by its first segment's mark (blaze-scope crate_roots, token-mark-2).
//! Step 2 begun: `ByName` keeps the body's READ LIST (`ByName::reads`, blaze-expand `Reads`:
//! every expansion's `env!`/`include*!` reads, eager inputs', attributes' and derives' too,
//! reset in `begin_body`); test `an_environment_read_joins_the_body_s_read_list`. Not done:
//! stages' `BodyHost::include` (still refuses): plan: names_load exposes its `Texts` (bytes by
//! crate and path, the package folder = `shown_from` prefix); the host joins the written path
//! to the body's file's folder (`file.path`), normalises `..` lexically, refuses a result
//! outside the package folder (add `Included::Outside` + `IncludeFailure::Outside` in
//! blaze-expand so the diagnostic names the path); wrap the host in blaze-build-script's
//! `ScriptCx` with the unit's record (names_load's `CrateInput::script`, filled the way
//! blaze-world `examples/names.rs --scripts` does from port27/scripts-1116/exported4: only 2
//! tempo crates there: tempo-chainspec, tempo-precompiles; `VERGEN_*` come from a script not
//! exported yet). Steps 3, 4, 5 not started. Tempo run: port27/hprime-6.txt: h' "11720
//! bodies: 11244 clean (95.9%)", "stopped only by macro calls: 407", 674 `path: NotFound`
//! (names lane's `publish_expanded_exports` now in the tree); `crate :: hex` 63 / `select` 56 /
//! `try_join` 18 unchanged (the render_with cause). FIRST CHECK: stage h FELL in the same run,
//! "15045 bodies: 8848 clean (58.8%)" (hprime-5: 13090), 1822 `$crate :: panic :: panic_2021`
//! left: stage h uses `ByName::new` (`Lists`), untouched by `Scoped`; suspects: this runner's
//! `ByName::call` read-list extend (a `Reads::extend` error turns into `Refused::Failed`) or
//! another lane's blaze-expand change; test by removing the extend and rerunning h.
//!
//! State (2026-11-16, 05:xx, relay runner after the one below): step 1 (proc macros in
//! bodies) wired in the harness, nothing in this crate changed. blaze-real
//! `examples/names_load/mod.rs` now builds blaze-proc-macro's hook (`Procs`, `procs_of`,
//! mirroring blaze-world's `examples/names.rs` `procs_of`: one concept in two examples, merge
//! into one library home when the driver takes it) and its `Texts` implements
//! `SourceTexts::proc_macro_key`/`run_proc_macro`, so the names fill gives each proc-macro
//! crate its runner key (`CrateNames::proc_key`) and `definition_of` builds
//! `MacroDefinition::proc_macro(runner, ..)`. stages.rs `BodyHost` holds the runner: a body's
//! proc call runs in its crate's package and edition (`ProcTally` counts), `env!` answers from
//! `names_load::unit_env` (CARGO_PKG_NAME/VERSION(+_MAJOR.._PRE), CARGO_CRATE_NAME,
//! CARGO_MANIFEST_DIR/PATH under the sandbox's `/workspace`: no machine path in a key), refused
//! for `names_load::CARGO_SET` names it does not know and for every unset name of a crate with a
//! build script (its `cargo::rustc-env` is the build-script lane's `ScriptCx`: not read yet).
//! Run: `stages <checkout> ~/.cargo <library> <fetch store> <module store> <out>`; tempo's
//! fetch store and module store (proc-macro lane): scratchpad `fetch-store`, `pm/store-c`
//! under /private/tmp/claude-501/-Volumes-Projects-blaze/65dedbdf-09f9-429d-b209-308a28ee95a1/
//! scratchpad. Output: port27/hprime-4.txt (first) and hprime-5.txt (second, from records):
//! "11720 bodies: 11231 clean (95.8%)", "stopped only by macro calls: 420" (was 478), "body
//! proc-macro calls: 82 ran in the sandbox or came from a record, 0 refused, 0 failed", "73
//! registered, 72 with a module"; names 59411.7 ms first, 19871.5 ms second. Left: 687
//! `path: NotFound` (dependency `#[macro_export]` macro_rules imported by `use`: `address`,
//! `quote`, `b256`, `json`; sent to the names lane acd2f3dd892b1b198 with the 47 `NotRead`),
//! `crate :: hex` 63 / `crate :: select` 56 / `crate :: try_join` 18 (paths an expansion
//! writes with `crate` first: check `Scoped::resolve`'s mark handling). Names lane's answer
//! (after hprime-5): `address`/`b256`/`bytes`/`uint` (alloy's `fixed_bytes_macros!`) and
//! tokio's `select!` (its `doc!` wrapper) are `#[macro_export]` macros written by an
//! expansion; fixed in blaze-scope task/exports.rs `publish_expanded_exports`, uncommitted,
//! blaze-scope red on the solver's lookup.rs: rerun h' once its mixcheck is green; if `quote`
//! or `json` still miss, send that lane the one module. `vec`/`assert_eq` `NotRead` stays open
//! (the prelude's macro modules are not read for body lookups). Leftover: `scoped.rs`'s `NoSource` branch builds a
//! proc definition under the names' crate key, which the hook does not know (dead now that the
//! names declare `MacroBody::Proc`; delete it). Step 3 finding: h' reads only the files the
//! module walk reached with `tests: false` (600 of 816: tests/, benches/, examples/ and
//! `#[cfg(test)] mod x;` files are h's only). Not started: include_str!/include! (the host's
//! `include` needs the call's file: `BodyHost` gets the body's file path; the read joins
//! blaze-expand's `Reads` in the expansion), step 4 (panic edition through marks), step 5.
//!
//! State (2026-11-16, 04:35, relay runner after the one below): tempo h' (run output
//! /Volumes/Projects/blaze-int-1112-notes/port27/hprime-3.txt) "11720 bodies: 11173 clean
//! (95.3%)" (was 10857), "stopped only by macro calls: 478" (was 716), h' 1253.7 ms (was
//! 3179.7 ms). Done: (1) `panic!`: the library writes `rustc_builtin_macro(core_panic)` /
//! `(std_panic)`; blaze-surface `BuiltinFn::of_library` maps both to `Panic` (blaze-scope's
//! `definition_of` uses it); `panic_edition` is the body's crate edition (`names.edition`);
//! test `panic_picks_its_form_by_the_call_site_s_edition`; fmt.rs now reports
//! `DiagKind::FormatArgMissing` (a `{}` with no argument; BODY_KEY_DOMAIN bumped to 4). Gap:
//! a `panic!` path written by ANOTHER crate's macro takes the body's edition, not the
//! writing macro's (rustc's `use_panic_2021` walks marks through
//! `allow_internal_unstable(edition_panic)`: needs that flag recorded on `DefAt`). No other
//! builtin is refused; env!/include_str! fail only because stages' `BodyHost` refuses them.
//! (2) [`scoped::Prepares`]: each `macro_rules!`/`macro` prepared once by its defining
//! item's key (88 on tempo); `Scoped<'d, 'p>` takes `prepared: Option<&Prepares>` (the
//! ladder passes `None`: give it one per build). (3) body-local `macro_rules!`: the machine
//! keeps them in textual order with their block depth (`local_macros`), feed hands them to
//! each call as `BodyCall::locals`, `ByName` tries them first (`MacroNames::local_site` gives
//! the crate); `Scoped::local` is gone; test `textual_scope_shadows_path_based_scope`.
//! Next: step 4 (h and h' over the same bodies: h reads every .rs under members' src, 15045,
//! h' only files the module walk reached, 11720: cause unverified) and the 47 `NotRead`
//! (`vec`, `assert_eq` in std modules the names never read: ask the names lane why), then
//! step 5 (stages.rs:1102 panic on blaze's own checkout; `lower_std` census through Scoped).
//! New cause on tempo: 82 "refused: NoDefinition" (proc macros the names now declare, with no
//! runner for their crate: the proc-macro lane's).
//!
//! Earlier (2026-11-16, 04:15, cross-crate body macros): [`scoped::Scoped`] is the one
//! `MacroNames` over blaze-scope's `Names::macro_definition` (hygiene rule in its doc and
//! `scoped_tests.rs`, 7 tests green); `MacroNames::resolve` returns `Result<_, NoMacro>` and
//! `ByName::findings()` gives each unresolved body call typed with its range. blaze-real's
//! stage h' uses it (its `BodyNames` is gone). Tempo h' (`stages` with the store as 4th arg):
//! "11720 bodies: 10857 clean (92.6%)", "stopped only by macro calls: 716"; misses: 796
//! `path: NotFound` (proc macros: blaze-scope declares no `#[proc_macro*]` yet: `address`,
//! `quote`, `b256`), 294 `refused: BuiltinUnknown` (std's `panic!` builtin: blaze-expand's
//! to add), 47 `NotRead` (`vec`, `assert_eq`: std's modules). Open: prepare each definition
//! once by its key inside `Scoped` (today `expand` prepares per call); the proc path (NoSource
//! to `MacroDefinition::proc_macro` with the defining crate's key) is untested until
//! blaze-scope declares proc macros; body-local `macro_rules!` are not yet gathered into
//! `Scoped::local` by the driver; step 3 (lower_std census through Scoped) not run.
//! Reported by the proc-macro lane (a3c0fcccebe508508), not fixed: blaze-expand item.rs
//! `ItemAttrs::derives` drops a field or variant whose `#[cfg]` is false but keeps the outer
//! attributes written BEFORE that cfg (`#[command(flatten)] #[cfg(feature = "absent")] b: T`
//! leaves `# [command (flatten)]` in the derive's input; clap errors on tempo's
//! bin/tempo/src/cli.rs:57 `pyroscope_args`). rustc drops the field with all its attributes.
//!
//! Earlier (2026-11-16, late): item-position expansion's one home is blaze-expand's `item.rs`
//! (`ItemAttrs::next` walks outer attributes in rustc's `take_first_attr` order, `attr_call`,
//! `derives` with the input a derive sees; `Call::attr`, `Call::derive`). [`lower_file`]'s
//! driver calls it (its own derive scan and `derive_input` are gone; [`Expand::derive`] and
//! [`Expand::attr`] take token streams). The names library's fill (blaze-scope, agent
//! a9d6146dd9986b11a) was sent the exact calls for `task/lower.rs` `item()` and
//! `resolve.rs` `expand_decls`; open: its proof (bstd's `Placement`/`Reach` `Clone` impls in
//! the impl table, `bstd_hosted_start` from `#[bstd::main]`, derive counts across bstd).
//! `cfg!` is answered by [`cfg_outcome`] (lower_std's host uses it); `cfg_select!` takes
//! expression arms; anonymous constants expand ([`lower_anon_const_expanding`]). Census:
//! lower_std "unhandled forms: 218 in all" (927 before); tempo stage h "13009 clean (86.5%)",
//! 1906 bodies stopped by dependency macros. Next (piece 3): a [`by_name::MacroNames`] over
//! blaze-scope's `Names::macro_definition(module, shape, written, alloc)` per body module (it
//! exists; `resolve::definition_of` is the one home of a definition; proc defs come back as
//! `MacroDefError::NoSource`, to be built with `MacroDefinition::proc_macro` and krate
//! `blaze_proc_macro::hook::crate_key(module)`), names from blaze-world's graph, then tempo's
//! stage h again. Derives in the names fill are wired and tested there (`Placement`'s `Clone`
//! in `tables.impls`); attribute macros wait on a `MacroShapes` set in blaze-surface
//! (`macro_rules!` bindings are FnLike-only, so `#[dep::wrap]` is `WrongMacroKind`). Left in lower_std: `const_eval_select!` (30 failed),
//! `cfg_select!` in a type (32, `Kind`), `asm!` (29), destructuring assignment (67).
//! Harness side of piece 3 (agent a0ac97cbffdc16b38): blaze-real's `examples/stages.rs` stage h'
//! (`bodies_named`, resolver `BodyNames`), names made by `examples/names_load/mod.rs`; run as
//! `stages <checkout> ~/.cargo <library> <fetch store>`; written against `MacroNames` returning `Option`.
//!
//! Earlier state: body-position macro expansion is in ([`lower_body_expanding`], the
//! driver's bodies go through it; `feed.rs` keeps one stack of parses; `by_name.rs` is the
//! stand-in resolver). Item position too: [`lower_file`]'s driver runs builtin derives
//! (`Expand::derive`, blaze-expand derive.rs) and attribute macros (`Expand::attr`, a
//! `macro_rules!` `attr()` arm such as bstd's `#[bstd::main]`). An expansion is lowered from
//! the expander's words, never re-lexed text (text loses `$crate`): `ExpansionWords` in
//! driver.rs, the outline read on a copy with `$crate`'s `$` blanked, and a body-local item of
//! such an item carries its slice of the words. `format_args!` while expanding is read from
//! `builtin # format_args` (machine.rs `FORMAT_ARGS_EXPR`, fmt.rs `builtin_format_args`).
//! `#[cfg_attr(.., derive(..))]` is applied under the unit's cfg set, and a derive sees its
//! item as rustc hands it over (`Driver::derive_input`: derives gone, cfg applied on fields
//! and variants). Open: cross-crate body macros wait on blaze-scope's workspace adapter
//! (`resolve_crates` inputs from a cargo workspace on disk; then an expander over
//! `names::macro_path` + `Names::macro_src`, per body module); `$crate` stays a path segment
//! named `$crate` (its resolution is the resolver's); `line!()` and kin where written give
//! blaze-expand holes this crate does not fill yet; anonymous constants are lowered without
//! the expander; `cfg!` needs a host that evaluates a predicate; const args and qualified
//! paths (84) not started. Run `examplerun.sh blaze-lower lower_std <library>` after any change.
//!
//! Next, asked by the coordinator 2026-11-16 (rules 54, 76, 24), not started: a body
//! record's range becomes its own type, so no reader can cut the item's text with a range
//! that points into [`View::expanded`]. The design:
//! - `pub struct BodyRange { start: u32, end: u32, item_len: u32, expanded_len: u32 }`,
//!   fields private, no `From`/`Into` a `blaze_surface::Range`, no `of`/`get`/`Index` on
//!   text. Only [`View`] accessors make one (every `Range` in `form.rs`'s decoded rows and
//!   in `View::name_ranges`, `consts`, `unhandled`, `diags` changes type).
//! - `View::text_at(&self, item_text: &'s str, r: BodyRange) -> Result<&'s str, TextAt>`,
//!   total: `TextAt::OtherView` when `r`'s two lengths are not this view's, or
//!   `TextAt::ItemText` when `item_text.len()` is not the recorded length; never a panic.
//!   Inside the crate the machine keeps plain `Range` (it builds the text); the type is at
//!   the record's door only. A diagnostic needing a file position asks the view for
//!   `item_relative(r) -> Option<Range>`, `None` inside an expansion.
//! - Readers to port in one batch: this crate (`dump.rs`, `typeck.rs`, `driver.rs`,
//!   `lower_std.rs`, the tests), blaze-ladder `src/front.rs` (`.get(text)`/`.of(text)` at
//!   ~598, 615, 857, 1070, 1296, 1349, 1400, 1434, 1491: message its lane the signatures
//!   before saving, it is mid-edit), blaze-consteval `src/fixture.rs` (`Refused::Form`'s
//!   range), the checker lane's `typeck_tests.rs`. blaze-stencil and blaze-macro-eval name
//!   another `blaze_lower` API (`Lower`, `Ran`), not this crate's records: check first.
//! - The test: a body whose expansion is longer than its item, every range read through
//!   `text_at`, the bytes equal to the expected words; and a range from another view
//!   refused.
//!
//! Rust source text to signature records and body records, straight from the parser's events:
//! no syntax tree is built. One event-driven machine ([`machine`]) reads the parser's
//! steps once, keeps a stack of the nodes open and a stack of the values their children made,
//! and writes each node into its column when the node closes (post-order). Nothing recurses:
//! 100,000 nested parentheses or chained operators are 100,000 frames on a heap stack.
//!
//! # What is lowered
//!
//! - [`lower_signature`]: one item's text (blaze-file-items' range of it) to its signature:
//!   generics with bounds and defaults, where-clauses, parameters as patterns with types, the
//!   return type, qualifiers and ABI, `self` forms; fields with visibility, variants with
//!   discriminants (as anonymous constants), supertraits, the trait reference of an impl and
//!   its polarity, the self type; the associated items as entries (each lowered as an item of
//!   its own); attributes as (path, arguments) ranges, `cfg` and `cfg_attr` evaluated.
//! - [`lower_body`]: a fn's body or a const's or static's value, in post-order columns with
//!   locals, block scopes, labels and loop targets resolved here, and rustc's desugarings
//!   made ([`desugar`]): `for`, `while`, `?`, ranges, `.await` and `async fn`. `if let` chains
//!   stay an `if` over `let` expressions and `let else` a statement, as in rustc's HIR.
//! - [`lower_anon_const`]: an array length, const argument, discriminant or field default.
//!
//! A path that is not a local stays a path: [`resolve`] answers it through the names of the
//! world (blaze-scope's), returning `Need` for what is not known yet.
//!
//! # Keys (rule 21)
//!
//! - A signature record's key is [`signature_key`]: [`SIGNATURE_KEY_DOMAIN`] (a subject key: the
//!   lowering's code is the filer's `ProducerId`, D105), the edition, whether the item has a body, and the
//!   item's signature text (blaze-file-items' `signature` range for a fn, const or static; the
//!   whole item for the rest). Its read list: the `cfg` atoms it asked ([`View::cfg_reads`]).
//! - A body record's key is [`body_key`]: [`BODY_KEY_DOMAIN`], the edition and the item's
//!   whole text. Its read list: the `cfg` atoms and the lang items its desugarings name
//!   ([`View::langs`]), and, lowered by [`lower_body_expanding`], each macro call's range
//!   and the key of its expansion ([`View::expansion_calls`], [`View::expansion_keys`]):
//!   that key is blaze-expand's call key (the definition and the input), and what the
//!   expansion itself read (`env!`, an included file) is on the expansion's own read list.
//!
//! # Macro calls in bodies
//!
//! [`lower_body_expanding`] expands each call where its node closes, through the caller's
//! [`Expand`] ([`by_name::ByName`] finds macros by name, for tests and census runs), parses
//! the expansion as the fragment its position takes (`blaze_expand::Fragment`: an expression,
//! statements, a pattern, a type) and feeds that parse next, on one stack of parses
//! ([`EXPANSION_DEPTH`] deep). The expansion's words are appended to the item's text, so
//! every range stays one range: past the item's length it is of [`View::expanded`]
//! ([`View::text_at`]). Locals and labels are keyed by name and local hygiene
//! (blaze-expand's `body` module states the rule). `format_args!` and its kin stay one node
//! (`fmt`); `asm!` lowers to one [`Expr::Asm`] row ([`asm`]); `global_asm!` and `naked_asm!`
//! are refused by name ([`Why::Asm`]).
//! - An anonymous constant's is [`anon_const_key`]: the item's text and the constant's range.
//!
//! The item's `ItemKey` is where a record is filed, not part of its key: two items with equal
//! text have equal records. No range in a record is a file's: each is from the item's first
//! byte (rule 71), kept in side columns no key reads.
//!
//! # What blaze-typeck's form lacks
//!
//! [`typeck::to_typeck`] converts a body into blaze-typeck's `BodyView`, and refuses by name
//! what that form has no place for: `let` in a condition, a break with no target, lang-item
//! paths (every desugaring's), coroutines and `yield`, `become`, raw borrows, `const` and
//! `try` blocks, struct defaults `..`, box, range and rest patterns, a slice pattern's `..`,
//! `b'x'` and `c"..."` literals, format arguments, macro calls, path anchors and lifetime
//! arguments. Dropped in the conversion, because no type depends on them: match and loop
//! sources and `unsafe` on a block (diagnostics and the unsafety check only), the shadowing
//! links and the hygiene of names made by desugarings.

#![no_std]
#![feature(allocator_api)]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

pub mod asm;
pub mod block_uses;
pub mod by_name;
pub mod call_reads;
mod columns;
pub mod desugar;
mod driver;
pub mod dump;
mod feed;
mod fmt;
pub mod form;
mod lit;
pub mod machine;
pub mod mark_site;
#[cfg(test)]
mod plant;
pub mod resolve;
pub mod scoped;
pub mod scope_origin;
mod super_let;
mod type_text;
pub mod typeck;

#[cfg(test)]
mod call_reads_tests;
#[cfg(test)]
mod expand_tests;
#[cfg(test)]
mod outline_sig_tests;
#[cfg(test)]
mod scoped_tests;
#[cfg(test)]
mod sizing_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod type_text_tests;
#[cfg(test)]
mod typeck_tests;

use blaze_ids::{ContentId, Hasher};
use blaze_own::{AVec, Alloc};
pub use blaze_surface::{Edition, Range};

pub use crate::columns::{BodyRange, Columns, Head, LoweredRoot, Open, TextAt, View, head, open};
pub use crate::driver::{
    BodyCall, BodyOut, Bound, CallInput, EXPANSION_BUDGET, EXPANSION_DEPTH, Expand, Expansion, Expansions, FileStats, InputMarks, ItemRef, Outcome,
    Refusal, Sink, Source, What, lower_file,
};
pub use crate::form::*;
pub use crate::machine::Mode;
pub use crate::type_text::{TypeTextRefused, lower_type_text, lower_type_text_expanding};

/// The domain of [`signature_key`], a subject key: what is asked, never which code answers
/// (decision D105: a cache of lowered records mixes in the lowering's `ProducerId`). Its number
/// is the key's layout.
pub const SIGNATURE_KEY_DOMAIN: &str = "blaze v2 lower signature key 1";
/// The domain of [`body_key`], a subject key (see [`SIGNATURE_KEY_DOMAIN`]).
pub const BODY_KEY_DOMAIN: &str = "blaze v2 lower body key 1";
/// The domain of [`anon_const_key`], a subject key (see [`SIGNATURE_KEY_DOMAIN`]).
pub const ANON_CONST_KEY_DOMAIN: &str = "blaze v2 lower anon const key 1";

/// What one name is in the value namespace at a body's module, as an identifier pattern sees
/// it (the Reference, "Identifier patterns": "Path patterns take precedence over identifier
/// patterns"; a binding may not shadow a static, a tuple struct or a tuple variant).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ValueName {
    /// Nothing a pattern cares about (no item, a fn, a local): the name binds.
    Nothing = 0,
    Const = 1,
    UnitStruct = 2,
    UnitVariant = 3,
    Static = 4,
    /// A tuple struct's or a tuple variant's constructor.
    TupleCtor = 5,
    /// Not known yet: the lowering finishes its pass and returns [`Need`] with the name's range.
    Need = 6,
}

impl ValueName {
    /// A path pattern: the name is matched against, never bound.
    pub fn is_path(self) -> bool {
        matches!(self, ValueName::Const | ValueName::UnitStruct | ValueName::UnitVariant)
    }

    pub(crate) fn of_code(code: u16) -> Option<ValueName> {
        [
            ValueName::Nothing,
            ValueName::Const,
            ValueName::UnitStruct,
            ValueName::UnitVariant,
            ValueName::Static,
            ValueName::TupleCtor,
            ValueName::Need,
        ]
        .get(code as usize)
        .copied()
    }
}

/// The one question a body's lowering asks of the world: what `name`, one identifier, names in
/// the value namespace at the body's module, resolved exactly as a one-segment value path
/// written there (its `use`s, globs and the prelude included). Items declared in the body's own
/// blocks are the lowering's, asked before this. Every answer is in the record's read list
/// ([`View::value_reads`]).
pub trait ValueNames {
    fn value_name(&mut self, name: &str) -> ValueName;

    /// What `name`, an identifier pattern, is through the `use` declarations of the blocks
    /// around it (`uses`: innermost block first; `locals`: the names of those blocks' own
    /// items, which a `use` path may start with): `Some` when one of them binds the name in the
    /// value namespace (a named import, or a glob import whose module or enum holds it:
    /// `use E::*;` makes a pattern `A` the variant), `None` when none does, and
    /// `Some(ValueName::Need)` when the world cannot say yet. Asked before the block's item
    /// and the module ([`block_uses`], where the order's rule is quoted). The default is a
    /// world with no modules to import from (a fixture's): no `use` binds anything.
    fn value_name_by_uses(&mut self, view: &View<'_>, name: &str, uses: &[block_uses::BlockUse<'_>], locals: &[&str]) -> Option<ValueName> {
        let _ = (view, name, uses, locals);
        None
    }
}

/// A test fixture's world, with no module around the body: every name is [`ValueName::Nothing`].
/// A real caller answers from its module.
pub struct NoModule;

impl ValueNames for NoModule {
    fn value_name(&mut self, _: &str) -> ValueName {
        ValueName::Nothing
    }
}

/// Whether every answer `view`'s lowering read of `names` still holds: when one does not (a
/// name it bound is now a constant), the body is lowered again.
pub fn value_reads_hold(view: &View<'_>, names: &mut dyn ValueNames) -> Result<bool, TextAt> {
    // A read through a block's `use` is asked again with the block's `use`s, which takes an
    // allocator: [`block_uses::reads_hold`] asks both kinds. Here such a record does not hold.
    if view.value_reads.iter().any(|n| n.flags & block_uses::BY_USES != 0) {
        return Ok(false);
    }
    for read in view.value_reads() {
        let (name, answer) = read.map_err(TextAt::Row)?;
        if names.value_name(view.name_text(name)?) != answer {
            return Ok(false);
        }
    }
    Ok(true)
}

/// What a `cfg` atom is in the build: blaze-surface's cfg set, asked one atom at a time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CfgAnswer {
    Yes,
    No,
    /// Not known yet: the lowering finishes its pass and returns [`Need`].
    Need,
}

/// The build's cfg set. `name` is the atom's name, `value` its string's contents (quotes
/// off, as written: an escape stays an escape).
pub trait Cfg {
    fn atom(&mut self, name: &str, value: Option<&str>) -> CfgAnswer;
}

/// The outcome of a `cfg!` predicate (an expander host's `ExpandCx::cfg`) under `cfg`, the
/// cfg set the body is lowered under: the one home of that answer for every host. An atom
/// not known yet, or tokens that are no predicate, are refused by name, never guessed.
pub fn cfg_outcome(predicate: &blaze_expand::TokenStream<'_>, cfg: &mut dyn Cfg, alloc: Alloc<'_>) -> Result<bool, blaze_expand::HostError> {
    let refused = blaze_expand::HostError { ask: blaze_expand::HostAsk::Cfg, why: blaze_expand::HostWhy::Refused };
    let mut atom = |name: &str, value: Option<&str>| match cfg.atom(name, value) {
        CfgAnswer::Yes => Ok(true),
        CfgAnswer::No => Ok(false),
        CfgAnswer::Need => Err(()),
    };
    blaze_expand::cfg_predicate(predicate, alloc, &mut atom).map_err(|_| refused)
}

/// A lowering that met `cfg` atoms, or identifier patterns' names ([`ValueName::Need`]), not
/// known yet: their ranges, item-relative. Nothing else of the pass is kept.
#[derive(Debug)]
pub struct Need<'a> {
    pub atoms: AVec<'a, Range>,
}

/// A finished lowering: its columns and the item text it was lowered from, read through
/// [`Lowered::view`] or written as a record.
pub struct Lowered<'a, 't> {
    pub columns: Columns<'a>,
    pub(crate) item: &'t str,
}

impl<'a, 't> Lowered<'a, 't> {
    /// The view, with the item's text it was lowered from: no other text can be given it.
    pub fn view(&self) -> View<'_> {
        self.columns.view(self.item)
    }

    /// The record: one blaze-format buffer, its root a [`LoweredRoot`].
    pub fn to_record<'b>(&self, alloc: Alloc<'b>) -> blaze_own::Vec<u8, Alloc<'b>> {
        self.columns.to_record(alloc)
    }
}

const fn edition_byte(edition: Edition) -> u8 {
    match edition {
        Edition::E2015 => 0,
        Edition::E2018 => 1,
        Edition::E2021 => 2,
        Edition::E2024 => 3,
    }
}

pub(crate) const fn parser_edition(edition: Edition) -> parser::Edition {
    match edition {
        Edition::E2015 => parser::Edition::Edition2015,
        Edition::E2018 => parser::Edition::Edition2018,
        Edition::E2021 => parser::Edition::Edition2021,
        Edition::E2024 => parser::Edition::Edition2024,
    }
}

/// The key of an item's signature record: its signature text (`key_text`), the edition, and
/// whether a body follows the text (`fn f();` and `fn f() {}` share their text before it).
pub fn signature_key(key_text: &str, edition: Edition, has_body: bool) -> ContentId {
    Hasher::new(SIGNATURE_KEY_DOMAIN)
        .part(&[edition_byte(edition), has_body as u8])
        .part(key_text.as_bytes())
        .finish()
}

/// The key of an item's body record: its whole text and the edition.
pub fn body_key(item_text: &str, edition: Edition) -> ContentId {
    Hasher::new(BODY_KEY_DOMAIN).part(&[edition_byte(edition)]).part(item_text.as_bytes()).finish()
}

/// The key of an anonymous constant's body record: its text and where it starts in the item
/// (its record's ranges are the item's), and the edition.
pub fn anon_const_key(item_text: &str, range: Range, edition: Edition) -> ContentId {
    Hasher::new(ANON_CONST_KEY_DOMAIN)
        .part(&[edition_byte(edition)])
        .part(&range.start.to_le_bytes())
        .part(range.of(item_text).as_bytes())
        .finish()
}

/// Lowers one item's signature from its text (from its first attribute to its last token).
/// The text is parsed as an outline: a fn's body is never parsed, and the body of the item's
/// `fn`, or of an `impl`'s or a `trait`'s, is never lexed either ([`parser::LexedStr::new_outline`]:
/// one token for the whole body). The parser's words and each word's range are made in one
/// pass over the tokens, so the feed takes each step's range from that column
/// ([`feed::feed_input`]).
///
/// # Panics
///
/// When `text` is longer than `u32::MAX` bytes.
pub fn lower_signature<'a, 't>(
    text: &'t str,
    edition: Edition,
    cfg: &mut dyn Cfg,
    alloc: Alloc<'a>,
) -> Result<Lowered<'a, 't>, Need<'a>> {
    // A text that spells `$crate` is read from its words, `$crate` one of them ([`spelled_words`]).
    if let Some(w) = spelled_words(text, edition, alloc) {
        return lower_signature_words(text, w.item_words(), edition, cfg, alloc);
    }
    let end = u32::try_from(text.len()).expect("an item of over 4 GiB: ranges are u32 pairs");
    let pe = parser_edition(edition);
    let lexed = parser::LexedStr::new_outline(pe, text);
    let (words, ranges) = input_columns(&lexed, pe, alloc);
    let output = parser::Reparser::top_level_item().parse_outline(parser::Tokens::new(&words, true));
    let mut m = machine::Machine::new(text, Mode::Signature, edition, cfg, None, alloc);
    feed::feed_input(&mut m, &output, ranges, end);
    m.finish()
}

/// The parser's words of `lexed` (its tokens that are not trivia, then [`parser::Tokens::PAD`]
/// `EOF` words: [`parser::LexedStr::to_input`]'s) and each word's range in its text, in one
/// pass ([`parser::LexedStr::each_input_token`]).
fn input_columns<'a>(lexed: &parser::LexedStr<'_>, pe: parser::Edition, alloc: Alloc<'a>) -> (AVec<'a, u32>, AVec<'a, Range>) {
    let mut words: AVec<'a, u32> = AVec::with_capacity_in(lexed.len() + parser::Tokens::PAD, alloc);
    let mut ranges: AVec<'a, Range> = AVec::with_capacity_in(lexed.len(), alloc);
    lexed.each_input_token(pe, &mut parser::Joiner::new(), |word, bytes, joins| {
        if joins && let Some(last) = words.last_mut() {
            *last |= parser::JOINT;
        }
        words.push(word);
        // The text is under 4 GiB ([`lower_signature`]).
        ranges.push(Range::new(bytes.start as u32, bytes.end as u32));
    });
    words.extend((0..parser::Tokens::PAD).map(|_| parser::Tokens::EOF_WORD));
    (words, ranges)
}

/// Lowers one item's signature with the given Blaze syntax [`parser::Extensions`] enabled
/// (e.g. `Extensions::UNIQ.with(Extensions::RECORDS)` for `#![blaze::uniq]` and `#![blaze::records]`).
pub fn lower_signature_with_extensions<'a, 't>(
    text: &'t str,
    edition: Edition,
    extensions: parser::Extensions,
    cfg: &mut dyn Cfg,
    alloc: Alloc<'a>,
) -> Result<Lowered<'a, 't>, Need<'a>> {
    if extensions.is_none() {
        return lower_signature(text, edition, cfg, alloc);
    }
    let pe = parser_edition(edition);
    let lexed = parser::LexedStr::new(pe, text);
    let mut input = lexed.to_input(pe);
    input.mark_extensions(0, extensions);
    let output = parser::Reparser::top_level_item().parse_outline(input.tokens());
    let mut m = machine::Machine::new(text, Mode::Signature, edition, cfg, None, alloc);
    feed::feed(&mut m, &lexed, &output, 0);
    m.finish()
}

/// Lowers one item's body with the given Blaze syntax [`parser::Extensions`] enabled
/// (e.g. `Extensions::UNIQ.with(Extensions::RECORDS)`).
pub fn lower_body_with_extensions<'a, 't>(
    text: &'t str,
    edition: Edition,
    extensions: parser::Extensions,
    cfg: &mut dyn Cfg,
    values: &mut dyn ValueNames,
    alloc: Alloc<'a>,
) -> Result<Lowered<'a, 't>, Need<'a>> {
    if extensions.is_none() {
        return lower_body(text, edition, cfg, values, alloc);
    }
    let pe = parser_edition(edition);
    let lexed = parser::LexedStr::new(pe, text);
    let mut input = lexed.to_input(pe);
    input.mark_extensions(0, extensions);
    let output = parser::Reparser::top_level_item().parse(input.tokens());
    let mut m = machine::Machine::new(text, Mode::Body, edition, cfg, Some(values), alloc);
    feed::feed(&mut m, &lexed, &output, 0);
    m.finish()
}

/// [`lower_signature`] as it was: the whole text lexed ([`parser::LexedStr::new`]), every body
/// token in the input, and the feed walking the lexer's tokens. The reference the outline
/// lexer's lowering is compared with, byte for byte.
#[cfg(test)]
pub(crate) fn lower_signature_whole_lex<'a, 't>(
    text: &'t str,
    edition: Edition,
    cfg: &mut dyn Cfg,
    alloc: Alloc<'a>,
) -> Result<Lowered<'a, 't>, Need<'a>> {
    if let Some(w) = spelled_words(text, edition, alloc) {
        return lower_signature_words(text, w.item_words(), edition, cfg, alloc);
    }
    let pe = parser_edition(edition);
    let lexed = parser::LexedStr::new(pe, text);
    let input = lexed.to_input(pe);
    let output = parser::Reparser::top_level_item().parse_outline(input.tokens());
    let mut m = machine::Machine::new(text, Mode::Signature, edition, cfg, None, alloc);
    feed::feed(&mut m, &lexed, &output, 0);
    m.finish()
}

/// An item text's parser words when the text SPELLS `$crate` ([`spelled_words`]): the words, then
/// [`parser::Tokens::PAD`] `EOF` words, and each word's range in the text.
struct SpelledWords<'a> {
    words: AVec<'a, u32>,
    ranges: AVec<'a, Range>,
}

impl SpelledWords<'_> {
    fn item_words(&self) -> ItemWords<'_> {
        ItemWords { words: &self.words, ranges: &self.ranges }
    }
}

/// The parser's words of `text` when it spells `$crate`, `$crate` ONE word (a path root, as the
/// expander writes it: [`ItemWords`]), each word's range in `text`; `None` when `text` spells no
/// `$crate` (the parser's lexer reads it as written, the common case).
///
/// Who writes such a text: a body's lowering copies an item a macro call in the body wrote out
/// of the expansion's words (blaze-ladder front2 locals.rs `keep`, `frame.alloc_str(view.text_at
/// (..))`), and those words spell `$crate` (blaze-expand `body_words`; a module-level rendering
/// writes `crate` with a root mark instead). Lexed again by the parser's lexer, `$crate` is `$`
/// and `crate`, and a path `$crate::x` is no path: std's `thread_local_inner!` writes, inside a
/// closure of a const block of `OUTPUT_CAPTURE`'s initializer, `#[thread_local] static
/// __RUST_STD_INTERNAL_VAL: $crate::thread::local_impl::EagerStorage<..> = $crate::..::new(..);`,
/// whose `$` stopped the type, so the `=` was in an error node, not the static's own: the static
/// had "no initializer" (`sig::HAS_BODY` unset) and std's hello world stopped at "a fn or static
/// declared in a body with no body" (g204119; the copy is 219 bytes, the stop's range).
///
/// A `$` right before `crate` (no byte between) is folded into one `$crate` identifier token,
/// then the tokens are made words as an expansion's are (blaze-expand `parser_words`). A text
/// blaze-expand's lexer refuses (an unterminated literal) is `None` too: the parser's lexer then
/// reads it, and its error tokens are the parse's, exactly as for any text that does not lex.
fn spelled_words<'a>(text: &str, edition: Edition, alloc: Alloc<'a>) -> Option<SpelledWords<'a>> {
    use blaze_expand::{Raw, TokenKind, TokenStream};
    if !text.contains("$crate") {
        return None;
    }
    let lexed = blaze_expand::lex(text, edition, alloc).ok()?;
    let places = blaze_expand::origin_column::input_places(text, blaze_expand::origin_column::InputShape::Whole, edition, alloc).ok()?;
    let tokens = lexed.tokens();
    if places.len() != tokens.len() {
        return None;
    }
    let mut folded = TokenStream::new_in(alloc);
    // Each folded token's bytes in `text`.
    let mut spans: AVec<'a, (u32, u32)> = AVec::with_capacity_in(tokens.len(), alloc);
    let mut i = 0;
    while i < tokens.len() {
        let t = tokens[i];
        if let TokenKind::Punct { char: '$', .. } = t.kind
            && let Some(next) = tokens.get(i + 1)
            && let TokenKind::Ident { raw: Raw::No, spacing } = next.kind
            && lexed.text_of(next) == "crate"
            && places[i].1 == places[i + 1].0
        {
            folded.push(TokenKind::Ident { raw: Raw::No, spacing }, "$crate", t.ctx).ok()?;
            spans.push((places[i].0, places[i + 1].1));
            i += 2;
            continue;
        }
        folded.push(t.kind, lexed.text_of(&t), t.ctx).ok()?;
        spans.push(places[i]);
        i += 1;
    }
    let pw = blaze_expand::parser_words(&folded, alloc);
    let n = pw.words.len().saturating_sub(parser::Tokens::PAD);
    // Word `w`'s range: from the first token that leads to it (a lifetime's `'` makes no word of
    // its own: its name's word starts at the `'`) to the end of the token that made it.
    let mut ranges: AVec<'a, Range> = AVec::with_capacity_in(n, alloc);
    let mut pending: Option<u32> = None;
    for (t, &(start, end)) in spans.iter().enumerate() {
        let (at, next) = (pw.word_at[t], pw.word_at[t + 1]);
        if next == at + 1 {
            ranges.push(Range::new(pending.take().unwrap_or(start), end));
        } else if next == at {
            pending = pending.or(Some(start));
        } else {
            // A token of more than one word: none of a lexed text's tokens is (an invisible
            // group's, a fragment's are an expansion's only).
            return None;
        }
    }
    if ranges.len() != n {
        return None;
    }
    Some(SpelledWords { words: pw.words, ranges })
}

/// Lowers one item's body: a fn's block, or a const's or static's value. An item with none
/// (a trait method without a default) lowers to a record whose head says so.
///
/// # Panics
///
/// When `text` is longer than `u32::MAX` bytes.
pub fn lower_body<'a, 't>(
    text: &'t str,
    edition: Edition,
    cfg: &mut dyn Cfg,
    values: &mut dyn ValueNames,
    alloc: Alloc<'a>,
) -> Result<Lowered<'a, 't>, Need<'a>> {
    // A text that spells `$crate`: from its words ([`spelled_words`]).
    if let Some(w) = spelled_words(text, edition, alloc) {
        let output = parser::Reparser::top_level_item().parse(parser::Tokens::new(&w.words, false));
        let mut m = machine::Machine::new(text, Mode::Body, edition, cfg, Some(values), alloc);
        feed::feed_words(&mut m, &output, &w.ranges, None);
        return m.finish();
    }
    let pe = parser_edition(edition);
    let lexed = parser::LexedStr::new(pe, text);
    let input = lexed.to_input(pe);
    let output = parser::Reparser::top_level_item().parse(input.tokens());
    let mut m = machine::Machine::new(text, Mode::Body, edition, cfg, Some(values), alloc);
    feed::feed(&mut m, &lexed, &output, 0);
    m.finish()
}

/// As [`lower_body`], with each macro call in the body expanded through `expand` and its
/// expansion lowered in place: the record's ranges past the item's text are of
/// [`View::expanded`], and its read list holds each call's range and expansion key
/// ([`View::expansion_calls`], [`View::expansion_keys`]).
pub fn lower_body_expanding<'a, 't>(
    text: &'t str,
    edition: Edition,
    cfg: &mut dyn Cfg,
    values: &mut dyn ValueNames,
    expand: &mut dyn Expand,
    alloc: Alloc<'a>,
) -> Result<Lowered<'a, 't>, Need<'a>> {
    // A text that spells `$crate`: from its words ([`spelled_words`]).
    if let Some(w) = spelled_words(text, edition, alloc) {
        return lower_body_words_expanding(text, w.item_words(), edition, cfg, values, expand, alloc);
    }
    let pe = parser_edition(edition);
    let lexed = parser::LexedStr::new(pe, text);
    let input = lexed.to_input(pe);
    let output = parser::Reparser::top_level_item().parse(input.tokens());
    let mut m = machine::Machine::new(text, Mode::Body, edition, cfg, Some(values), alloc);
    expand.begin_body();
    feed::feed_expanding(&mut m, &lexed, &output, 0, expand);
    m.finish()
}

/// [`lower_body_expanding`] of an item a macro wrote, its text `marks.text()`: each of its own
/// words keeps its mark beside the text ([`View::mark_of_path`]), so a path of it, and a macro
/// call's name ([`BodyCall::path_mark`]), resolves where its mark says (`$crate`, written
/// `crate` in an item text, as the crate its mark names; drive.md, "A token keeps its mark").
pub fn lower_body_expanding_marked<'a, 't>(
    marks: mark_site::ItemMarks<'t>,
    edition: Edition,
    cfg: &mut dyn Cfg,
    values: &mut dyn ValueNames,
    expand: &mut dyn Expand,
    alloc: Alloc<'a>,
) -> Result<Lowered<'a, 't>, Need<'a>> {
    let text = marks.text();
    // A copy of a body expansion's item spells `$crate` (blaze-ladder front2 locals.rs): read
    // from its words, `$crate` one of them, its mark at the `$` ([`spelled_words`];
    // `mark_site::column_of_copy` starts the `$crate` row there).
    if let Some(w) = spelled_words(text, edition, alloc) {
        let output = parser::Reparser::top_level_item().parse(parser::Tokens::new(&w.words, false));
        let mut m = machine::Machine::new(text, Mode::Body, edition, cfg, Some(values), alloc);
        m.mark_item(marks);
        expand.begin_body();
        feed::feed_words(&mut m, &output, &w.ranges, Some(expand));
        return m.finish();
    }
    let pe = parser_edition(edition);
    let lexed = parser::LexedStr::new(pe, text);
    let input = lexed.to_input(pe);
    let output = parser::Reparser::top_level_item().parse(input.tokens());
    let mut m = machine::Machine::new(text, Mode::Body, edition, cfg, Some(values), alloc);
    m.mark_item(marks);
    expand.begin_body();
    feed::feed_expanding(&mut m, &lexed, &output, 0, expand);
    m.finish()
}

/// As [`lower_body_expanding`], with explicit Blaze syntax [`parser::Extensions`] enabled.
pub fn lower_body_expanding_with_extensions<'a, 't>(
    text: &'t str,
    edition: Edition,
    extensions: parser::Extensions,
    cfg: &mut dyn Cfg,
    values: &mut dyn ValueNames,
    expand: &mut dyn Expand,
    alloc: Alloc<'a>,
) -> Result<Lowered<'a, 't>, Need<'a>> {
    if extensions.is_none() {
        return lower_body_expanding(text, edition, cfg, values, expand, alloc);
    }
    if let Some(w) = spelled_words(text, edition, alloc) {
        return lower_body_words_expanding(text, w.item_words(), edition, cfg, values, expand, alloc);
    }
    let pe = parser_edition(edition);
    let lexed = parser::LexedStr::new(pe, text);
    let mut input = lexed.to_input(pe);
    input.mark_extensions(0, extensions);
    let output = parser::Reparser::top_level_item().parse(input.tokens());
    let mut m = machine::Machine::new(text, Mode::Body, edition, cfg, Some(values), alloc);
    expand.begin_body();
    feed::feed_expanding(&mut m, &lexed, &output, 0, expand);
    m.finish()
}

/// As [`lower_body_expanding_marked`], with explicit Blaze syntax [`parser::Extensions`] enabled.
pub fn lower_body_expanding_marked_with_extensions<'a, 't>(
    marks: mark_site::ItemMarks<'t>,
    edition: Edition,
    extensions: parser::Extensions,
    cfg: &mut dyn Cfg,
    values: &mut dyn ValueNames,
    expand: &mut dyn Expand,
    alloc: Alloc<'a>,
) -> Result<Lowered<'a, 't>, Need<'a>> {
    if extensions.is_none() {
        return lower_body_expanding_marked(marks, edition, cfg, values, expand, alloc);
    }
    let text = marks.text();
    if let Some(w) = spelled_words(text, edition, alloc) {
        let output = parser::Reparser::top_level_item().parse(parser::Tokens::new(&w.words, false));
        let mut m = machine::Machine::new(text, Mode::Body, edition, cfg, Some(values), alloc);
        m.mark_item(marks);
        expand.begin_body();
        feed::feed_words(&mut m, &output, &w.ranges, Some(expand));
        return m.finish();
    }
    let pe = parser_edition(edition);
    let lexed = parser::LexedStr::new(pe, text);
    let mut input = lexed.to_input(pe);
    input.mark_extensions(0, extensions);
    let output = parser::Reparser::top_level_item().parse(input.tokens());
    let mut m = machine::Machine::new(text, Mode::Body, edition, cfg, Some(values), alloc);
    m.mark_item(marks);
    expand.begin_body();
    feed::feed_expanding(&mut m, &lexed, &output, 0, expand);
    m.finish()
}

/// An item written by an item macro (a `macro_rules!` call at item position, a derive), as
/// the expander wrote it: the parser's `words` (ending in [`parser::Tokens::PAD`] `EOF`
/// words) and each word's range in `text`, the item's text. `$crate` is one word here, a path
/// root; lexed again from text it would be `$` and `crate`.
#[derive(Clone, Copy)]
pub struct ItemWords<'w> {
    pub words: &'w [u32],
    pub ranges: &'w [Range],
}

/// [`lower_signature`] of an item written by a macro, read from its words.
pub fn lower_signature_words<'a, 't>(
    text: &'t str,
    words: ItemWords<'_>,
    edition: Edition,
    cfg: &mut dyn Cfg,
    alloc: Alloc<'a>,
) -> Result<Lowered<'a, 't>, Need<'a>> {
    let output = parser::Reparser::top_level_item().parse_outline(parser::Tokens::new(words.words, false));
    let mut m = machine::Machine::new(text, Mode::Signature, edition, cfg, None, alloc);
    feed::feed_words(&mut m, &output, words.ranges, None);
    m.finish()
}

/// [`lower_body_expanding`] of an item written by a macro, read from its words.
pub fn lower_body_words_expanding<'a, 't>(
    text: &'t str,
    words: ItemWords<'_>,
    edition: Edition,
    cfg: &mut dyn Cfg,
    values: &mut dyn ValueNames,
    expand: &mut dyn Expand,
    alloc: Alloc<'a>,
) -> Result<Lowered<'a, 't>, Need<'a>> {
    let output = parser::Reparser::top_level_item().parse(parser::Tokens::new(words.words, false));
    let mut m = machine::Machine::new(text, Mode::Body, edition, cfg, Some(values), alloc);
    expand.begin_body();
    feed::feed_words(&mut m, &output, words.ranges, Some(expand));
    m.finish()
}

/// Lowers the anonymous constant at `range` of the item `text` (a [`ConstIdx`]'s range) as a
/// body of its own, its ranges still the item's.
pub fn lower_anon_const<'a, 't>(
    text: &'t str,
    range: Range,
    edition: Edition,
    cfg: &mut dyn Cfg,
    values: &mut dyn ValueNames,
    alloc: Alloc<'a>,
) -> Result<Lowered<'a, 't>, Need<'a>> {
    let pe = parser_edition(edition);
    let sub = range.of(text);
    let lexed = parser::LexedStr::new(pe, sub);
    let input = lexed.to_input(pe);
    let output = parser::TopEntryPoint::Expr.parse(input.tokens());
    let mut m = machine::Machine::new(text, Mode::AnonConst, edition, cfg, Some(values), alloc);
    feed::feed(&mut m, &lexed, &output, range.start);
    m.finish()
}

/// [`lower_anon_const`] with its macro calls expanded through `expand`, as
/// [`lower_body_expanding`] expands a body's (`[u8; size_of!(T)]`, a discriminant `concat!`).
pub fn lower_anon_const_expanding<'a, 't>(
    text: &'t str,
    range: Range,
    edition: Edition,
    cfg: &mut dyn Cfg,
    values: &mut dyn ValueNames,
    expand: &mut dyn Expand,
    alloc: Alloc<'a>,
) -> Result<Lowered<'a, 't>, Need<'a>> {
    let pe = parser_edition(edition);
    let lexed = parser::LexedStr::new(pe, range.of(text));
    let input = lexed.to_input(pe);
    let output = parser::TopEntryPoint::Expr.parse(input.tokens());
    let mut m = machine::Machine::new(text, Mode::AnonConst, edition, cfg, Some(values), alloc);
    expand.begin_body();
    feed::feed_expanding(&mut m, &lexed, &output, range.start, expand);
    m.finish()
}
